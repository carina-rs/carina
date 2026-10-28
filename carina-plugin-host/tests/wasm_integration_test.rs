//! Integration tests: load MockProvider .wasm via WasmProviderFactory and perform CRUD.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use carina_core::effect::PlanOp;
use carina_core::executor::complete_create_for_writeback;
use carina_core::provider::{
    AppliedIdentifier, BoxFuture, CreateFailure, CreateFailureKind, CreateOutcome,
    CreateOutcomeKind, CreateRequest, DeleteRequest, InFlightCreate, Observation, Provider,
    ProviderError, ProviderFactory, ProviderReadyConfig, RawSavedAttrs, ReadRequest,
    ResumeCreateRequest, ResumeFailure, ResumeFailureKind, ResumeOutcome, ResumeOutcomeKind,
    SavedAttrs, Sent, SentRecord, UpdateRequest, hydrate_provider_states,
    normalize_provider_states,
};
use carina_core::resource::{
    ConcreteValue, DataSource, DeferredValue, EnumValueResolver, RawEnumIdentifier,
    RedactedSecretHash, ResolvedResource, Resource, ResourceId, State, UnknownReason, Value,
};
use carina_core::schema::{AttributeType, TypeIdentity};
use carina_core::value::PersistedValue;
use carina_plugin_host::{GuestCallBudget, WasmProviderFactory};

fn mock_identifier() -> AppliedIdentifier {
    AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty")
}

fn create_state_for_writeback(outcome: CreateOutcome) -> State {
    complete_create_for_writeback(outcome).0
}

fn lifted_saved_attrs(
    entries: impl IntoIterator<Item = (ResourceId, HashMap<String, Value>)>,
) -> carina_core::provider::LiftedSavedAttrs {
    RawSavedAttrs::from_decoded_values_for_test(entries.into_iter().collect())
        .lift(&carina_core::schema::SchemaRegistry::new())
}

fn migrate_legacy_state_attribute(
    value: serde_json::Value,
    display_type: &str,
    identity: &str,
    key: &str,
) -> serde_json::Value {
    let mut attributes = serde_json::Map::new();
    attributes.insert(key.to_string(), value);
    let mut document = serde_json::json!({
        "version": 13,
        "resources": [{
            "provider": "",
            "resource_type": display_type,
            "identity": identity,
            "attributes": attributes,
            "deposed": [],
        }],
    });
    carina_core::resource::legacy_secret_migration::migrate_state_document_v13(&mut document)
        .expect("migrate legacy test state document");
    document["resources"][0]["attributes"]
        .as_object_mut()
        .and_then(|attributes| attributes.remove(key))
        .expect("migrated test document must retain its attribute")
}

fn redacted_secret_and_legacy_marker(plaintext: &str) -> (Value, String) {
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String(plaintext.into()),
    ))));
    let persisted = PersistedValue::encode_provider_value(&secret, None)
        .expect("test secret must be persistable");
    let encoded = persisted.as_json();
    let digest = encoded
        .as_object()
        .and_then(|tag| tag.get(carina_core::value::PERSISTED_SECRET_KEY))
        .and_then(serde_json::Value::as_str)
        .expect("structural test secret tag")
        .to_string();
    let hash: RedactedSecretHash =
        serde_json::from_value(serde_json::json!(digest)).expect("deserialize generated digest");
    (
        Value::Deferred(DeferredValue::RedactedSecret { hash }),
        format!("_secret:{digest}"),
    )
}

async fn create_request_for_test(resource: Resource) -> CreateRequest {
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let module_gate = carina_core::executor::ModuleConstraintGate::new(&[]);
    let schemas = carina_core::schema::SchemaRegistry::new();
    let preparation = carina_core::executor::ProviderPreparationContext::new(
        &bindings,
        &module_gate,
        &[],
        &carina_core::provider::NoopNormalizer,
        &[],
        &schemas,
    );
    carina_core::executor::prepare_create_request(resource, &preparation)
        .await
        .expect("test resource should pass checked create preparation")
}

trait DispatchCreateForTest {
    fn create_for_test<'a>(
        &'a self,
        id: &'a ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'a, Result<CreateOutcome, CreateFailure>>;
}

impl<T> DispatchCreateForTest for T
where
    T: Provider + ?Sized,
{
    fn create_for_test<'a>(
        &'a self,
        id: &'a ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'a, Result<CreateOutcome, CreateFailure>> {
        Box::pin(carina_core::executor::dispatch_create(self, id, request))
    }
}

async fn update_request_for_test(
    resource: Resource,
    from: State,
    changed_attributes: &[String],
) -> UpdateRequest {
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let module_gate = carina_core::executor::ModuleConstraintGate::new(&[]);
    let mut schemas = carina_core::schema::SchemaRegistry::new();
    schemas.insert(
        resource.id.provider.clone(),
        carina_core::schema::ResourceSchema::new(resource.id.resource_type.clone()).attribute(
            carina_core::schema::AttributeSchema::new(
                "reassert_token",
                carina_core::schema::AttributeType::string(),
            )
            .write_only(),
        ),
    );
    let preparation = carina_core::executor::ProviderPreparationContext::new(
        &bindings,
        &module_gate,
        &[],
        &carina_core::provider::NoopNormalizer,
        &[],
        &schemas,
    );
    carina_core::executor::prepare_update_request(resource, from, changed_attributes, &preparation)
        .await
        .expect("test resource should pass checked update preparation")
}

fn wasm_path() -> Option<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    // Cargo uses hyphens in binary names but underscores in library names; check both.
    for name in &["carina_provider_mock.wasm", "carina-provider-mock.wasm"] {
        let path = workspace_root.join("target/wasm32-wasip2/debug").join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

macro_rules! skip_if_no_wasm {
    () => {
        match wasm_path() {
            Some(p) => p,
            None => {
                eprintln!(
                    "SKIP: WASM binary not found. Build with: \
                     cargo build -p carina-provider-mock --target wasm32-wasip2"
                );
                return;
            }
        }
    };
}

/// Build a `WasmProviderFactory` using a per-test temporary cache directory.
///
/// Tests in this binary run in parallel; if they all shared
/// `WasmProviderFactory::from_bytes()`'s default `~/.carina/cache`, concurrent
/// precompile runs race on the same `.cwasm` path and one test can observe
/// a partially-written file (`"failed to load code for …"`). Each test gets
/// its own cache dir via this helper to eliminate that race.
async fn load_factory(wasm: &std::path::Path) -> (WasmProviderFactory, tempfile::TempDir) {
    let cache = tempfile::tempdir().expect("Failed to create cache tempdir");
    let wasm_bytes = std::fs::read(wasm).expect("Failed to read WASM provider");
    let factory = WasmProviderFactory::from_bytes_with_cache_dir(&wasm_bytes, cache.path())
        .await
        .expect("Failed to load WASM provider");
    (factory, cache)
}

fn resource_with_int_attribute(
    resource_type: &str,
    identity: &str,
    attribute: &str,
    value: i64,
) -> Resource {
    let identity = carina_core::resource::ResourceIdentity::try_from(identity.to_string())
        .expect("test identity must be non-empty");
    let mut resource = Resource::with_provider("mock", resource_type, identity, None);
    resource
        .attributes
        .insert(attribute.into(), Value::Concrete(ConcreteValue::Int(value)));
    resource
}

fn resource_with_string_attribute(
    resource_type: &str,
    identity: &str,
    attribute: &str,
    value: &str,
) -> Resource {
    let identity = carina_core::resource::ResourceIdentity::try_from(identity.to_string())
        .expect("test identity must be non-empty");
    let mut resource = Resource::with_provider("mock", resource_type, identity, None);
    resource.attributes.insert(
        attribute.into(),
        Value::Concrete(ConcreteValue::String(value.to_string().into())),
    );
    resource
}

fn create_failure_error(failure: CreateFailure) -> ProviderError {
    match failure.into_kind() {
        CreateFailureKind::Untracked(error) => error,
        CreateFailureKind::InFlight { pending: _, error } => error,
        CreateFailureKind::Created { created: _, error } => error,
    }
}

fn persisted_create_for_resume(
    id: &ResourceId,
    token: &str,
    identifier: Option<&str>,
) -> InFlightCreate {
    let identity = id
        .identity()
        .cloned()
        .expect("resume fixture identity is resolved");
    let mut row = serde_json::to_value(carina_state::ResourceState::new(
        &id.resource_type,
        identity,
        &id.provider,
    ))
    .expect("fixture row serializes");
    row["current"] = serde_json::json!({
        "kind": "create_in_flight",
        "envelope": {
            "token": token,
            "identifier": identifier,
            "write_only": {},
            "secrets": {},
        },
        "started_at": "2026-10-23T08:00:00Z",
    });
    let mut document =
        serde_json::to_value(carina_state::StateFile::new()).expect("fixture state serializes");
    document["resources"] = serde_json::json!([row]);
    let loaded = carina_state::state::check_and_migrate(&document.to_string())
        .expect("v15 fixture passes the public state loader")
        .into_state();
    match loaded.build_state_for_resource(id) {
        carina_core::resource::Current::CreateInFlight(pending) => pending,
        carina_core::resource::Current::Absent(state) => {
            panic!("resume fixture loaded as absent: {state:?}")
        }
        carina_core::resource::Current::Present(state) => {
            panic!("resume fixture loaded as present: {state:?}")
        }
        carina_core::resource::Current::MissingIdentifier(state) => {
            panic!("resume fixture loaded without an identifier: {state:?}")
        }
    }
}

fn ready_config(
    factory: &dyn ProviderFactory,
    attributes: &indexmap::IndexMap<String, Value>,
) -> ProviderReadyConfig {
    let config = resolved_provider_config(factory.name(), attributes.clone());
    carina_core::executor::prepare_provider_ready_config(factory, &config)
        .expect("test provider config should pass preparation")
}

fn resolved_provider_config(
    name: &str,
    attributes: indexmap::IndexMap<String, Value>,
) -> carina_core::parser::ResolvedProviderConfig {
    let parsed = carina_core::parser::InferredFile {
        providers: vec![carina_core::parser::ProviderConfig {
            name: name.to_string(),
            attributes,
            default_tags: indexmap::IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: indexmap::IndexMap::new(),
            binding: None,
        }],
        ..Default::default()
    };
    carina_core::parser::resolve_provider_attributes_with_remote(
        &parsed,
        &HashMap::new(),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("test provider config must be fully resolved")
    .iter()
    .next()
    .expect("test provider config must exist")
    .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_factory() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;

    assert_eq!(factory.name(), "mock");
    assert_eq!(factory.display_name(), "Mock Provider (Process)");

    let schemas = factory.schemas();
    let short_budget = schemas
        .iter()
        .find(|schema| schema.resource_type() == "test.short_budget")
        .and_then(|schema| schema.operation_config())
        .expect("mock provider should expose the short-budget schema");
    assert_eq!(short_budget.create_timeout_secs, Some(2));
    assert_eq!(short_budget.update_timeout_secs, Some(2));
    assert_eq!(short_budget.delete_timeout_secs, Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_create_polls_until_success() {
    const PENDING_POLLS: u32 = 2;

    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "pending-create", None);
    let resource = resource_with_int_attribute(
        "test.resource",
        "pending-create",
        "create_pending_polls",
        i64::from(PENDING_POLLS),
    );

    let state = create_state_for_writeback(
        provider
            .create_for_test(&id, create_request_for_test(resource).await)
            .await
            .expect("pending create should settle"),
    );

    assert_eq!(state.identifier.as_deref(), Some("mock-id"));
    assert!(state.exists);
    assert_eq!(
        state.attributes.get("__mock_create_polls_served__"),
        Some(&Value::Concrete(ConcreteValue::Int(i64::from(
            PENDING_POLLS
        ))))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_update_polls_until_success() {
    const PENDING_POLLS: u32 = 2;

    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "pending-update", None);
    let mut resource = resource_with_int_attribute("test.resource", "pending-update", "value", 1);
    let created = create_state_for_writeback(
        provider
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .expect("setup create should succeed"),
    );
    resource.set_attr("value", Value::Concrete(ConcreteValue::Int(2)));
    resource.set_attr(
        "update_pending_polls",
        Value::Concrete(ConcreteValue::Int(i64::from(PENDING_POLLS))),
    );
    let request = update_request_for_test(
        resource,
        created,
        &["value".to_string(), "update_pending_polls".to_string()],
    )
    .await;

    let state = provider
        .update(&id, &mock_identifier(), request)
        .await
        .expect("pending update should settle")
        .into_state_for_writeback();

    assert_eq!(
        state.attributes.get("value"),
        Some(&Value::Concrete(ConcreteValue::Int(2)))
    );
    assert_eq!(
        state.attributes.get("__mock_update_polls_served__"),
        Some(&Value::Concrete(ConcreteValue::Int(i64::from(
            PENDING_POLLS
        ))))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_delete_polls_until_success() {
    const PENDING_POLLS: u32 = 2;

    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "pending-delete", None);
    let resource = resource_with_int_attribute(
        "test.resource",
        "pending-delete",
        "delete_pending_polls",
        i64::from(PENDING_POLLS),
    );
    provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect("setup create should succeed");

    let delete_started = Instant::now();
    provider
        .delete(&id, &mock_identifier(), DeleteRequest::default())
        .await
        .expect("pending delete should settle");
    assert!(
        delete_started.elapsed() >= Duration::from_secs(u64::from(PENDING_POLLS)),
        "delete must serve every requested one-second pending poll"
    );

    let state = provider
        .read(&id, &mock_identifier(), ReadRequest)
        .await
        .expect("provider should remain usable")
        .into_state();
    assert!(!state.exists);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_create_can_settle_on_final_poll_at_deadline() {
    const PENDING_POLLS: u32 = 2;

    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "final-deadline-poll",
        None,
    );
    let resource = resource_with_int_attribute(
        "test.short_budget",
        "final-deadline-poll",
        "create_pending_polls",
        i64::from(PENDING_POLLS),
    );

    let state = create_state_for_writeback(
        provider
            .create_for_test(&id, create_request_for_test(resource).await)
            .await
            .expect("the final poll at the schema deadline should be allowed"),
    );

    assert_eq!(
        state.attributes.get("__mock_create_polls_served__"),
        Some(&Value::Concrete(ConcreteValue::Int(i64::from(
            PENDING_POLLS
        ))))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_cancel_between_polls_does_not_poison_instance() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let pending_id =
        ResourceId::with_provider_identity("mock", "test.resource", "cancel-pending", None);
    let pending = resource_with_int_attribute(
        "test.resource",
        "cancel-pending",
        "create_pending_polls",
        100,
    );

    tokio::time::timeout(
        Duration::from_millis(750),
        provider.create_for_test(&pending_id, create_request_for_test(pending).await),
    )
    .await
    .expect_err("the caller timeout should drop the operation between polls");

    let healthy_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-cancel", None);
    let healthy = resource_with_int_attribute("test.resource", "after-cancel", "value", 1);
    provider
        .create_for_test(&healthy_id, create_request_for_test(healthy).await)
        .await
        .expect("cancellation between polls must not poison the WASM instance");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_over_budget_times_out_without_poisoning_instance() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.short_budget", "over-budget", None);
    let resource = resource_with_int_attribute(
        "test.short_budget",
        "over-budget",
        "create_pending_polls",
        100,
    );

    let error = provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("pending create should exhaust its schema budget")
        .into_kind();
    match error {
        CreateFailureKind::InFlight { pending, error } => {
            assert_eq!(pending.id(), &id);
            assert_eq!(error.variant_name(), "timeout");
        }
        CreateFailureKind::Untracked(error) => {
            panic!("pending create was downgraded to untracked: {error}")
        }
        CreateFailureKind::Created { created, error } => panic!(
            "pending create was classified as created for {}: {error}",
            created.identifier()
        ),
    }

    let healthy_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-timeout", None);
    let healthy = resource_with_int_attribute("test.resource", "after-timeout", "value", 1);
    provider
        .create_for_test(&healthy_id, create_request_for_test(healthy).await)
        .await
        .expect("budget exhaustion must not poison the WASM instance");
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_mock_exercises_untracked_created_and_malformed_pending_verdicts() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");

    let untracked_id =
        ResourceId::with_provider_identity("mock", "test.resource", "mode-untracked", None);
    let untracked = resource_with_string_attribute(
        "test.resource",
        "mode-untracked",
        "create_mode",
        "untracked",
    );
    let untracked = provider
        .create_for_test(&untracked_id, create_request_for_test(untracked).await)
        .await
        .expect_err("untracked mode must return a classified failure")
        .into_kind();
    assert!(matches!(untracked, CreateFailureKind::Untracked(_error)));

    let created_id =
        ResourceId::with_provider_identity("mock", "test.resource", "mode-created", None);
    let mut created =
        resource_with_string_attribute("test.resource", "mode-created", "create_mode", "created");
    created.set_attr(
        "name",
        Value::Concrete(ConcreteValue::String("created-name".into())),
    );
    created.set_attr(
        "create_sent",
        Value::Concrete(ConcreteValue::String("name".into())),
    );
    let created = provider
        .create_for_test(&created_id, create_request_for_test(created).await)
        .await
        .expect_err("created mode must preserve its observed resource")
        .into_kind();
    let CreateFailureKind::Created { created, error } = created else {
        panic!("created mode returned the wrong failure variant");
    };
    drop(error);
    assert_eq!(created.identifier().as_str(), "mock-id");
    match created.sent() {
        SentRecord::Request(Sent::Only(attributes)) => {
            assert_eq!(
                attributes,
                &std::collections::BTreeSet::from(["name".to_string()])
            );
        }
        SentRecord::Request(Sent::AllRequestAttributes) => {
            panic!("created mode ignored its sent subset")
        }
        SentRecord::Persisted(envelope) => {
            panic!("fresh created verdict unexpectedly used a persisted envelope: {envelope:?}")
        }
    }

    let malformed_id =
        ResourceId::with_provider_identity("mock", "test.short_budget", "mode-malformed", None);
    let malformed = resource_with_string_attribute(
        "test.short_budget",
        "mode-malformed",
        "create_mode",
        "malformed_pending",
    );
    let malformed = provider
        .create_for_test(&malformed_id, create_request_for_test(malformed).await)
        .await
        .expect_err("an undecodable poll token must remain in flight")
        .into_kind();
    match malformed {
        CreateFailureKind::InFlight { pending, error } => {
            assert_eq!(pending.id(), &malformed_id);
            drop(error);
        }
        CreateFailureKind::Untracked(error) => {
            panic!("undecodable token was downgraded to untracked: {error}")
        }
        CreateFailureKind::Created { created, error } => panic!(
            "undecodable token was classified as created for {}: {error}",
            created.identifier()
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_issued_encode_failure_is_created_never_untracked() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id =
        ResourceId::with_provider_identity("mock", "test.resource", "unencodable-created", None);
    let resource = resource_with_string_attribute(
        "test.resource",
        "unencodable-created",
        "create_mode",
        "unencodable_created",
    );

    let failure = provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("guest encoding failure must remain classified")
        .into_kind();
    match failure {
        CreateFailureKind::Created { created, error } => {
            assert_eq!(created.identifier().as_str(), "mock-id");
            assert!(!error.to_string().is_empty());
        }
        CreateFailureKind::Untracked(error) => {
            panic!("issued create was lost as untracked: {error}")
        }
        CreateFailureKind::InFlight { pending, error } => {
            panic!("finished create unexpectedly remained in flight {pending:?}: {error}")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_unissued_pending_token_encode_failure_is_untracked() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "unencodable-pending",
        None,
    );
    let resource = resource_with_string_attribute(
        "test.short_budget",
        "unencodable-pending",
        "create_mode",
        "unencodable_pending",
    );

    match provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("an unissued unencodable continuation cannot be tracked")
        .into_kind()
    {
        CreateFailureKind::Untracked(error) => {
            assert!(error.to_string().contains("pending token"));
        }
        CreateFailureKind::Created { created, error } => panic!(
            "unissued create unexpectedly retained identifier {}: {error}",
            created.identifier()
        ),
        CreateFailureKind::InFlight { pending, error } => {
            panic!("unencodable token entered in-flight state {pending:?}: {error}")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_issued_pending_token_encode_failure_is_created() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "unencodable-issued-pending",
        None,
    );
    let resource = resource_with_string_attribute(
        "test.short_budget",
        "unencodable-issued-pending",
        "create_mode",
        "unencodable_pending_issued",
    );

    match provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("an issued create with no encodable continuation must be retained")
        .into_kind()
    {
        CreateFailureKind::Created { created, error } => {
            assert_eq!(created.identifier().as_str(), "mock-id");
            let Observation::ReadFailed(read_error) = created.observation() else {
                panic!("token encoding failure must be recorded as a failed observation")
            };
            assert!(read_error.to_string().contains("pending token"));
            assert!(matches!(
                created.sent(),
                SentRecord::Request(Sent::AllRequestAttributes)
            ));
            assert!(error.to_string().contains("pending token"));
        }
        CreateFailureKind::Untracked(error) => {
            panic!("issued create was lost as untracked: {error}")
        }
        CreateFailureKind::InFlight { pending, error } => {
            panic!("unencodable token entered in-flight state {pending:?}: {error}")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unissued_poll_token_encode_failure_keeps_previous_continuation() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id =
        ResourceId::with_provider_identity("mock", "test.short_budget", "unencodable-poll", None);
    let resource = resource_with_string_attribute(
        "test.short_budget",
        "unencodable-poll",
        "create_mode",
        "unencodable_poll",
    );

    match provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("an unissued poll with no replacement token must remain in flight")
        .into_kind()
    {
        CreateFailureKind::InFlight { pending, error } => {
            let token: serde_json::Value =
                serde_json::from_str(pending.token().expose_for_provider_call())
                    .expect("the previous continuation must remain decodable");
            assert_eq!(token["polls_served"], 0);
            assert!(error.to_string().contains("poll_create pending token"));
        }
        CreateFailureKind::Untracked(error) => {
            panic!("poll token encoding failure lost the previous continuation: {error}")
        }
        CreateFailureKind::Created { created, error } => panic!(
            "unissued poll unexpectedly retained identifier {}: {error}",
            created.identifier()
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn issued_poll_token_encode_failure_keeps_previous_continuation() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "unencodable-issued-poll",
        None,
    );
    let resource = resource_with_string_attribute(
        "test.short_budget",
        "unencodable-issued-poll",
        "create_mode",
        "unencodable_poll_issued",
    );

    match provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("an issued poll with no encodable replacement must remain in flight")
        .into_kind()
    {
        CreateFailureKind::InFlight { pending, error } => {
            let token: serde_json::Value =
                serde_json::from_str(pending.token().expose_for_provider_call())
                    .expect("the previous continuation must remain decodable");
            assert_eq!(token["polls_served"], 0);
            assert_eq!(
                pending.identifier().map(AppliedIdentifier::as_str),
                Some("mock-id")
            );
            assert!(error.to_string().contains("poll_create pending token"));
        }
        CreateFailureKind::Untracked(error) => {
            panic!("issued poll was lost as untracked: {error}")
        }
        CreateFailureKind::Created { created, error } => panic!(
            "issued poll ended the in-flight operation for {}: {error}",
            created.identifier()
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_pending_token_encode_failure_keeps_previous_continuation() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "unencodable-repending",
        None,
    );
    let previous_token = serde_json::json!({
        "remaining": 100,
        "polls_served": 0,
        "mode": "pending",
    })
    .to_string();
    let pending = persisted_create_for_resume(&id, &previous_token, Some("mock-id"));
    let resource = resource_with_string_attribute(
        "test.short_budget",
        "unencodable-repending",
        "create_mode",
        "unencodable_repending",
    );
    let request = ResumeCreateRequest::for_test(pending, create_request_for_test(resource).await)
        .expect("resume fixture route must match");

    match provider
        .resume_create(request)
        .await
        .map(ResumeOutcome::into_kind)
        .map_err(ResumeFailure::into_kind)
    {
        Err(ResumeFailureKind::InFlight { pending, error }) => {
            assert_eq!(
                pending.identifier().map(AppliedIdentifier::as_str),
                Some("mock-id")
            );
            assert_eq!(
                pending.token().expose_for_provider_call(),
                previous_token,
                "a failed token encode must keep the last decodable continuation"
            );
            assert!(error.to_string().contains("pending token"));
        }
        Err(ResumeFailureKind::Ended(error)) => {
            panic!("token encoding failure incorrectly ended the create: {error}")
        }
        Err(ResumeFailureKind::Created { created, error }) => panic!(
            "token encoding failure unexpectedly finalized {}: {error}",
            created.identifier()
        ),
        Ok(ResumeOutcomeKind::Resumed(created)) => {
            panic!("token encoding failure unexpectedly resumed {created:?}")
        }
        Ok(ResumeOutcomeKind::Resent(outcome)) => {
            panic!("token encoding failure unexpectedly resent {outcome:?}")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_mock_exercises_resumed_discarded_undetermined_repending_and_resend() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");

    for (mode, identifier, expected) in [
        ("resumed", Some("mock-id"), "resumed"),
        ("discarded", Some("mock-id"), "ended"),
        ("undetermined", None, "in_flight"),
        ("pending", None, "in_flight"),
        ("resend", None, "resent"),
    ] {
        let identity = format!("resume-{mode}");
        let id = ResourceId::with_provider_identity(
            "mock",
            "test.short_budget",
            carina_core::resource::ResourceIdentity::try_from(identity.clone())
                .expect("resume fixture identity is non-empty"),
            None,
        );
        let token = serde_json::json!({
            "remaining": 100,
            "polls_served": 0,
            "mode": mode,
        })
        .to_string();
        let pending = persisted_create_for_resume(&id, &token, identifier);
        let resource =
            resource_with_string_attribute("test.short_budget", &identity, "name", &identity);
        let request =
            ResumeCreateRequest::for_test(pending, create_request_for_test(resource).await)
                .expect("resume fixture route must match");
        let result = provider
            .resume_create(request)
            .await
            .map(ResumeOutcome::into_kind)
            .map_err(ResumeFailure::into_kind);

        match (expected, result) {
            ("resumed", Ok(ResumeOutcomeKind::Resumed(created))) => {
                assert_eq!(created.identifier().as_str(), "mock-id");
            }
            ("resent", Ok(ResumeOutcomeKind::Resent(outcome))) => match outcome.into_kind() {
                CreateOutcomeKind::Success { state } => {
                    assert_eq!(state.identifier().as_str(), "mock-id");
                }
                CreateOutcomeKind::PartialSuccess { state, diagnostic } => {
                    panic!(
                        "resend unexpectedly returned partial state {state:?} with {diagnostic:?}"
                    )
                }
            },
            ("ended", Err(ResumeFailureKind::Ended(error))) => drop(error),
            ("in_flight", Err(ResumeFailureKind::InFlight { pending, error })) => {
                assert_eq!(pending.id(), &id);
                drop(error);
            }
            (expected, Ok(ResumeOutcomeKind::Resumed(created))) => {
                panic!("expected {expected}, got resumed {created:?}")
            }
            (expected, Ok(ResumeOutcomeKind::Resent(outcome))) => {
                panic!("expected {expected}, got resent {outcome:?}")
            }
            (expected, Err(ResumeFailureKind::Ended(error))) => {
                panic!("expected {expected}, got ended {error}")
            }
            (expected, Err(ResumeFailureKind::Created { created, error })) => {
                panic!("expected {expected}, got created {created:?}: {error}")
            }
            (expected, Err(ResumeFailureKind::InFlight { pending, error })) => {
                panic!("expected {expected}, got in flight {pending:?}: {error}")
            }
        }
    }

    let malformed_id =
        ResourceId::with_provider_identity("mock", "test.short_budget", "resume-malformed", None);
    let malformed = persisted_create_for_resume(&malformed_id, "not-json", None);
    let malformed_resource = resource_with_string_attribute(
        "test.short_budget",
        "resume-malformed",
        "name",
        "resume-malformed",
    );
    let malformed_request =
        ResumeCreateRequest::for_test(malformed, create_request_for_test(malformed_resource).await)
            .expect("malformed-token fixture route must match");
    match provider
        .resume_create(malformed_request)
        .await
        .map(ResumeOutcome::into_kind)
        .map_err(ResumeFailure::into_kind)
    {
        Err(ResumeFailureKind::InFlight { pending, error }) => {
            assert_eq!(pending.id(), &malformed_id);
            drop(error);
        }
        Err(ResumeFailureKind::Ended(error)) => {
            panic!("undecodable resume token incorrectly ended the row: {error}")
        }
        Err(ResumeFailureKind::Created { created, error }) => panic!(
            "undecodable resume token was classified as created for {}: {error}",
            created.identifier()
        ),
        Ok(ResumeOutcomeKind::Resumed(created)) => panic!(
            "undecodable resume token unexpectedly resumed {}",
            created.identifier()
        ),
        Ok(ResumeOutcomeKind::Resent(outcome)) => {
            panic!("undecodable resume token unexpectedly resent: {outcome:?}")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn canonicalized_composite_identifiers_survive_finished_and_resumed_paths() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let canonical = "part-a|part-b";

    let finished_id =
        ResourceId::with_provider_identity("mock", "test.resource", "canonical-finished", None);
    let finished = resource_with_string_attribute(
        "test.resource",
        "canonical-finished",
        "canonical_identifier",
        canonical,
    );
    let finished = provider
        .create_for_test(&finished_id, create_request_for_test(finished).await)
        .await
        .expect("canonicalized finished create should succeed")
        .into_kind();
    match finished {
        CreateOutcomeKind::Success { state } => {
            assert_eq!(state.identifier().as_str(), canonical)
        }
        CreateOutcomeKind::PartialSuccess { state, diagnostic } => {
            panic!("unexpected partial canonical create {state:?}: {diagnostic:?}")
        }
    }

    let resumed_id =
        ResourceId::with_provider_identity("mock", "test.short_budget", "canonical-resumed", None);
    let token = serde_json::json!({
        "remaining": 100,
        "polls_served": 0,
        "mode": "resumed",
    })
    .to_string();
    let pending = persisted_create_for_resume(&resumed_id, &token, Some("provisional-id"));
    let resumed = resource_with_string_attribute(
        "test.short_budget",
        "canonical-resumed",
        "canonical_identifier",
        canonical,
    );
    let request = ResumeCreateRequest::for_test(pending, create_request_for_test(resumed).await)
        .expect("resume fixture route must match");
    match provider
        .resume_create(request)
        .await
        .map(ResumeOutcome::into_kind)
        .map_err(ResumeFailure::into_kind)
    {
        Ok(ResumeOutcomeKind::Resumed(created)) => {
            assert_eq!(created.identifier().as_str(), canonical);
        }
        Ok(ResumeOutcomeKind::Resent(outcome)) => {
            panic!("expected resumed canonical identifier, got resent {outcome:?}")
        }
        Err(ResumeFailureKind::Ended(error)) => panic!("canonical resume ended: {error}"),
        Err(ResumeFailureKind::Created { created, error }) => {
            panic!("canonical resume failed after create {created:?}: {error}")
        }
        Err(ResumeFailureKind::InFlight { pending, error }) => {
            panic!("canonical resume stayed in flight {pending:?}: {error}")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_operation_budget_includes_initial_guest_call() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id =
        ResourceId::with_provider_identity("mock", "test.short_budget", "slow-initial-call", None);
    let mut resource = resource_with_int_attribute(
        "test.short_budget",
        "slow-initial-call",
        "call_delay_millis",
        2_100,
    );
    resource.set_attr(
        "create_pending_polls",
        Value::Concrete(ConcreteValue::Int(1)),
    );

    let error = provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("the slow initial guest call should exhaust the schema budget");

    match error.into_kind() {
        CreateFailureKind::InFlight {
            pending: _pending,
            error,
        } => assert!(matches!(
            error,
            carina_core::provider::ProviderError::Timeout(_timeout)
        )),
        CreateFailureKind::Untracked(error) => {
            panic!("slow initial call was classified as untracked: {error}")
        }
        CreateFailureKind::Created { created, error } => panic!(
            "slow initial call was classified as created for {}: {error}",
            created.identifier()
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_production_budget_allows_25s_host_wait() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "host-wait-25s", None);
    let resource = resource_with_int_attribute(
        "test.resource",
        "host-wait-25s",
        "call_delay_millis",
        25_000,
    );

    provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect("a host wait below the compute budget should succeed");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_production_budget_allows_35s_host_wait() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "host-wait-35s", None);
    let resource = resource_with_int_attribute(
        "test.resource",
        "host-wait-35s",
        "call_delay_millis",
        35_000,
    );

    provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect("time parked in a host import must not consume guest compute budget");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_short_compute_budget_excludes_5s_host_wait() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(2),
        Duration::from_secs(30),
    ));
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "host-wait-5s", None);
    let resource =
        resource_with_int_attribute("test.resource", "host-wait-5s", "call_delay_millis", 5_000);

    provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect("host wait longer than the compute budget should succeed");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_short_compute_budget_interrupts_busy_loop() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(2),
        Duration::from_secs(60),
    ));
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "spin-forever", None);
    let mut resource = resource_with_int_attribute("test.resource", "spin-forever", "value", 1);
    resource.set_attr(
        "call_spin_forever",
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    let started = Instant::now();

    let error = provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("busy guest computation should exhaust its compute budget");
    let elapsed = started.elapsed();
    let error = create_failure_error(error);
    let rendered = error.to_string();

    assert!(matches!(error, ProviderError::Timeout(_)), "{rendered}");
    assert!(rendered.contains("guest compute budget"), "{rendered}");
    assert!(
        !rendered.to_lowercase().contains("credential"),
        "{rendered}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "compute timeout should fire well before the wall budget; elapsed {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_short_compute_budget_interrupts_spin_with_host_calls() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(2),
        Duration::from_secs(60),
    ));
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "spin-with-host-calls-forever",
        None,
    );
    let mut resource =
        resource_with_int_attribute("test.resource", "spin-with-host-calls-forever", "value", 1);
    resource.set_attr(
        "call_spin_with_host_calls_forever",
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    let started = Instant::now();

    let error = tokio::time::timeout(
        Duration::from_secs(10),
        provider.create_for_test(&id, create_request_for_test(resource).await),
    )
    .await
    .expect("compute timeout should fire within 10s even when the guest calls host imports")
    .expect_err("guest computation should exhaust its compute budget across host calls");
    let elapsed = started.elapsed();
    let error = create_failure_error(error);
    let rendered = error.to_string();

    assert!(matches!(error, ProviderError::Timeout(_)), "{rendered}");
    assert!(rendered.contains("guest compute budget"), "{rendered}");
    assert!(
        !rendered.to_lowercase().contains("credential"),
        "{rendered}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "compute timeout should fire well before the wall budget; elapsed {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_short_wall_budget_times_out_host_wait() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(30),
        Duration::from_secs(2),
    ));
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "wall-timeout", None);
    let resource =
        resource_with_int_attribute("test.resource", "wall-timeout", "call_delay_millis", 10_000);
    let started = Instant::now();

    let error = provider
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("the whole guest call should be bounded by its wall budget");
    let elapsed = started.elapsed();
    let error = create_failure_error(error);
    let rendered = error.to_string();

    assert!(matches!(error, ProviderError::Timeout(_)), "{rendered}");
    let cause = error
        .detail()
        .cause()
        .expect("wall timeout must retain its typed cause");
    assert!(
        cause.source().is_some(),
        "the operation-aware timeout cause must retain WallBudgetExceeded"
    );
    assert!(rendered.contains("wall-clock budget"), "{rendered}");
    assert_eq!(
        rendered.matches("wall-clock budget").count(),
        1,
        "{rendered}"
    );
    assert!(
        !rendered.to_lowercase().contains("credential"),
        "{rendered}"
    );
    assert!(
        elapsed >= Duration::from_millis(1_500) && elapsed < Duration::from_secs(6),
        "wall timeout should fire after about 2s; elapsed {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_poll_sleep_does_not_block_sibling_operation() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let pending_id =
        ResourceId::with_provider_identity("mock", "test.resource", "sleeping-poll", None);
    let resource =
        resource_with_int_attribute("test.resource", "sleeping-poll", "create_pending_polls", 1);
    let request = create_request_for_test(resource).await;
    let sibling_id =
        ResourceId::with_provider_identity("mock", "test.resource", "sibling-read", None);

    let pending_operation = provider.create_for_test(&pending_id, request);
    let sibling_operation = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::time::timeout(
            Duration::from_millis(500),
            provider.read(&sibling_id, &mock_identifier(), ReadRequest),
        )
        .await
        .expect("sibling read must not wait for the provider's poll sleep")
        .expect("sibling read should succeed")
        .into_state()
    };

    let (pending_result, sibling_state) = tokio::join!(pending_operation, sibling_operation);
    pending_result.expect("pending create should settle");
    assert!(!sibling_state.exists);
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_short_wall_budget_times_out_validate_config() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(30),
        Duration::from_secs(2),
    ));
    let attributes = indexmap::IndexMap::from([(
        "validate_delay_millis".to_string(),
        Value::Concrete(ConcreteValue::Int(10_000)),
    )]);
    let config = resolved_provider_config(factory.name(), attributes);
    let started = Instant::now();

    let error = carina_core::executor::prepare_provider_ready_config(&factory, &config)
        .expect_err("validate_config should be bounded by the guest wall budget");
    let elapsed = started.elapsed();
    let rendered = error.to_string();

    assert!(rendered.contains("validate_config"), "{rendered}");
    assert!(rendered.contains("wall-clock budget"), "{rendered}");
    assert!(
        !rendered.to_lowercase().contains("credential"),
        "{rendered}"
    );
    assert!(
        elapsed >= Duration::from_millis(1_500) && elapsed < Duration::from_secs(6),
        "validate_config should time out after about 2s; elapsed {elapsed:?}"
    );

    let clean_config = resolved_provider_config(factory.name(), indexmap::IndexMap::new());
    let retry_started = Instant::now();
    let retry_error = carina_core::executor::prepare_provider_ready_config(&factory, &clean_config)
        .expect_err("the timed-out init instance must remain poisoned");
    let retry_elapsed = retry_started.elapsed();
    let retry_rendered = retry_error.to_string();

    assert!(
        retry_rendered.contains("unusable after wall-clock timeout in validate_config"),
        "{retry_rendered}"
    );
    assert!(
        !retry_rendered.contains("cannot enter component instance"),
        "the follow-up must report the original poison reason: {retry_rendered}"
    );
    assert!(
        retry_elapsed < Duration::from_secs(1),
        "a poisoned init instance must fail fast; elapsed {retry_elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_store_lock_wait_does_not_count_against_guest_call_timeout() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(30),
        Duration::from_secs(1),
    ));
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let first_id = ResourceId::with_provider_identity("mock", "test.resource", "slow-one", None);
    let second_id = ResourceId::with_provider_identity("mock", "test.resource", "slow-two", None);
    let fast_id = ResourceId::with_provider_identity("mock", "test.resource", "queued-fast", None);
    let first = create_request_for_test(resource_with_int_attribute(
        "test.resource",
        "slow-one",
        "call_delay_millis",
        600,
    ))
    .await;
    let second = create_request_for_test(resource_with_int_attribute(
        "test.resource",
        "slow-two",
        "call_delay_millis",
        600,
    ))
    .await;
    let fast = create_request_for_test(resource_with_int_attribute(
        "test.resource",
        "queued-fast",
        "value",
        1,
    ))
    .await;

    let first_operation = provider.create_for_test(&first_id, first);
    let second_operation = async {
        tokio::time::sleep(Duration::from_millis(25)).await;
        provider.create_for_test(&second_id, second).await
    };
    let fast_operation = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let queued_at = Instant::now();
        let result = provider.create_for_test(&fast_id, fast).await;
        (result, queued_at.elapsed())
    };
    let (first_result, second_result, (fast_result, fast_elapsed)) =
        tokio::join!(first_operation, second_operation, fast_operation);

    first_result.expect("first slow guest call should stay within the injected timeout");
    second_result.expect("second slow guest call should stay within the injected timeout");
    fast_result.expect("queued lock time must not consume the guest-call timeout");
    assert!(
        fast_elapsed > Duration::from_secs(1),
        "the fast call must have spent longer than the injected timeout queued behind siblings"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_create_and_read() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");

    assert_eq!(provider.name(), "mock");

    // Read before create - should return a state with no identifier and empty attributes
    let id = ResourceId::with_provider_identity("mock", "test.resource", "my-resource", None);
    let state = provider
        .read(&id, &mock_identifier(), ReadRequest)
        .await
        .expect("read should not error")
        .into_state();
    assert!(state.identifier.is_none());
    assert!(state.attributes.is_empty());

    // Create a resource
    let mut resource = Resource::with_provider("mock", "test.resource", "my-resource", None);
    resource.attributes = indexmap::IndexMap::from([
        (
            "name".into(),
            Value::Concrete(ConcreteValue::String("my-resource".into())),
        ),
        (
            "region".into(),
            Value::Concrete(ConcreteValue::String("us-east-1".into())),
        ),
        ("count".into(), Value::Concrete(ConcreteValue::Int(42))),
    ]);

    let created = create_state_for_writeback(
        provider
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .expect("create should succeed"),
    );
    assert_eq!(created.identifier, Some("mock-id".into()));
    assert_eq!(
        created.attributes.get("name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "my-resource".into()
        )))
    );
    assert_eq!(
        created.attributes.get("region"),
        Some(&Value::Concrete(ConcreteValue::String("us-east-1".into())))
    );
    assert_eq!(
        created.attributes.get("count"),
        Some(&Value::Concrete(ConcreteValue::Int(42)))
    );

    // Read back - should return the created state
    let read_state = provider
        .read(&id, &mock_identifier(), ReadRequest)
        .await
        .expect("read should not error")
        .into_state();
    assert_eq!(read_state.identifier, Some("mock-id".into()));
    assert_eq!(
        read_state.attributes.get("name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "my-resource".into()
        )))
    );
    assert_eq!(
        read_state.attributes.get("region"),
        Some(&Value::Concrete(ConcreteValue::String("us-east-1".into())))
    );
    assert_eq!(
        read_state.attributes.get("count"),
        Some(&Value::Concrete(ConcreteValue::Int(42)))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_guest_read_reports_nested_non_finite_float_as_provider_error() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "__mock_non_finite_read__",
        None,
    );

    let error = provider
        .read(&id, &mock_identifier(), ReadRequest)
        .await
        .expect_err("guest output containing nested NaN must be a provider error");

    let rendered = error.to_string();
    assert!(rendered.contains("WASM boundary encode error"));
    assert!(rendered.contains("non-finite float NaN"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_update_and_delete() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");

    let id = ResourceId::with_provider_identity("mock", "test.resource", "updatable", None);

    // Create first
    let mut resource = Resource::with_provider("mock", "test.resource", "updatable", None);
    resource.attributes = indexmap::IndexMap::from([
        (
            "color".into(),
            Value::Concrete(ConcreteValue::String("red".into())),
        ),
        ("size".into(), Value::Concrete(ConcreteValue::Int(10))),
        (
            "removed".into(),
            Value::Concrete(ConcreteValue::String("old".into())),
        ),
        (
            "reassert_token".into(),
            Value::Concrete(ConcreteValue::String("token".into())),
        ),
    ]);

    let created = create_state_for_writeback(
        provider
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .expect("create should succeed"),
    );
    assert_eq!(
        created.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("red".into())))
    );

    // Prepare all four patch variants through the checked core seam.
    resource.set_attr(
        "color",
        Value::Concrete(ConcreteValue::String("blue".into())),
    );
    resource.attributes.shift_remove("removed");
    resource.set_attr("added", Value::Concrete(ConcreteValue::Int(20)));
    let request = update_request_for_test(
        resource,
        created.clone(),
        &[
            "added".to_string(),
            "color".to_string(),
            "removed".to_string(),
        ],
    )
    .await;

    let updated = provider
        .update(&id, &mock_identifier(), request)
        .await
        .expect("update should succeed")
        .into_state_for_writeback();
    assert_eq!(
        updated.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("blue".into())))
    );
    assert_eq!(
        updated.attributes.get("added"),
        Some(&Value::Concrete(ConcreteValue::Int(20)))
    );
    assert!(!updated.attributes.contains_key("removed"));
    assert_eq!(
        updated.attributes.get("reassert_token"),
        Some(&Value::Concrete(ConcreteValue::String("token".into())))
    );
    // The mock echoes the applied patch op kinds + keys as a sentinel
    // attribute so we can verify the patch round-tripped through the
    // WIT boundary in op order.
    let echoed = updated
        .attributes
        .get("__mock_patch_ops__")
        .expect("mock should echo patch ops");
    let Value::Concrete(ConcreteValue::List(ops)) = echoed else {
        panic!("__mock_patch_ops__ should be a list, got {echoed:?}");
    };
    assert_eq!(ops.len(), 4);
    assert_eq!(
        ops[0],
        Value::Concrete(ConcreteValue::String("add:added".into()))
    );
    assert_eq!(
        ops[1],
        Value::Concrete(ConcreteValue::String("replace:color".into()))
    );
    assert_eq!(
        ops[2],
        Value::Concrete(ConcreteValue::String("remove:removed".into()))
    );
    assert_eq!(
        ops[3],
        Value::Concrete(ConcreteValue::String("reassert:reassert_token".into()))
    );

    // Read to verify update persisted in memory
    let read_state = provider
        .read(&id, &mock_identifier(), ReadRequest)
        .await
        .expect("read should not error")
        .into_state();
    assert_eq!(
        read_state.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("blue".into())))
    );

    // Delete
    provider
        .delete(&id, &mock_identifier(), DeleteRequest::default())
        .await
        .expect("delete should succeed");

    // Read after delete - should return empty state (no identifier, no attributes)
    let deleted_state = provider
        .read(&id, &mock_identifier(), ReadRequest)
        .await
        .expect("read should not error")
        .into_state();
    assert!(deleted_state.identifier.is_none());
    assert!(deleted_state.attributes.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_update_accepts_redacted_secret_in_current_state() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "secret-update", None);
    let (persisted_secret, _) = redacted_secret_and_legacy_marker("stored-write-only-secret");
    let from = State::existing(
        id.clone(),
        HashMap::from([
            (
                "color".to_string(),
                Value::Concrete(ConcreteValue::String("red".into())),
            ),
            ("persisted_token".to_string(), persisted_secret),
        ]),
    )
    .with_identifier("mock-id");
    let resource = Resource::with_provider("mock", "test.resource", "secret-update", None)
        .with_attribute(
            "color",
            Value::Concrete(ConcreteValue::String("blue".into())),
        );
    let request = update_request_for_test(
        resource,
        from,
        &["color".to_string(), "persisted_token".to_string()],
    )
    .await;

    let updated = provider
        .update(&id, &mock_identifier(), request)
        .await
        .expect("persisted current-state secrets must cross the update boundary")
        .into_state_for_writeback();

    assert_eq!(
        updated.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("blue".into())))
    );
    assert!(
        !updated.attributes.contains_key("persisted_token"),
        "the ordinary remove patch must still be applied"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_update_carries_unchanged_redacted_secret_into_outcome() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "secret-carry", None);
    let (persisted_secret, _) = redacted_secret_and_legacy_marker("stored-write-only-secret");
    let from = State::existing(
        id.clone(),
        HashMap::from([
            (
                "color".to_string(),
                Value::Concrete(ConcreteValue::String("red".into())),
            ),
            ("persisted_token".to_string(), persisted_secret.clone()),
        ]),
    )
    .with_identifier("mock-id");
    let resource = Resource::with_provider("mock", "test.resource", "secret-carry", None)
        .with_attribute(
            "color",
            Value::Concrete(ConcreteValue::String("blue".into())),
        );
    let request = update_request_for_test(resource, from, &["color".to_string()]).await;

    let updated = provider
        .update(&id, &mock_identifier(), request)
        .await
        .expect("unchanged persisted secrets must cross the update outcome boundary")
        .into_state_for_writeback();

    assert_eq!(
        updated.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("blue".into())))
    );
    assert_eq!(
        updated.attributes.get("persisted_token"),
        Some(&persisted_secret),
        "the provider must carry the typed stored secret forward unchanged",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_update_preserves_cloud_attribute_with_nested_stored_secret_for_read() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "nested-secret", None);
    let cloud_payload = Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
        "label".to_string() => Value::Concrete(ConcreteValue::String("kept".into())),
        "token".to_string() => Value::Concrete(ConcreteValue::String("cloud-secret".into())),
    }));
    let mut resource = Resource::with_provider("mock", "test.resource", "nested-secret", None);
    resource.attributes = indexmap::IndexMap::from([
        (
            "color".into(),
            Value::Concrete(ConcreteValue::String("red".into())),
        ),
        ("payload".into(), cloud_payload.clone()),
    ]);
    provider
        .create_for_test(&id, create_request_for_test(resource.clone()).await)
        .await
        .expect("create should seed the mock cloud store");

    let (persisted_secret, _) = redacted_secret_and_legacy_marker("cloud-secret");
    let from = State::existing(
        id.clone(),
        HashMap::from([
            (
                "color".to_string(),
                Value::Concrete(ConcreteValue::String("red".into())),
            ),
            (
                "payload".to_string(),
                Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                    "label".to_string() => Value::Concrete(ConcreteValue::String("kept".into())),
                    "token".to_string() => persisted_secret,
                })),
            ),
        ]),
    )
    .with_identifier("mock-id");
    resource.set_attr(
        "color",
        Value::Concrete(ConcreteValue::String("blue".into())),
    );
    let request = update_request_for_test(resource, from, &["color".to_string()]).await;
    provider
        .update(&id, &mock_identifier(), request)
        .await
        .expect("update should preserve the unchanged persisted attribute");

    let read = provider
        .read(&id, &mock_identifier(), ReadRequest)
        .await
        .expect("read after update should succeed")
        .into_state();
    assert_eq!(read.attributes.get("payload"), Some(&cloud_payload));
    assert_eq!(
        read.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("blue".into())))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_normalizer() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");

    // normalize_desired: mock provider returns resources unchanged
    let mut resources = vec![{
        let mut r = Resource::with_provider("mock", "test.resource", "norm-test", None);
        r.attributes = indexmap::IndexMap::from([(
            "key".into(),
            Value::Concrete(ConcreteValue::String("value".into())),
        )]);
        r
    }];
    let original_attrs = resources[0].resolved_attributes();
    normalizer.normalize_desired(&mut resources).await.unwrap();
    assert_eq!(resources[0].resolved_attributes(), original_attrs);

    // normalize_state: mock provider returns states unchanged
    let id = ResourceId::with_provider_identity("mock", "test.resource", "norm-test", None);
    let attrs = HashMap::from([
        (
            "key".into(),
            Value::Concrete(ConcreteValue::String("value".into())),
        ),
        (
            "__mock_normalize_state__".into(),
            Value::Concrete(ConcreteValue::Bool(true)),
        ),
    ]);
    let state = carina_core::resource::State::existing(id.clone(), attrs.clone());
    let mut states = HashMap::from([(id.clone(), state)]);
    normalize_provider_states(normalizer.as_ref(), &mut states)
        .await
        .unwrap();
    let result_state = states.values().next().unwrap();
    assert_eq!(
        result_state.attributes.get("key"),
        Some(&Value::Concrete(ConcreteValue::String("value".into())))
    );
    assert_eq!(
        result_state.attributes.get("__mock_normalized_state__"),
        Some(&Value::Concrete(ConcreteValue::Bool(true))),
        "resolved-state normalization returned by the guest must be applied"
    );

    // Two pending IDs have the same display string. Correlation across the
    // WASM boundary must therefore use the IDs themselves, not `to_string()`.
    let first_id = ResourceId::pending_with_provider("mock", "test.resource", None);
    let second_id = ResourceId::pending_with_provider("mock", "test.resource", None);
    assert_ne!(first_id, second_id);
    let mut pending_states = HashMap::from([
        (
            first_id.clone(),
            State::existing(
                first_id.clone(),
                HashMap::from([(
                    "__mock_normalize_state__".to_string(),
                    Value::Concrete(ConcreteValue::String("first".into())),
                )]),
            ),
        ),
        (
            second_id.clone(),
            State::existing(
                second_id.clone(),
                HashMap::from([(
                    "__mock_normalize_state__".to_string(),
                    Value::Concrete(ConcreteValue::String("second".into())),
                )]),
            ),
        ),
    ]);
    normalize_provider_states(normalizer.as_ref(), &mut pending_states)
        .await
        .unwrap();

    assert_eq!(pending_states.len(), 2);
    assert_eq!(
        pending_states[&first_id]
            .attributes
            .get("__mock_normalized_state__"),
        Some(&Value::Concrete(ConcreteValue::String("first".into()))),
        "the first pending state must receive its own normalized WASM result"
    );
    assert_eq!(
        pending_states[&second_id]
            .attributes
            .get("__mock_normalized_state__"),
        Some(&Value::Concrete(ConcreteValue::String("second".into()))),
        "the second pending state must receive its own normalized WASM result"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_normalize_desired_echo_preserves_nested_enum_typing() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");
    let hosted_zone_type = AttributeType::enum_(
        TypeIdentity::new(
            Some("aws"),
            ["route53", "RecordSet", "AliasTarget"],
            "HostedZoneId",
        ),
        Some(vec!["Z2FDTNDATAQYW2".to_string()]),
        Vec::new(),
        None,
        None,
    );
    let hosted_zone = EnumValueResolver::new(&hosted_zone_type)
        .resolve_raw(&RawEnumIdentifier::parse("Z2FDTNDATAQYW2"))
        .expect("the authored identifier must resolve through the Enum union member");
    let mut resources = vec![
        Resource::with_provider("mock", "test.resource", "enum-echo", None).with_attribute(
            "alias_target",
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "hosted_zone_id".to_string(),
                Value::Concrete(ConcreteValue::CanonicalEnum(hosted_zone)),
            )]))),
        ),
    ];

    normalizer
        .normalize_desired(&mut resources)
        .await
        .expect("the echo normalizer must succeed");

    let Some(Value::Concrete(ConcreteValue::Map(alias_target))) =
        resources[0].get_attr("alias_target")
    else {
        panic!("alias_target must remain a map");
    };
    let hosted_zone = alias_target
        .get("hosted_zone_id")
        .expect("hosted_zone_id must remain present");
    assert!(matches!(
        hosted_zone,
        Value::Concrete(ConcreteValue::CanonicalEnum(value))
            if value.api_value() == "Z2FDTNDATAQYW2"
    ));
    assert_eq!(
        carina_core::value::format_value(hosted_zone),
        "Z2FDTNDATAQYW2",
        "the plan formatter must keep identifier syntax instead of quoting an echoed string"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_normalize_desired_preserves_ref_and_unknown_bearing_attributes() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");

    let peer_ref = Value::resource_ref("peer", "identifier", vec![]);
    let mut resource = Resource::with_provider("mock", "test.resource", "ref-bearing", None);
    resource.set_attr(
        "tags",
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
            (
                "Name".to_string(),
                Value::Concrete(ConcreteValue::String("ref-bearing".into())),
            ),
            ("Peer".to_string(), peer_ref),
        ]))),
    );
    resource.set_attr(
        "pending",
        Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
            DeferredValue::Unknown(UnknownReason::ForValue),
        )])),
    );
    let mut resources = vec![resource];
    let expected_tags = resources[0]
        .get_attr("tags")
        .expect("tags must be present")
        .clone();

    normalizer
        .normalize_desired(&mut resources)
        .await
        .expect("the WASM boundary must hide values its transport cannot encode");

    assert_eq!(resources[0].get_attr("tags"), Some(&expected_tags));
    assert!(matches!(
        resources[0].get_attr("pending"),
        Some(Value::Concrete(ConcreteValue::List(values)))
            if matches!(
                values.as_slice(),
                [Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue))]
            )
    ));
    assert_eq!(
        resources[0].attributes.keys().collect::<Vec<_>>(),
        vec!["tags", "pending"],
        "ref/Unknown-bearing attributes must be restored unchanged after the guest call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_state_accepts_one_rekeyed_result_and_rejects_zero_results() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");

    let rekeyed_id = ResourceId::pending_with_provider("mock", "test.resource", None);
    let mut rekeyed_states = HashMap::from([(
        rekeyed_id.clone(),
        State::existing(
            rekeyed_id.clone(),
            HashMap::from([
                (
                    "__mock_rekey_state_result__".to_string(),
                    Value::Concrete(ConcreteValue::Bool(true)),
                ),
                (
                    "__mock_normalize_state__".to_string(),
                    Value::Concrete(ConcreteValue::String("rekeyed".into())),
                ),
            ]),
        ),
    )]);
    normalize_provider_states(normalizer.as_ref(), &mut rekeyed_states)
        .await
        .expect("a single pending result is correlated by position, not guest key");
    assert_eq!(
        rekeyed_states[&rekeyed_id]
            .attributes
            .get("__mock_normalized_state__"),
        Some(&Value::Concrete(ConcreteValue::String("rekeyed".into())))
    );

    let dropped_id = ResourceId::pending_with_provider("mock", "test.resource", None);
    let mut dropped_states = HashMap::from([(
        dropped_id.clone(),
        State::existing(
            dropped_id,
            HashMap::from([(
                "__mock_drop_state_result__".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            )]),
        ),
    )]);
    assert!(
        normalize_provider_states(normalizer.as_ref(), &mut dropped_states)
            .await
            .is_err(),
        "a pending call returning zero entries must fail"
    );

    let hydrate_id = ResourceId::pending_with_provider("mock", "test.resource", None);
    let mut hydrate_states = HashMap::from([(
        hydrate_id.clone(),
        State::existing(
            hydrate_id.clone(),
            HashMap::from([(
                "__mock_rekey_state_result__".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            )]),
        ),
    )]);
    let saved_attrs = lifted_saved_attrs([(
        hydrate_id.clone(),
        HashMap::from([(
            "__mock_hydrate_read_state__".to_string(),
            Value::Concrete(ConcreteValue::String("hydrated".into())),
        )]),
    )]);
    hydrate_provider_states(
        normalizer.as_ref(),
        &mut hydrate_states,
        saved_attrs.as_provider_saved_attrs(),
    )
    .await
    .expect("a single pending hydrate result is correlated by position, not guest key");
    assert_eq!(
        hydrate_states[&hydrate_id]
            .attributes
            .get("__mock_hydrated_read_state__"),
        Some(&Value::Concrete(ConcreteValue::String("hydrated".into())))
    );

    let dropped_hydrate_id = ResourceId::pending_with_provider("mock", "test.resource", None);
    let mut dropped_hydrate_states = HashMap::from([(
        dropped_hydrate_id.clone(),
        State::existing(
            dropped_hydrate_id,
            HashMap::from([(
                "__mock_drop_state_result__".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            )]),
        ),
    )]);
    assert!(
        hydrate_provider_states(
            normalizer.as_ref(),
            &mut dropped_hydrate_states,
            &SavedAttrs::new()
        )
        .await
        .is_err(),
        "a pending hydrate call returning zero entries must fail"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_resolved_wire_key_collisions_are_normalized_one_at_a_time() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");

    let first_id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "same-identity",
        Some("first".to_string()),
    );
    let second_id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "same-identity",
        Some("second".to_string()),
    );
    let mut states = HashMap::from([
        (
            first_id.clone(),
            State::existing(
                first_id.clone(),
                HashMap::from([(
                    "__mock_normalize_state__".to_string(),
                    Value::Concrete(ConcreteValue::String("first".into())),
                )]),
            ),
        ),
        (
            second_id.clone(),
            State::existing(
                second_id.clone(),
                HashMap::from([(
                    "__mock_normalize_state__".to_string(),
                    Value::Concrete(ConcreteValue::String("second".into())),
                )]),
            ),
        ),
    ]);
    normalize_provider_states(normalizer.as_ref(), &mut states)
        .await
        .unwrap();

    for (id, expected) in [(&first_id, "first"), (&second_id, "second")] {
        assert_eq!(
            states[id].attributes.get("__mock_normalized_state__"),
            Some(&Value::Concrete(ConcreteValue::String(
                expected.to_string().into()
            ))),
            "every host state sharing a wire key must be normalized"
        );
    }

    let mut hydrate_states = HashMap::from([
        (
            first_id.clone(),
            State::existing(first_id.clone(), HashMap::new()),
        ),
        (
            second_id.clone(),
            State::existing(second_id.clone(), HashMap::new()),
        ),
    ]);
    let saved_attrs = lifted_saved_attrs([
        (
            first_id.clone(),
            HashMap::from([(
                "__mock_hydrate_read_state__".to_string(),
                Value::Concrete(ConcreteValue::String("first".into())),
            )]),
        ),
        (
            second_id.clone(),
            HashMap::from([(
                "__mock_hydrate_read_state__".to_string(),
                Value::Concrete(ConcreteValue::String("second".into())),
            )]),
        ),
    ]);
    hydrate_provider_states(
        normalizer.as_ref(),
        &mut hydrate_states,
        saved_attrs.as_provider_saved_attrs(),
    )
    .await
    .unwrap();

    for (id, expected) in [(&first_id, "first"), (&second_id, "second")] {
        assert_eq!(
            hydrate_states[id]
                .attributes
                .get("__mock_hydrated_read_state__"),
            Some(&Value::Concrete(ConcreteValue::String(
                expected.to_string().into()
            ))),
            "every host state sharing a wire key must be hydrated"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_provider_config_preparation_propagates_boundary_encoding_failure() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let attributes = indexmap::IndexMap::from([(
        "bad".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Float(f64::INFINITY),
        )])),
    )]);

    let resolved = resolved_provider_config(factory.name(), attributes);
    let result = carina_core::executor::prepare_provider_ready_config(&factory, &resolved);

    assert!(
        result.is_err(),
        "provider config encoding failures must stop construction"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_create_normalizer_propagates_instance_initialization_failure() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let attributes = indexmap::IndexMap::from([(
        "__mock_initialize_error__".to_string(),
        Value::Concrete(ConcreteValue::String("normalizer init failed".into())),
    )]);
    let config = ready_config(&factory, &attributes);

    let result = factory
        .create_normalizer(Some("bad-normalizer"), &config)
        .await;

    let error = match result {
        Ok(_) => panic!("normalizer instance failures must not become NoopNormalizer"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("intentional mock initialization failure"),
        "unexpected normalizer initialization error: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_normalizer_rejects_missing_and_extra_state_keys_atomically() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");

    for identity in [
        "__mock_normalize_state_missing_key__",
        "__mock_normalize_state_extra_key__",
    ] {
        let id = ResourceId::with_provider_identity("mock", "test.resource", identity, None);
        let update_id =
            ResourceId::with_provider_identity("mock", "test.resource", "would-update", None);
        let mut states = HashMap::from([
            (
                id.clone(),
                State::existing(
                    id,
                    HashMap::from([(
                        "original".to_string(),
                        Value::Concrete(ConcreteValue::String("value".into())),
                    )]),
                ),
            ),
            (
                update_id.clone(),
                State::existing(
                    update_id,
                    HashMap::from([(
                        "__mock_normalize_state__".to_string(),
                        Value::Concrete(ConcreteValue::String("would-change".into())),
                    )]),
                ),
            ),
        ]);
        let before = states.clone();

        let result = normalize_provider_states(normalizer.as_ref(), &mut states).await;

        assert!(result.is_err(), "{identity} must be rejected");
        assert_eq!(states, before, "key mismatches must be atomic");
    }

    for identity in [
        "__mock_hydrate_state_missing_key__",
        "__mock_hydrate_state_extra_key__",
    ] {
        let id = ResourceId::with_provider_identity("mock", "test.resource", identity, None);
        let update_id =
            ResourceId::with_provider_identity("mock", "test.resource", "would-update", None);
        let mut states = HashMap::from([
            (
                id.clone(),
                State::existing(
                    id,
                    HashMap::from([(
                        "original".to_string(),
                        Value::Concrete(ConcreteValue::String("value".into())),
                    )]),
                ),
            ),
            (
                update_id.clone(),
                State::existing(update_id.clone(), HashMap::new()),
            ),
        ]);
        let before = states.clone();
        let saved_attrs = lifted_saved_attrs([(
            update_id,
            HashMap::from([(
                "__mock_hydrate_read_state__".to_string(),
                Value::Concrete(ConcreteValue::String("would-change".into())),
            )]),
        )]);
        let result = hydrate_provider_states(
            normalizer.as_ref(),
            &mut states,
            saved_attrs.as_provider_saved_attrs(),
        )
        .await;

        assert!(result.is_err(), "{identity} must be rejected");
        assert_eq!(states, before, "key mismatches must be atomic");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_hydrate_read_state_round_trips_structural_redacted_secret() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("wasm-wire-secret".into()),
    ))));
    let persisted = PersistedValue::encode_provider_value(&secret, None)
        .expect("serialize test secret")
        .as_json()
        .clone();
    let digest = persisted[carina_core::value::PERSISTED_SECRET_KEY]
        .as_str()
        .expect("structural secret digest")
        .to_string();
    let expected_hash = RedactedSecretHash::from_encoded_digest(&digest)
        .expect("parse generated redacted-secret digest");
    let persisted_secret = Value::Deferred(DeferredValue::RedactedSecret {
        hash: expected_hash.clone(),
    });
    let id = ResourceId::with_provider_identity("mock", "test.resource", "secret-state", None);
    let mut states = HashMap::from([(id.clone(), State::existing(id.clone(), HashMap::new()))]);
    let saved_attrs = lifted_saved_attrs([(
        id.clone(),
        HashMap::from([("__mock_hydrate_read_state__".to_string(), persisted_secret)]),
    )]);
    hydrate_provider_states(
        normalizer.as_ref(),
        &mut states,
        saved_attrs.as_provider_saved_attrs(),
    )
    .await
    .expect("persisted secret must cross the typed guest wire");

    assert!(matches!(
        states[&id].attributes.get("__mock_hydrated_read_state__"),
        Some(Value::Deferred(DeferredValue::RedactedSecret { hash: loaded }))
            if loaded == &expected_hash
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_hydrate_keeps_marker_strings_plain_top_level_and_nested() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");
    let (redacted, marker) = redacted_secret_and_legacy_marker("wasm-marker-collision");
    let plain = Value::Concrete(ConcreteValue::String(marker.clone().into()));
    let top_plain_id =
        ResourceId::with_provider_identity("mock", "test.resource", "top-plain", None);
    let top_secret_id =
        ResourceId::with_provider_identity("mock", "test.resource", "top-secret", None);
    let nested_id = ResourceId::with_provider_identity("mock", "test.resource", "nested", None);
    let mut states = HashMap::from([
        (
            top_plain_id.clone(),
            State::existing(top_plain_id.clone(), HashMap::new()),
        ),
        (
            top_secret_id.clone(),
            State::existing(top_secret_id.clone(), HashMap::new()),
        ),
        (
            nested_id.clone(),
            State::existing(nested_id.clone(), HashMap::new()),
        ),
    ]);
    let nested = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
        ("plain".to_string(), plain.clone()),
        ("typed".to_string(), redacted.clone()),
        (
            "items".to_string(),
            Value::Concrete(ConcreteValue::List(vec![plain.clone(), redacted.clone()])),
        ),
    ])));
    let saved_attrs = lifted_saved_attrs([
        (
            top_plain_id.clone(),
            HashMap::from([("__mock_hydrate_read_state__".to_string(), plain.clone())]),
        ),
        (
            top_secret_id.clone(),
            HashMap::from([("__mock_hydrate_read_state__".to_string(), redacted.clone())]),
        ),
        (
            nested_id.clone(),
            HashMap::from([("__mock_hydrate_read_state__".to_string(), nested.clone())]),
        ),
    ]);
    hydrate_provider_states(
        normalizer.as_ref(),
        &mut states,
        saved_attrs.as_provider_saved_attrs(),
    )
    .await
    .expect("hydrate persisted strings and typed secrets");

    assert_eq!(
        states[&top_plain_id]
            .attributes
            .get("__mock_hydrated_read_state__"),
        Some(&plain),
    );
    assert_eq!(
        states[&top_secret_id]
            .attributes
            .get("__mock_hydrated_read_state__"),
        Some(&redacted),
    );
    assert_eq!(
        states[&nested_id]
            .attributes
            .get("__mock_hydrated_read_state__"),
        Some(&nested),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_hydrate_read_state_write_only_legacy_secret_plans_no_change() {
    use argon2::Argon2;

    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "legacy-write-only", None);
    let plaintext = "wasm-write-only-legacy";
    let material = serde_json::to_vec(&serde_json::Value::String(plaintext.to_string()))
        .expect("serialize legacy plaintext");
    let salt = b"carina:mock.test.resource:legacy-write-only:__mock_write_only_secret__";
    let mut output = [0_u8; 32];
    Argon2::default()
        .hash_password_into(&material, salt, &mut output)
        .expect("hash legacy write-only fixture");
    let encoded = migrate_legacy_state_attribute(
        serde_json::json!(format!("_secret:argon2:{}", hex::encode(output))),
        "mock.test.resource",
        "legacy-write-only",
        "__mock_write_only_secret__",
    );
    let persisted: PersistedValue =
        serde_json::from_value(encoded).expect("migration emits valid persisted JSON");
    let typed = persisted.decode().expect("migration emits a typed secret");
    let saved_attrs = lifted_saved_attrs([(
        id.clone(),
        HashMap::from([("__mock_write_only_secret__".to_string(), typed)]),
    )]);
    let mut states = HashMap::from([(id.clone(), State::existing(id.clone(), HashMap::new()))]);
    hydrate_provider_states(
        normalizer.as_ref(),
        &mut states,
        saved_attrs.as_provider_saved_attrs(),
    )
    .await
    .expect("hydrate write-only legacy secret");

    let Some(Value::Deferred(DeferredValue::RedactedSecret { hash })) =
        states[&id].attributes.get("__mock_write_only_secret__")
    else {
        panic!("hydrated write-only legacy secret must remain typed")
    };
    assert!(
        hash.verify(&material),
        "the returned digest must retain the saved row's origin without a previous current-state leaf",
    );

    let desired = Resource::from_id(id.clone()).with_attribute(
        "__mock_write_only_secret__",
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String(plaintext.into()),
        )))),
    );
    let diff = carina_core::differ::diff(
        &ResolvedResource::new(desired),
        &states[&id],
        &saved_attrs,
        None,
        None,
    );
    assert!(
        !diff.is_change(),
        "refresh hydration of an unchanged write-only legacy secret must not plan an update: {diff:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_normalize_state_trap_is_an_error_and_does_not_mutate_state() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let attributes = indexmap::IndexMap::new();
    let provider = factory
        .create_provider(None, &ready_config(&factory, &attributes))
        .await
        .expect("provider should initialize");
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &attributes))
        .await
        .expect("normalizer should initialize");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "__mock_normalize_state_trap__",
        None,
    );
    let mut states = HashMap::from([(
        id.clone(),
        State::existing(
            id,
            HashMap::from([(
                "original".to_string(),
                Value::Concrete(ConcreteValue::String("value".into())),
            )]),
        ),
    )]);
    let before = states.clone();

    let error = normalize_provider_states(normalizer.as_ref(), &mut states)
        .await
        .expect_err("a guest normalizer trap must become a normalizer error");

    assert!(error.to_string().contains("normalize_state"));
    assert_eq!(states, before, "guest traps must be atomic");

    let create_id = ResourceId::with_provider_identity("mock", "test.resource", "after-trap", None);
    let resource = Resource::with_provider("mock", "test.resource", "after-trap", None);
    let error = provider
        .create_for_test(&create_id, create_request_for_test(resource).await)
        .await
        .expect_err("the shared provider instance must remain poisoned after a normalizer trap");
    assert_eq!(
        error.to_string(),
        "create failed before it could be tracked"
    );
    let rendered = std::error::Error::source(&error)
        .expect("classified create failure retains the poisoned provider error")
        .to_string();

    assert!(
        rendered.contains("provider instance unusable after trap in normalize_state"),
        "the follow-up CRUD error must name the normalizer trap: {rendered}"
    );
    assert!(
        !rendered.contains("WASM guest trapped: create call failed")
            && !rendered.contains("cannot enter component instance"),
        "the follow-up CRUD error must not misattribute the poisoned instance: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_crud_trap_poisons_instance_with_original_operation() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let trap_id =
        ResourceId::with_provider_identity("mock", "test.resource", "__mock_create_trap__", None);
    let trap_resource =
        Resource::with_provider("mock", "test.resource", "__mock_create_trap__", None);

    let initial_error = provider
        .create_for_test(&trap_id, create_request_for_test(trap_resource).await)
        .await
        .expect_err("the mock create hook must trap");
    assert_eq!(
        initial_error.to_string(),
        "create failed before it could be tracked"
    );
    let initial_source = std::error::Error::source(&initial_error)
        .expect("classified create failure retains the trap")
        .to_string();
    assert!(
        initial_source.contains("WASM guest trapped: create call failed"),
        "the initial error source must name the trapping operation: {initial_source}"
    );

    let followup_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-create-trap", None);
    let followup_error = provider
        .read(&followup_id, &mock_identifier(), ReadRequest)
        .await
        .expect_err("the shared instance must remain poisoned after a CRUD trap");
    let rendered = followup_error.to_string();

    assert!(
        rendered.contains("provider instance unusable after trap in create")
            && rendered.contains("wasm `unreachable` instruction executed"),
        "the follow-up error must name the original operation and typed trap cause: {rendered}"
    );
    assert!(
        !rendered.contains("WASM guest trapped: read call failed")
            && !rendered.contains("cannot enter component instance")
            && !rendered.contains("wasm backtrace"),
        "the follow-up error must not be attributed to the later read: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_provider_returned_error_does_not_poison_instance() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let error_id =
        ResourceId::with_provider_identity("mock", "test.resource", "__mock_create_error__", None);
    let error_resource =
        Resource::with_provider("mock", "test.resource", "__mock_create_error__", None);

    let error = provider
        .create_for_test(&error_id, create_request_for_test(error_resource).await)
        .await
        .expect_err("the mock create hook must return a provider error");
    assert_eq!(
        error.to_string(),
        "create failed before it could be tracked"
    );
    assert!(
        std::error::Error::source(&error)
            .expect("classified create failure retains the guest provider error")
            .to_string()
            .contains("intentional mock create error")
    );

    let followup_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-create-error", None);
    let state = provider
        .read(&followup_id, &mock_identifier(), ReadRequest)
        .await
        .expect("a provider-returned error must not poison the shared instance")
        .into_state();
    assert!(!state.exists);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_provider_normalizer_errors_are_structured_and_do_not_poison_instance() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let attributes = indexmap::IndexMap::new();
    let provider = factory
        .create_provider(None, &ready_config(&factory, &attributes))
        .await
        .expect("provider should initialize");
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &attributes))
        .await
        .expect("normalizer should initialize");

    let desired_id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "__mock_normalize_desired_error__",
        None,
    );
    let mut desired_resources = vec![Resource::with_provider(
        "mock",
        "test.resource",
        "__mock_normalize_desired_error__",
        None,
    )];
    desired_resources[0].set_attr(
        "peer",
        Value::resource_ref("peer", "identifier", Vec::new()),
    );
    let desired_before = desired_resources.clone();
    let error = normalizer
        .normalize_desired(&mut desired_resources)
        .await
        .expect_err("a guest normalize_desired error must cross the structured error channel");
    assert_eq!(error.variant_name(), "internal");
    assert_eq!(
        error.detail().context(),
        "intentional mock normalize_desired error"
    );
    assert_eq!(
        error.detail().resource_id.as_deref(),
        Some(&desired_id),
        "the guest resource id must survive the WIT conversion"
    );
    assert_eq!(error.detail().provider_name.as_deref(), Some("mock"));
    assert_eq!(
        desired_resources, desired_before,
        "provider normalizer errors must be atomic and restore stripped attributes"
    );

    let mut recovery_resources = vec![Resource::with_provider(
        "mock",
        "test.resource",
        "after-normalize-desired-error",
        None,
    )];
    normalizer
        .normalize_desired(&mut recovery_resources)
        .await
        .expect("a provider-returned normalize_desired error must not poison the shared instance");

    let id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "__mock_normalize_state_error__",
        None,
    );
    let mut states = HashMap::from([(
        id.clone(),
        State::existing(
            id.clone(),
            HashMap::from([(
                "original".to_string(),
                Value::Concrete(ConcreteValue::String("value".into())),
            )]),
        ),
    )]);
    let before = states.clone();

    let error = normalize_provider_states(normalizer.as_ref(), &mut states)
        .await
        .expect_err("a guest normalize_state error must cross the structured error channel");

    assert_eq!(error.variant_name(), "internal");
    assert_eq!(
        error.detail().context(),
        "intentional mock normalize_state error"
    );
    assert_eq!(error.detail().resource_id.as_deref(), Some(&id));
    assert_eq!(states, before, "provider normalizer errors must be atomic");

    normalizer
        .normalize_desired(&mut recovery_resources)
        .await
        .expect("a provider-returned normalize_state error must not poison the shared instance");

    let hydrate_id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "__mock_hydrate_state_error__",
        None,
    );
    let mut hydrate_states = HashMap::from([(
        hydrate_id.clone(),
        State::existing(
            hydrate_id.clone(),
            HashMap::from([(
                "original".to_string(),
                Value::Concrete(ConcreteValue::String("value".into())),
            )]),
        ),
    )]);
    let hydrate_before = hydrate_states.clone();
    let error =
        hydrate_provider_states(normalizer.as_ref(), &mut hydrate_states, &SavedAttrs::new())
            .await
            .expect_err("a guest hydrate_read_state error must cross the structured error channel");
    assert_eq!(error.variant_name(), "internal");
    assert_eq!(
        error.detail().context(),
        "intentional mock hydrate_read_state error"
    );
    assert_eq!(error.detail().resource_id.as_deref(), Some(&hydrate_id));
    assert_eq!(
        hydrate_states, hydrate_before,
        "provider hydration errors must be atomic"
    );

    normalizer
        .normalize_desired(&mut recovery_resources)
        .await
        .expect("a provider-returned hydrate_read_state error must not poison the shared instance");

    let read_id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "after-normalizer-errors",
        None,
    );
    let state = provider
        .read(&read_id, &mock_identifier(), ReadRequest)
        .await
        .expect("provider-returned normalizer errors must not poison a later CRUD call")
        .into_state();
    assert!(!state.exists);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_normalizer_rejects_nested_non_finite_float_without_mutating_resource() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");

    let mut resource = Resource::with_provider("mock", "test.resource", "non-finite", None);
    resource.attributes.insert(
        "values".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Float(f64::INFINITY),
        )])),
    );
    let mut resources = vec![resource];
    let before = resources.clone();

    let error = normalizer
        .normalize_desired(&mut resources)
        .await
        .expect_err("nested non-finite floats must be provider errors, not panics");

    assert!(error.to_string().contains("non-finite float"));
    assert!(error.detail().cause().is_some());
    assert_eq!(resources, before, "failed encoding must be atomic");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_normalize_state_rejects_nested_non_finite_float_without_mutation() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "bad-state", None);
    let mut states = HashMap::from([(
        id.clone(),
        State::existing(
            id,
            HashMap::from([(
                "values".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::Float(f64::INFINITY),
                )])),
            )]),
        ),
    )]);
    let before = states.clone();

    let error = normalize_provider_states(normalizer.as_ref(), &mut states)
        .await
        .expect_err("non-finite state values must fail boundary encoding");

    assert!(error.to_string().contains("non-finite float"));
    assert_eq!(states, before, "failed encoding must be atomic");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_hydrate_rejects_non_finite_saved_attribute_without_mutation() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "bad-saved", None);
    let mut states = HashMap::from([(id.clone(), State::existing(id.clone(), HashMap::new()))]);
    let before = states.clone();
    let saved_attrs = lifted_saved_attrs([(
        id,
        HashMap::from([(
            "values".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Float(f64::NEG_INFINITY),
            )])),
        )]),
    )]);
    let error = hydrate_provider_states(
        normalizer.as_ref(),
        &mut states,
        saved_attrs.as_provider_saved_attrs(),
    )
    .await
    .expect_err("non-finite saved attributes must fail boundary encoding");

    assert!(error.to_string().contains("non-finite float"));
    assert_eq!(states, before, "failed encoding must be atomic");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_normalize_desired_preserves_secret() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");

    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("normalize-secret-plaintext".into()),
    ))));
    let mut resource = Resource::with_provider("mock", "test.resource", "secret-normalize", None);
    resource
        .attributes
        .insert("api_key".to_string(), secret.clone());
    let mut resources = vec![resource];

    normalizer.normalize_desired(&mut resources).await.unwrap();

    assert_eq!(resources[0].get_attr("api_key"), Some(&secret));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_hydrate_read_state_preserves_host_ids() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");

    let resolved = ResourceId::with_provider_identity("mock", "test.resource", "resolved", None);
    let first_pending = ResourceId::pending_with_provider("mock", "test.resource", None);
    let second_pending = ResourceId::pending_with_provider("mock", "test.resource", None);
    let ids = [resolved, first_pending, second_pending];

    let mut states = ids
        .iter()
        .cloned()
        .map(|id| {
            let state = State::existing(id.clone(), HashMap::new());
            (id, state)
        })
        .collect::<HashMap<_, _>>();
    let saved_attrs = lifted_saved_attrs(ids.iter().enumerate().map(|(index, id)| {
        (
            id.clone(),
            HashMap::from([(
                "__mock_hydrate_read_state__".to_string(),
                Value::Concrete(ConcreteValue::String(format!("marker-{index}").into())),
            )]),
        )
    }));
    hydrate_provider_states(
        normalizer.as_ref(),
        &mut states,
        saved_attrs.as_provider_saved_attrs(),
    )
    .await
    .unwrap();

    assert_eq!(states.len(), ids.len());
    for (index, id) in ids.iter().enumerate() {
        let state = &states[id];
        assert_eq!(&state.id, id, "the original host ResourceId must survive");
        assert_eq!(
            state.attributes.get("__mock_hydrated_read_state__"),
            Some(&Value::Concrete(ConcreteValue::String(
                format!("marker-{index}").into()
            ))),
            "each state must receive the saved attrs associated with its host ID"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_read_data_source_dispatches_override() {
    // Regression test for carina-rs/carina#1677: the plugin boundary must
    // route `read_data_source` through to the guest's implementation so
    // providers can see user-supplied input attributes.
    //
    // The mock provider's `read_data_source` echoes input attributes back
    // into state plus a sentinel `__mock_read_data_source__` flag. If
    // that flag shows up, the WASM bridge forwarded the call correctly.
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");

    let mut resource = DataSource::with_provider("mock", "test.data_source", "example", None);
    resource.attributes = indexmap::IndexMap::from([
        (
            "identity_store_id".into(),
            Value::Concrete(ConcreteValue::String("d-1234567890".into())),
        ),
        (
            "user_name".into(),
            Value::Concrete(ConcreteValue::String("alice@example.com".into())),
        ),
    ]);
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let module_gate = carina_core::executor::ModuleConstraintGate::new(&[]);
    let schemas = carina_core::schema::SchemaRegistry::new();
    let preparation = carina_core::executor::ProviderPreparationContext::new(
        &bindings,
        &module_gate,
        &[],
        &carina_core::provider::NoopNormalizer,
        &[],
        &schemas,
    );
    let resource =
        carina_core::executor::prepare_provider_ready_data_source(resource, &preparation)
            .expect("data-source request should pass the checked host boundary");

    let state = provider
        .read_data_source(&resource)
        .await
        .expect("read_data_source should dispatch to the plugin override");

    assert!(state.exists, "state should be marked as existing");
    assert_eq!(
        state.attributes.get("__mock_read_data_source__"),
        Some(&Value::Concrete(ConcreteValue::Bool(true))),
        "sentinel attribute must be present — proves the plugin override ran"
    );
    assert_eq!(
        state.attributes.get("identity_store_id"),
        Some(&Value::Concrete(ConcreteValue::String(
            "d-1234567890".into()
        ))),
        "input attributes must cross the WASM boundary unchanged"
    );
    assert_eq!(
        state.attributes.get("user_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "alice@example.com".into()
        ))),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_required_permissions_dispatches_through_wit() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "example", None);

    assert_eq!(
        provider
            .required_permissions(&id, PlanOp::Create)
            .expect("required_permissions should succeed"),
        Vec::<String>::new()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_required_permissions_trap_is_not_an_empty_permission_set() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "__mock_required_permissions_trap__",
        None,
    );

    let error = provider
        .required_permissions(&id, PlanOp::Create)
        .expect_err("a guest trap must be returned to the caller");

    assert!(
        error.to_string().contains("required_permissions"),
        "the error must name the trapping export: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_invalid_satisfier_hint_is_not_silently_dropped() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "__mock_invalid_satisfier_hint__",
        None,
    );

    let error = provider
        .satisfier_hint(
            &id,
            &carina_core::wait::predicate::AttrPath::single("status"),
        )
        .expect_err("an invalid guest pattern must be returned to the caller");

    assert!(
        error.to_string().contains("satisfier_hint"),
        "the error must name the malformed export: {error}"
    );
}
