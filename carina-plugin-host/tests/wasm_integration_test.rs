//! Integration tests for the general mock and the isolated wasi:http probe guests.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carina_core::effect::PlanOp;
use carina_core::executor::{AcceptedRequest, complete_create_for_writeback};
use carina_core::provider::{
    AppliedIdentifier, BoxFuture, CreateFailure, CreateFailureKind, CreateObservation,
    CreateOutcome, CreateOutcomeKind, CreateRequest, DeleteRequest, InFlightCreate, Provider,
    ProviderDirectiveSchema, ProviderError, ProviderErrorKind, ProviderFactory,
    ProviderReadyConfig, RawSavedAttrs, ReadRequest, ResumeCreateRequest, ResumeFailure,
    ResumeFailureKind, ResumeOutcome, ResumeOutcomeKind, SavedAttrs, Sent, SentRecord,
    UnobservedReason, UpdateRequest, hydrate_provider_states, normalize_provider_states,
};
use carina_core::resource::{
    ApplyTimeUnknown, ConcreteValue, DataSource, DeferredValue, EnumValueResolver,
    ProviderResource, RawEnumIdentifier, RedactedSecretHash, ResolvedResource, Resource,
    ResourceId, State, Value, absolute_binding_address_for_test,
};
use carina_core::schema::{CompiledAttributeType as AttributeType, TypeIdentity};
use carina_core::value::PersistedValue;
use carina_plugin_host::{
    AwsCredentialError, AwsCredentialProvisioner, AwsCredentialSource, AwsCredentials,
    GuestCallBudget, WasmProviderFactory, WasmProviderLoadError,
};
use carina_provider_resolver::{
    LockFile, ProviderLoadIdentity, ProviderTrustClass, VerifiedEgressDeclaration, VerifiedLoad,
};

fn mock_identity() -> ProviderLoadIdentity {
    ProviderLoadIdentity::for_test("mock", ProviderTrustClass::Untrusted)
}

fn mock_load(bytes: &[u8]) -> VerifiedLoad {
    VerifiedLoad::for_test(bytes, mock_identity())
}

fn mock_http_load(bytes: &[u8]) -> VerifiedLoad {
    mock_http_load_with_class(bytes, ProviderTrustClass::Untrusted)
}

fn mock_http_load_with_class(bytes: &[u8], class: ProviderTrustClass) -> VerifiedLoad {
    VerifiedLoad::for_test(bytes, ProviderLoadIdentity::for_test("mock_http", class))
}

fn mock_http_aws_load_with_class(bytes: &[u8], class: ProviderTrustClass) -> VerifiedLoad {
    VerifiedLoad::for_test(bytes, ProviderLoadIdentity::for_test("aws", class))
}

fn mock_identifier() -> AppliedIdentifier {
    AppliedIdentifier::new("mock-id").expect("the mock identifier is non-empty")
}

fn assert_minted_mock_identifier(identifier: &str) {
    assert!(
        identifier.starts_with("mock-v1:"),
        "expected a uniquely minted mock identifier, got {identifier}"
    );
}

fn delete_request_without_schema() -> DeleteRequest {
    DeleteRequest::resolve(
        &carina_core::resource::Directives::default(),
        ProviderDirectiveSchema::Unavailable,
    )
}

fn provider_resources(resources: Vec<Resource>) -> Vec<ProviderResource> {
    resources.into_iter().map(ProviderResource::from).collect()
}

fn root_resource_ref(binding: &str, attribute: &str) -> Value {
    carina_core::resource::AuthoredValue::resource_ref(binding, attribute, Vec::new())
        .resolve_root_for_test()
        .into_expanded()
        .expect("root resource reference contains no placeholders")
}

fn create_state_for_writeback(outcome: CreateOutcome) -> State {
    let request = AcceptedRequest::create_for_test();
    complete_create_for_writeback(outcome, request).0
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
    let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
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
        let (create, cancellation) = carina_core::executor::dispatch_create(self, id, request);
        Box::pin(async move {
            let _cancellation = cancellation;
            create.await
        })
    }
}

async fn update_request_for_test(
    resource: Resource,
    from: State,
    changed_attributes: &[String],
) -> UpdateRequest {
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
    let mut schemas = carina_core::schema::SchemaRegistry::new();
    schemas.insert(
        resource.id.provider.clone(),
        carina_core::schema::PatternResourceSchema::new(resource.id.resource_type.clone())
            .attribute(
                carina_core::schema::PatternAttributeSchema::optional(
                    "reassert_token",
                    carina_core::schema::PatternAttributeType::string(),
                )
                .write_only()
                .build(),
            )
            .validate()
            .expect("test schema must be valid"),
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

fn http_wasm_path() -> Option<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    for name in &[
        "carina_provider_mock_http.wasm",
        "carina-provider-mock-http.wasm",
    ] {
        let path = workspace_root.join("target/wasm32-wasip2/debug").join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn aws_http_wasm_path() -> Option<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    for name in &[
        "carina_provider_mock_http_aws.wasm",
        "carina-provider-mock-http-aws.wasm",
    ] {
        let path = workspace_root.join("target/wasm32-wasip2/debug").join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn aws_http_unused_wasm_path() -> Option<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    for name in &[
        "carina_provider_mock_http_aws_unused.wasm",
        "carina-provider-mock-http-aws-unused.wasm",
    ] {
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

macro_rules! skip_if_no_http_wasm {
    () => {
        match http_wasm_path() {
            Some(p) => p,
            None => {
                eprintln!(
                    "SKIP: HTTP probe WASM binary not found. Build with: \
                     cargo build -p carina-provider-mock-http --target wasm32-wasip2"
                );
                return;
            }
        }
    };
}

macro_rules! skip_if_no_aws_http_wasm {
    () => {
        match aws_http_wasm_path() {
            Some(p) => p,
            None => {
                eprintln!(
                    "SKIP: AWS HTTP probe WASM binary not found. Build with: \
                     cargo build -p carina-provider-mock-http --target wasm32-wasip2"
                );
                return;
            }
        }
    };
}

macro_rules! skip_if_no_aws_http_unused_wasm {
    () => {
        match aws_http_unused_wasm_path() {
            Some(p) => p,
            None => {
                eprintln!(
                    "SKIP: unused-credentials AWS HTTP probe WASM binary not found. Build with: \
                     cargo build -p carina-provider-mock-http --bin \
                     carina-provider-mock-http-aws-unused --target wasm32-wasip2"
                );
                return;
            }
        }
    };
}

struct FixedAwsCredentialSource(Result<AwsCredentials, AwsCredentialError>);

impl AwsCredentialSource for FixedAwsCredentialSource {
    fn current(&self) -> BoxFuture<'_, Result<AwsCredentials, AwsCredentialError>> {
        let result = self.0.clone();
        Box::pin(async move { result })
    }
}

struct SequenceAwsCredentialSource {
    results: Mutex<VecDeque<Result<AwsCredentials, AwsCredentialError>>>,
}

impl SequenceAwsCredentialSource {
    fn new(results: impl IntoIterator<Item = Result<AwsCredentials, AwsCredentialError>>) -> Self {
        Self {
            results: Mutex::new(results.into_iter().collect()),
        }
    }
}

impl AwsCredentialSource for SequenceAwsCredentialSource {
    fn current(&self) -> BoxFuture<'_, Result<AwsCredentials, AwsCredentialError>> {
        let result = self
            .results
            .lock()
            .expect("credential response queue must not be poisoned")
            .pop_front()
            .expect("credential response queue must contain one result per guest call");
        Box::pin(async move { result })
    }
}

struct FixedAwsCredentialProvisioner(Result<Arc<dyn AwsCredentialSource>, AwsCredentialError>);

impl AwsCredentialProvisioner for FixedAwsCredentialProvisioner {
    fn provision(&self) -> BoxFuture<'_, Result<Arc<dyn AwsCredentialSource>, AwsCredentialError>> {
        let result = self.0.clone();
        Box::pin(async move { result })
    }
}

struct CountingAwsCredentialProvisioner {
    calls: Arc<AtomicUsize>,
}

impl AwsCredentialProvisioner for CountingAwsCredentialProvisioner {
    fn provision(&self) -> BoxFuture<'_, Result<Arc<dyn AwsCredentialSource>, AwsCredentialError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(AwsCredentialError::ProviderFailed(
                "non-AWS loads must not provision credentials".to_string(),
            ))
        })
    }
}

fn test_aws_credentials(access_key_id: &str) -> AwsCredentials {
    AwsCredentials::new(
        access_key_id,
        "host-secret-sentinel",
        Some("host-session-sentinel".to_string()),
        None,
    )
}

fn default_aws_provisioner() -> FixedAwsCredentialProvisioner {
    FixedAwsCredentialProvisioner(Ok(Arc::new(FixedAwsCredentialSource(Ok(
        test_aws_credentials("AKIADEFAULT"),
    )))))
}

/// Build a `WasmProviderFactory` using a per-test temporary cache directory.
///
/// Tests in this binary run in parallel; if they all shared
/// `WasmProviderFactory::from_bytes()`'s configured compiled-WASM cache, concurrent
/// precompile runs race on the same `.cwasm` path and one test can observe
/// a partially-written file (`"failed to load code for …"`). Each test gets
/// its own cache dir via this helper to eliminate that race.
async fn load_factory(wasm: &std::path::Path) -> (WasmProviderFactory, tempfile::TempDir) {
    let cache = tempfile::tempdir().expect("Failed to create cache tempdir");
    let wasm_bytes = std::fs::read(wasm).expect("Failed to read WASM provider");
    let load = mock_load(&wasm_bytes);
    let locked_egress = explicit_test_egress(&load, cache.path());
    let aws = default_aws_provisioner();
    let factory = WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
        load,
        cache.path(),
        locked_egress.as_ref(),
        &aws,
    )
    .await
    .expect("Failed to load WASM provider");
    (factory, cache)
}

async fn load_aws_http_factory(
    wasm: &std::path::Path,
    credentials: Arc<dyn AwsCredentialSource>,
) -> (WasmProviderFactory, tempfile::TempDir) {
    let cache = tempfile::tempdir().expect("Failed to create cache tempdir");
    let wasm_bytes = std::fs::read(wasm).expect("Failed to read AWS HTTP probe WASM provider");
    let load = mock_http_aws_load_with_class(&wasm_bytes, ProviderTrustClass::Aws);
    let locked_egress = explicit_test_egress(&load, cache.path());
    let aws = FixedAwsCredentialProvisioner(Ok(credentials));
    let factory = WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
        load,
        cache.path(),
        locked_egress.as_ref(),
        &aws,
    )
    .await
    .expect("Failed to load AWS HTTP probe WASM provider");
    (factory, cache)
}

async fn load_http_factory(wasm: &std::path::Path) -> (WasmProviderFactory, tempfile::TempDir) {
    load_http_factory_with_class(wasm, ProviderTrustClass::Untrusted).await
}

async fn load_http_factory_with_class(
    wasm: &std::path::Path,
    class: ProviderTrustClass,
) -> (WasmProviderFactory, tempfile::TempDir) {
    load_http_factory_with_name_and_class(wasm, "mock_http", class).await
}

async fn load_http_factory_with_name_and_class(
    wasm: &std::path::Path,
    name: &str,
    class: ProviderTrustClass,
) -> (WasmProviderFactory, tempfile::TempDir) {
    let cache = tempfile::tempdir().expect("Failed to create cache tempdir");
    let wasm_bytes = std::fs::read(wasm).expect("Failed to read HTTP probe WASM provider");
    let load = VerifiedLoad::for_test(&wasm_bytes, ProviderLoadIdentity::for_test(name, class));
    let locked_egress = explicit_test_egress(&load, cache.path());
    let aws = default_aws_provisioner();
    let factory = WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
        load,
        cache.path(),
        locked_egress.as_ref(),
        &aws,
    )
    .await
    .expect("Failed to load HTTP probe WASM provider");
    (factory, cache)
}

fn explicit_test_egress(
    load: &carina_provider_resolver::VerifiedLoad,
    lock_dir: &std::path::Path,
) -> Option<carina_provider_resolver::LockedEgress> {
    let declaration = VerifiedEgressDeclaration::read(load)
        .expect("test provider must carry a valid or empty egress declaration");
    let canonical_source = declaration.source().to_string();
    let source = canonical_source
        .strip_prefix("file://")
        .map(|path| format!("file://{}", lock_dir.join(path).display()))
        .unwrap_or(canonical_source);
    let sha256 = declaration.sha256().as_str();
    let hosts = declaration
        .patterns()
        .iter()
        .map(|pattern| format!("    {:?},", pattern.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    if declaration.patterns().is_empty() {
        return None;
    }
    let lock = format!(
        "version = 5\n\n[[provider]]\nname = \"ignored-consent-name\"\nsource = {source:?}\nmode = \"file\"\nsha256 = {sha256:?}\n\n[provider.egress]\nconsent = \"explicit\"\nhosts = [\n{hosts}\n]\n"
    );
    std::fs::write(lock_dir.join("carina-providers.lock"), lock).expect("write test egress lock");
    let (lock, _migration) = LockFile::load(lock_dir)
        .expect("load test egress lock")
        .expect("test egress lock exists")
        .into_parts();
    let locked = lock
        .locked_egress(&declaration, lock_dir)
        .expect("test lock has egress consent for the canonical source");
    Some(locked)
}

async fn load_schema_http_factory(
    wasm: &std::path::Path,
) -> (WasmProviderFactory, tempfile::TempDir) {
    let cache = tempfile::tempdir().expect("Failed to create cache tempdir");
    let wasm_bytes = std::fs::read(wasm).expect("Failed to read HTTP probe WASM provider");
    let factory =
        WasmProviderFactory::from_bytes_with_cache_dir(mock_http_load(&wasm_bytes), cache.path())
            .await
            .expect("Failed to load HTTP probe WASM provider");
    (factory, cache)
}

fn aws_credential_probe_data_source() -> carina_core::provider::ProviderReadyDataSource {
    let resource = DataSource::with_provider("aws", "test.aws_credentials", "credentials", None);
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = carina_core::schema::SchemaRegistry::new();
    let preparation = carina_core::executor::ProviderPreparationContext::new(
        &bindings,
        &module_gate,
        &[],
        &carina_core::provider::NoopNormalizer,
        &[],
        &schemas,
    );
    carina_core::executor::prepare_provider_ready_data_source(resource, &preparation)
        .expect("credential probe data source should pass the checked host boundary")
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
    resource_with_provider_string_attribute("mock", resource_type, identity, attribute, value)
}

fn resource_with_provider_string_attribute(
    provider: &str,
    resource_type: &str,
    identity: &str,
    attribute: &str,
    value: &str,
) -> Resource {
    let identity = carina_core::resource::ResourceIdentity::try_from(identity.to_string())
        .expect("test identity must be non-empty");
    let mut resource = Resource::with_provider(provider, resource_type, identity, None);
    resource.attributes.insert(
        attribute.into(),
        Value::Concrete(ConcreteValue::String(value.to_string().into())),
    );
    resource
}

fn http_send_phase_resource(identity: &str, url: &str, method: &str, body: &str) -> Resource {
    http_send_phase_resource_with_provider("mock_http", identity, url, method, body)
}

fn http_send_phase_resource_with_provider(
    provider: &str,
    identity: &str,
    url: &str,
    method: &str,
    body: &str,
) -> Resource {
    let mut resource =
        resource_with_provider_string_attribute(provider, "test.send_phase", identity, "url", url);
    resource.attributes.insert(
        "method".to_string(),
        Value::Concrete(ConcreteValue::String(method.to_string().into())),
    );
    resource.attributes.insert(
        "body".to_string(),
        Value::Concrete(ConcreteValue::String(body.to_string().into())),
    );
    resource
}

fn with_http_create_behavior(mut resource: Resource, behavior: &str) -> Resource {
    resource.attributes.insert(
        "create_behavior".to_string(),
        Value::Concrete(ConcreteValue::String(behavior.to_string().into())),
    );
    resource
}

fn http_probe_body() -> String {
    // Exceed the two-chunk host body buffer so the closed-port case can close
    // the receiver while the guest is still writing, exercising the
    // future-error recovery path. The denied-host case fails synchronously in
    // outgoing_handler::handle instead.
    "x".repeat(16 * 1024)
}

fn http_probe_outcome(state: &State) -> &str {
    let Some(Value::Concrete(ConcreteValue::String(outcome))) = state.attributes.get("send_phase")
    else {
        panic!("HTTP probe state must contain a string send_phase attribute");
    };
    outcome.expose_plaintext()
}

fn read_http_request_head(stream: &mut impl Read) -> Vec<u8> {
    // Read exactly the head so the early-response server cannot accidentally
    // drain any of the body along with the last header read.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).expect("read request head");
        head.push(byte[0]);
        assert!(head.len() <= 32 * 1024, "request head is too large");
    }
    head
}

fn http_request_content_length(head: &[u8]) -> usize {
    std::str::from_utf8(head)
        .expect("request head is ASCII")
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| {
                value
                    .trim()
                    .parse()
                    .expect("Content-Length is a byte count")
            })
        })
        .expect("the SDK must frame the request with Content-Length")
}

fn spawn_http_ok_server() -> Option<(SocketAddr, std::thread::JoinHandle<usize>)> {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("SKIP: sandbox cannot bind the loopback test server: {error}");
            return None;
        }
    };
    let address = listener
        .local_addr()
        .expect("bound loopback listener has an address");
    listener
        .set_nonblocking(true)
        .expect("set loopback listener nonblocking");
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "server accept timed out");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("server accept failed: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set accepted socket blocking");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server read timeout");
        let head = read_http_request_head(&mut stream);
        let mut body = vec![0; http_request_content_length(&head)];
        stream
            .read_exact(&mut body)
            .expect("read the whole dispatched request body before responding");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("write successful HTTP response");
        head.len() + body.len()
    });
    Some((address, server))
}

fn create_failure_error(failure: CreateFailure) -> ProviderError {
    match failure.into_kind() {
        CreateFailureKind::Untracked(error) => error,
        CreateFailureKind::OutcomeUnknown { unknown: _, error } => error,
        CreateFailureKind::InFlight { pending: _, error } => error,
        CreateFailureKind::Created { created: _, error } => error,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_create_trap_after_dispatched_egress_is_outcome_unknown() {
    let path = skip_if_no_http_wasm!();
    let Some((server_address, server)) = spawn_http_ok_server() else {
        return;
    };
    let (factory, _cache) = load_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", server_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "trap-after-egress",
        None,
    );
    let resource = with_http_create_behavior(
        http_send_phase_resource(
            "trap-after-egress",
            &format!("http://phase-test.invalid:{}/", server_address.port()),
            "POST",
            &http_probe_body(),
        ),
        "trap_after_send",
    );

    let failure = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("the guest must trap after the request completes");
    let bytes_read = server.join().expect("loopback server should finish");

    assert!(bytes_read > 0, "the request must actually leave the guest");
    let CreateFailureKind::OutcomeUnknown { unknown, error } = failure.into_kind() else {
        panic!("a trap after dispatched egress must not be classified untracked")
    };
    assert_eq!(unknown.id(), &id);
    assert!(error.to_string().contains("WASM guest trapped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_create_unencodable_pending_token_after_egress_is_outcome_unknown() {
    let path = skip_if_no_http_wasm!();
    let Some((server_address, server)) = spawn_http_ok_server() else {
        return;
    };
    let (factory, _cache) = load_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", server_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "unencodable-pending-after-egress",
        None,
    );
    let resource = with_http_create_behavior(
        http_send_phase_resource(
            "unencodable-pending-after-egress",
            &format!("http://phase-test.invalid:{}/", server_address.port()),
            "POST",
            &http_probe_body(),
        ),
        "pending_nan_after_send",
    );

    let failure = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("the guest must trap while encoding the NaN continuation token");
    let bytes_read = server.join().expect("loopback server should finish");

    assert!(bytes_read > 0, "the request must actually leave the guest");
    let CreateFailureKind::OutcomeUnknown { unknown, error } = failure.into_kind() else {
        panic!("a continuation encode failure after egress must never be untracked")
    };
    assert_eq!(unknown.id(), &id);
    let rendered = error.to_string();
    assert!(rendered.contains("WASM guest trapped"), "{rendered}");
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_create_trap_without_egress_is_untracked() {
    let path = skip_if_no_http_wasm!();
    let (factory, _cache) = load_http_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "trap-before-egress",
        None,
    );
    let resource = with_http_create_behavior(
        http_send_phase_resource(
            "trap-before-egress",
            "http://phase-test.invalid:1/",
            "POST",
            &http_probe_body(),
        ),
        "trap_before_send",
    );

    let failure = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("the guest must trap before issuing the request");

    let CreateFailureKind::Untracked(error) = failure.into_kind() else {
        panic!("a trap with no dispatched egress must remain untracked")
    };
    assert!(error.to_string().contains("WASM guest trapped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_declared_untracked_remains_untracked_after_egress() {
    let path = skip_if_no_http_wasm!();
    let Some((server_address, server)) = spawn_http_ok_server() else {
        return;
    };
    let (factory, _cache) = load_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", server_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "declared-untracked-after-egress",
        None,
    );
    let resource = with_http_create_behavior(
        http_send_phase_resource(
            "declared-untracked-after-egress",
            &format!("http://phase-test.invalid:{}/", server_address.port()),
            "POST",
            &http_probe_body(),
        ),
        "untracked_after_send",
    );

    let failure = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("the provider must return its declared verdict");
    let bytes_read = server.join().expect("loopback server should finish");

    assert!(bytes_read > 0, "the request must actually leave the guest");
    let CreateFailureKind::Untracked(error) = failure.into_kind() else {
        panic!("a decoded provider-declared untracked verdict must remain authoritative")
    };
    assert!(error.to_string().contains("provider declared untracked"));
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
        carina_core::resource::Current::CreateOutcomeUnknown(unknown) => {
            panic!("resume fixture loaded as outcome unknown: {unknown:?}")
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
    let mut parsed = carina_core::parser::ExpandedInferredFile::default();
    parsed.providers = vec![carina_core::parser::ProviderConfig {
        name: name.to_string(),
        attributes,
        default_tags: indexmap::IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: indexmap::IndexMap::new(),
        binding: None,
    }];
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
        .find(|schema| schema.parts().resource_type == "test.short_budget")
        .and_then(|schema| schema.parts().operation_config)
        .expect("mock provider should expose the short-budget schema");
    assert_eq!(short_budget.create_timeout_secs, Some(2));
    assert_eq!(short_budget.update_timeout_secs, Some(2));
    assert_eq!(short_budget.delete_timeout_secs, Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_only_loads_an_aws_world_without_a_credential_source() {
    let path = skip_if_no_aws_http_wasm!();
    let cache = tempfile::tempdir().expect("create schema-only cache tempdir");
    let wasm_bytes = std::fs::read(path).expect("read AWS HTTP probe WASM provider");

    let factory = WasmProviderFactory::from_bytes_with_cache_dir(
        mock_http_aws_load_with_class(&wasm_bytes, ProviderTrustClass::Aws),
        cache.path(),
    )
    .await
    .expect("schema-only loading must link the failing credential import");

    assert_eq!(factory.name(), "aws");
    assert!(
        factory
            .schemas()
            .iter()
            .any(|schema| schema.parts().resource_type == "test.aws_credentials")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn aws_kind_runtime_rejects_an_http_world_before_provisioning_credentials() {
    let path = skip_if_no_http_wasm!();
    let cache = tempfile::tempdir().expect("create AWS-kind cache tempdir");
    let wasm_bytes = std::fs::read(path).expect("read HTTP-world WASM provider");
    let load = mock_http_load_with_class(&wasm_bytes, ProviderTrustClass::Aws);
    let locked_egress = explicit_test_egress(&load, cache.path());
    let calls = Arc::new(AtomicUsize::new(0));
    let aws = CountingAwsCredentialProvisioner {
        calls: Arc::clone(&calls),
    };

    let result = WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
        load,
        cache.path(),
        locked_egress.as_ref(),
        &aws,
    )
    .await;
    let error = match result {
        Ok(_) => panic!("an AWS-kind runtime must reject a component built with the HTTP world"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        WasmProviderLoadError::AwsCredentialsImportMissing
    ));
    assert_eq!(
        error.to_string(),
        "an AWS-kind provider must be built with the `aws` world (`export_provider!(T, aws, egress: [..])`)"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn aws_world_without_credential_calls_loads_as_aws_kind_at_runtime() {
    let path = skip_if_no_aws_http_unused_wasm!();
    let cache = tempfile::tempdir().expect("create unused-credentials AWS-world cache tempdir");
    let wasm_bytes = std::fs::read(path).expect("read unused-credentials AWS-world WASM provider");
    let load = VerifiedLoad::for_test(
        &wasm_bytes,
        ProviderLoadIdentity::for_test("mock_http_aws_unused", ProviderTrustClass::Aws),
    );
    let locked_egress = explicit_test_egress(&load, cache.path());
    let aws = default_aws_provisioner();

    let factory = WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
        load,
        cache.path(),
        locked_egress.as_ref(),
        &aws,
    )
    .await
    .expect("the AWS world must retain its credential import even when guest code never calls it");

    assert_eq!(factory.name(), "mock_http_aws_unused");
}

#[tokio::test(flavor = "multi_thread")]
async fn aws_world_provision_failure_remains_a_typed_load_error() {
    let path = skip_if_no_aws_http_wasm!();
    let cache = tempfile::tempdir().expect("create AWS-world cache tempdir");
    let wasm_bytes = std::fs::read(path).expect("read AWS-world WASM provider");
    let load = mock_http_aws_load_with_class(&wasm_bytes, ProviderTrustClass::Aws);
    let locked_egress = explicit_test_egress(&load, cache.path());
    let aws = FixedAwsCredentialProvisioner(Err(AwsCredentialError::NotFound(
        "host provisioning failure sentinel".to_string(),
    )));

    let result = WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
        load,
        cache.path(),
        locked_egress.as_ref(),
        &aws,
    )
    .await;

    assert!(matches!(
        result,
        Err(WasmProviderLoadError::AwsCredentials(
            AwsCredentialError::NotFound(ref message)
        )) if message == "host provisioning failure sentinel"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn non_aws_kinds_never_provision_and_cannot_instantiate_an_aws_world() {
    let path = skip_if_no_aws_http_wasm!();
    let wasm_bytes = std::fs::read(path).expect("read AWS HTTP probe WASM provider");
    let calls = Arc::new(AtomicUsize::new(0));
    let aws = CountingAwsCredentialProvisioner {
        calls: Arc::clone(&calls),
    };

    for class in [ProviderTrustClass::GitHub, ProviderTrustClass::Untrusted] {
        let cache = tempfile::tempdir().expect("create non-AWS cache tempdir");
        let load = mock_http_aws_load_with_class(&wasm_bytes, class);
        let locked_egress = explicit_test_egress(&load, cache.path());
        let result = WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
            load,
            cache.path(),
            locked_egress.as_ref(),
            &aws,
        )
        .await;
        let error = match result {
            Ok(_) => panic!("a non-AWS runtime must reject the AWS credential import"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            WasmProviderLoadError::AwsCredentialsRequired
        ));
        assert_eq!(
            error.to_string(),
            "this provider requires AWS credentials, which only an AWS-kind provider can receive"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn aws_credential_import_returns_the_sources_credentials() {
    let path = skip_if_no_aws_http_wasm!();
    let source = Arc::new(FixedAwsCredentialSource(Ok(AwsCredentials::new(
        "AKIAFROMHOST",
        "host-secret-sentinel",
        Some("host-session-sentinel".to_string()),
        None,
    ))));
    let (factory, _cache) = load_aws_http_factory(&path, source).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("AWS credential probe should initialize");

    let state = provider
        .provider()
        .read_data_source(&aws_credential_probe_data_source())
        .await
        .expect("the guest should receive host credentials");

    assert_eq!(
        state.attributes.get("access_key_id"),
        Some(&Value::Concrete(ConcreteValue::String(
            "AKIAFROMHOST".into()
        )))
    );
    assert_eq!(
        state.attributes.get("has_session_token"),
        Some(&Value::Concrete(ConcreteValue::Bool(true)))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn aws_credential_source_error_reaches_the_guest_as_a_wit_error() {
    let path = skip_if_no_aws_http_wasm!();
    let source = Arc::new(FixedAwsCredentialSource(Err(AwsCredentialError::NotFound(
        "credential-source-not-found-sentinel".to_string(),
    ))));
    let (factory, _cache) = load_aws_http_factory(&path, source).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("AWS credential probe should initialize");

    let error = provider
        .provider()
        .read_data_source(&aws_credential_probe_data_source())
        .await
        .expect_err("the guest should surface the typed source failure");

    assert_eq!(error.kind(), ProviderErrorKind::InvalidInput);
    assert!(
        error
            .to_string()
            .contains("credential-source-not-found-sentinel"),
        "unexpected guest error: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_guest_call_asks_the_host_source() {
    let path = skip_if_no_aws_http_wasm!();
    let source = Arc::new(SequenceAwsCredentialSource::new([
        Ok(AwsCredentials::new("AKIAFIRST", "first-secret", None, None)),
        Ok(AwsCredentials::new(
            "AKIASECOND",
            "second-secret",
            None,
            None,
        )),
    ]));
    let (factory, _cache) = load_aws_http_factory(&path, source).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("AWS credential probe should initialize");

    let first = provider
        .provider()
        .read_data_source(&aws_credential_probe_data_source())
        .await
        .expect("first guest credential request should succeed");
    let second = provider
        .provider()
        .read_data_source(&aws_credential_probe_data_source())
        .await
        .expect("second guest credential request should succeed");

    assert_eq!(
        first.attributes.get("access_key_id"),
        Some(&Value::Concrete(ConcreteValue::String("AKIAFIRST".into())))
    );
    assert_eq!(
        second.attributes.get("access_key_id"),
        Some(&Value::Concrete(ConcreteValue::String("AKIASECOND".into())))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn initialized_reference_scope_is_kept_per_provider_instance() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let tokyo_attributes = indexmap::IndexMap::from([(
        "__mock_reference_scope_region__".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    )]);
    let virginia_attributes = indexmap::IndexMap::from([(
        "__mock_reference_scope_region__".to_string(),
        Value::Concrete(ConcreteValue::String("us-east-1".into())),
    )]);

    let _tokyo = factory
        .create_initialized_provider(None, &ready_config(&factory, &tokyo_attributes))
        .await
        .expect("default provider should initialize");
    let _virginia = factory
        .create_initialized_provider(
            Some("virginia"),
            &ready_config(&factory, &virginia_attributes),
        )
        .await
        .expect("named provider should initialize");

    assert_eq!(
        factory.initialized_reference_scope_for_test(None).await,
        Some(
            carina_provider_protocol::types::ReferenceScope::available(
                "mock",
                "ap-northeast-1",
                "test-account",
            )
            .expect("fixture scope is valid"),
        )
    );
    assert_eq!(
        factory
            .initialized_reference_scope_for_test(Some("virginia"))
            .await,
        Some(
            carina_provider_protocol::types::ReferenceScope::available(
                "mock",
                "us-east-1",
                "test-account",
            )
            .expect("fixture scope is valid"),
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_schema_validator_dispatches_through_the_wasm_host_without_an_override() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factories: Arc<Vec<Box<dyn ProviderFactory>>> = Arc::new(vec![Box::new(factory)]);
    let schemas = carina_core::provider::collect_schemas(&factories);
    let schema = schemas
        .get(
            "mock",
            "test.sdk_validator",
            carina_core::schema::SchemaKind::Resource,
        )
        .expect("mock SDK validator schema must cross the host boundary");
    let custom_types =
        carina_core::provider::provider_custom_types_from_factories(Arc::clone(&factories));
    let attributes: HashMap<String, Value> = HashMap::from([(
        "guarded".to_string(),
        Value::Concrete(ConcreteValue::String("rejected".into())),
    )]);

    let errors = schema
        .validate_with_resolver(&attributes, &|_| false, &custom_types)
        .expect_err("the SDK schema callback must reject through the guest bridge");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("intentional SDK schema validator rejection")),
        "unexpected errors: {errors:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn declared_host_is_allowed_before_the_test_route_replaces_resolution() {
    let path = skip_if_no_http_wasm!();
    let Some((server_address, server)) = spawn_http_ok_server() else {
        return;
    };
    let (factory, _cache) = load_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", server_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "declared-egress-host",
        None,
    );
    let resource = http_send_phase_resource(
        "declared-egress-host",
        &format!("http://phase-test.invalid:{}/", server_address.port()),
        "POST",
        "declared",
    );

    let state = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource).await)
            .await
            .expect("the declared request should be dispatched"),
    );
    let bytes_read = server.join().expect("loopback server should finish");

    assert!(bytes_read > 0);
    assert_eq!(http_probe_outcome(&state), "success");
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_only_instance_denies_even_a_declared_host() {
    let path = skip_if_no_http_wasm!();
    let socket = match tokio::net::TcpSocket::new_v4().and_then(|socket| {
        socket.bind((Ipv4Addr::LOCALHOST, 0).into())?;
        Ok(socket)
    }) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("SKIP: sandbox cannot reserve a loopback port: {error}");
            return;
        }
    };
    let address = socket.local_addr().expect("reserved socket has an address");
    let (factory, _cache) = load_schema_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", address);
    let attributes = indexmap::IndexMap::from([(
        "validate_egress_url".to_string(),
        Value::Concrete(ConcreteValue::String(
            format!("http://phase-test.invalid:{}/", address.port()).into(),
        )),
    )]);
    let config = resolved_provider_config(factory.name(), attributes);

    let error = carina_core::executor::prepare_provider_ready_config(&factory, &config)
        .expect_err("the schema-only guest store must have no egress");
    drop(socket);
    let rendered = error.to_string();
    assert!(
        rendered.contains("phase-test.invalid")
            && rendered.contains("consented egress declaration"),
        "unexpected schema-only denial: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_kinds_do_not_inherit_amazonaws_egress() {
    let http_path = skip_if_no_http_wasm!();
    let aws_path = skip_if_no_aws_http_wasm!();
    for (class, path, provider_name) in [
        (ProviderTrustClass::Aws, aws_path.as_path(), "aws"),
        (ProviderTrustClass::GitHub, http_path.as_path(), "mock_http"),
        (
            ProviderTrustClass::Untrusted,
            http_path.as_path(),
            "mock_http",
        ),
    ] {
        let socket = match tokio::net::TcpSocket::new_v4().and_then(|socket| {
            socket.bind((Ipv4Addr::LOCALHOST, 0).into())?;
            Ok(socket)
        }) {
            Ok(socket) => socket,
            Err(error) => {
                eprintln!("SKIP: sandbox cannot reserve a loopback port: {error}");
                return;
            }
        };
        let address = socket.local_addr().expect("reserved socket has an address");
        let (factory, _cache) =
            load_http_factory_with_name_and_class(path, provider_name, class).await;
        let factory = factory.with_http_test_route("sts.amazonaws.com", address);
        let provider = factory
            .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
            .await
            .expect("provider should initialize");
        let id = ResourceId::with_provider_identity(
            provider_name,
            "test.send_phase",
            "amazonaws-denied",
            None,
        );
        let resource = http_send_phase_resource_with_provider(
            provider_name,
            "amazonaws-denied",
            &format!("http://sts.amazonaws.com:{}/", address.port()),
            "POST",
            "denied",
        );

        let failure = provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource).await)
            .await
            .expect_err("kind alone must not grant amazonaws.com egress");
        drop(socket);
        let rendered = create_failure_error(failure).to_string();
        assert!(
            rendered.contains("sts.amazonaws.com")
                && rendered.contains("consented egress declaration"),
            "unexpected {class:?} denial: {rendered}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_egress_denial_traps_instead_of_reaching_guest_send_phase_classification() {
    let path = skip_if_no_http_wasm!();
    let (factory, _cache) = load_http_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "wasi-http-denied",
        None,
    );
    let resource = http_send_phase_resource(
        "wasi-http-denied",
        "http://example.invalid/",
        "POST",
        "denied",
    );

    let failure = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("the host egress policy must trap the guest");
    let CreateFailureKind::Untracked(error) = failure.into_kind() else {
        panic!("a policy-denied request must not count as dispatched egress")
    };
    let rendered = error.to_string();

    assert!(
        rendered.contains("mock_http")
            && rendered.contains("file:///test-providers/mock_http.wasm")
            && rendered.contains("example.invalid")
            && rendered.contains("host is not in the provider's consented egress declaration")
            && rendered.contains("WASM guest call stopped by the host egress policy")
            && rendered.contains("create call:"),
        "unexpected host-policy denial: {rendered}"
    );
    assert_eq!(
        rendered.matches("host egress policy").count(),
        1,
        "{rendered}"
    );
    assert!(
        !rendered.contains("not_sent") && !rendered.contains("WASM guest trapped"),
        "the denial must not be classified by the guest or use generic trap wording: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn untrusted_metadata_denial_is_immediate_and_poisons_the_provider_instance() {
    let path = skip_if_no_http_wasm!();
    let (factory, _cache) = load_http_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "wasi-http-metadata-denied",
        None,
    );
    let resource = http_send_phase_resource(
        "wasi-http-metadata-denied",
        "http://169.254.169.254/latest/api/token",
        "PUT",
        "",
    );
    let request = create_request_for_test(resource).await;

    let started = Instant::now();
    let failure = provider
        .provider()
        .create_for_test(&id, request)
        .await
        .expect_err("an untrusted provider must trap on an IMDS request");
    let elapsed = started.elapsed();
    let error = create_failure_error(failure);
    let rendered = error.to_string();
    eprintln!("untrusted metadata create denial elapsed={elapsed:?}: {rendered}");

    assert!(
        elapsed < Duration::from_secs(5),
        "create denial took {elapsed:?}"
    );
    assert!(
        rendered.contains("mock_http")
            && rendered.contains("169.254.169.254")
            && rendered.contains("instance-metadata address")
            && rendered.contains("create call"),
        "unexpected metadata denial: {rendered}"
    );
    assert!(
        !rendered.contains("WASM guest trapped")
            && !rendered.contains("timed out")
            && !rendered.contains("--trust-local-provider")
            && !rendered.contains("CARINA_TRUST_LOCAL_PROVIDER")
            && !rendered.contains('\n'),
        "the denial must retain its dedicated non-timeout diagnostic: {rendered}"
    );

    let followup_id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "after-metadata-denial",
        None,
    );
    let followup_started = Instant::now();
    let followup_error = provider
        .provider()
        .read(&followup_id, &mock_identifier(), ReadRequest)
        .await
        .expect_err("the trapping metadata denial must poison the shared instance");
    let followup_elapsed = followup_started.elapsed();
    let followup_rendered = followup_error.to_string();

    assert!(
        followup_elapsed < Duration::from_secs(1),
        "poisoned provider did not fail fast: {followup_elapsed:?}"
    );
    assert!(
        followup_rendered.contains("provider instance unusable after trap in create")
            && followup_rendered.contains("mock_http")
            && followup_rendered.contains("169.254.169.254"),
        "poisoned-provider error lost the denial root cause: {followup_rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn post_resolution_metadata_denial_traps_and_poisons_the_provider_instance() {
    let path = skip_if_no_http_wasm!();
    let (factory, _cache) = load_http_factory(&path).await;
    let metadata_address = SocketAddr::from((Ipv4Addr::new(169, 254, 169, 254), 80));
    let factory = factory.with_http_test_route("resolved-metadata.invalid", metadata_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "wasi-http-resolved-metadata-denied",
        None,
    );
    let resource = http_send_phase_resource(
        "wasi-http-resolved-metadata-denied",
        "http://resolved-metadata.invalid/latest/api/token",
        "PUT",
        "",
    );

    let started = Instant::now();
    let failure = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("post-resolution metadata policy must trap the guest");
    let elapsed = started.elapsed();
    let rendered = create_failure_error(failure).to_string();

    assert!(
        elapsed < Duration::from_secs(5),
        "post-resolution metadata denial took {elapsed:?}"
    );
    assert!(
        rendered.contains("WASM guest call stopped by the host egress policy")
            && rendered.contains("create call:")
            && rendered.contains("provider 'mock_http'")
            && rendered.contains("file:///test-providers/mock_http.wasm")
            && rendered.contains("request to resolved-metadata.invalid was denied")
            && rendered.contains(
                "instance-metadata address 169.254.169.254 is withheld from every provider"
            ),
        "unexpected post-resolution metadata denial: {rendered}"
    );
    assert!(
        !rendered.contains("not_sent")
            && !rendered.contains("WASM guest trapped")
            && !rendered.contains("timed out")
            && !rendered.contains("--trust-local-provider")
            && !rendered.contains("CARINA_TRUST_LOCAL_PROVIDER")
            && !rendered.contains('\n'),
        "the post-resolution denial must remain an outer typed trap: {rendered}"
    );

    let followup_id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "after-resolved-metadata-denial",
        None,
    );
    let followup_started = Instant::now();
    let followup_error = provider
        .provider()
        .read(&followup_id, &mock_identifier(), ReadRequest)
        .await
        .expect_err("the post-resolution metadata trap must poison the shared instance");
    let followup_elapsed = followup_started.elapsed();
    let followup_rendered = followup_error.to_string();

    assert!(
        followup_elapsed < Duration::from_secs(1),
        "poisoned provider did not fail fast: {followup_elapsed:?}"
    );
    assert!(
        followup_rendered.contains("provider instance unusable after trap in create")
            && followup_rendered.contains("mock_http")
            && followup_rendered.contains("resolved-metadata.invalid"),
        "poisoned-provider error lost the post-resolution denial: {followup_rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_guest_classifies_post_body_closed_tcp_port_as_not_sent() {
    let path = skip_if_no_http_wasm!();
    let body = http_probe_body();
    let reserved_socket = match tokio::net::TcpSocket::new_v4().and_then(|socket| {
        socket.bind((Ipv4Addr::LOCALHOST, 0).into())?;
        Ok(socket)
    }) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("SKIP: sandbox cannot reserve a closed loopback port: {error}");
            return;
        }
    };
    let closed_address = reserved_socket
        .local_addr()
        .expect("reserved socket has an address");

    let (factory, _cache) = load_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", closed_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "wasi-http-connect-failure",
        None,
    );
    let resource = http_send_phase_resource(
        "wasi-http-connect-failure",
        &format!("http://phase-test.invalid:{}/", closed_address.port()),
        "POST",
        &body,
    );

    let state = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource).await)
            .await
            .expect("the mock transport probe should report its phase"),
    );
    drop(reserved_socket);

    assert!(
        matches!(
            http_probe_outcome(&state),
            "not_sent:ConnectionRefused" | "not_sent:ConnectionTimeout"
        ),
        "unexpected closed-port outcome: {}",
        http_probe_outcome(&state)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_guest_classifies_post_body_closed_after_request_as_maybe_sent() {
    let path = skip_if_no_http_wasm!();
    let body = http_probe_body();
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("SKIP: sandbox cannot bind the loopback test server: {error}");
            return;
        }
    };
    let server_address = listener
        .local_addr()
        .expect("bound listener has an address");
    listener
        .set_nonblocking(true)
        .expect("set loopback listener nonblocking");

    let (factory, _cache) = load_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", server_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "wasi-http-closed",
        None,
    );
    let resource = http_send_phase_resource(
        "wasi-http-closed",
        &format!("http://phase-test.invalid:{}/", server_address.port()),
        "POST",
        &body,
    );

    // Component compilation and provider initialization can be slow under
    // parallel load, so start the accept deadline only once the guest is ready.
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "server accept timed out");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("server accept failed: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set accepted socket blocking");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server read timeout");
        let mut request = [0_u8; 4096];
        stream
            .read(&mut request)
            .expect("read request bytes before closing")
    });

    let state = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource).await)
            .await
            .expect("the mock transport probe should report its phase"),
    );
    let bytes_read = server.join().expect("loopback server should finish");

    assert!(bytes_read > 0, "the server must receive request bytes");
    assert_eq!(http_probe_outcome(&state), "maybe_sent:HttpProtocolError");
}

async fn probe_early_http_response(response: &'static [u8], check_outcome: impl FnOnce(&str)) {
    const BODY_BYTES: usize = 32 * 1024 * 1024;

    let path = skip_if_no_http_wasm!();
    let listener = match tokio::net::TcpSocket::new_v4().and_then(|socket| {
        // Bound the receive window before listen/accept so it is inherited by
        // the connection. Early-response ordering relies on the body exceeding
        // the kernel send/receive buffers (Linux tcp_wmem max is 4 MiB by
        // default), plus the host's two-chunk outgoing-body channel.
        socket.set_recv_buffer_size(16 * 1024)?;
        socket.bind((Ipv4Addr::LOCALHOST, 0).into())?;
        socket.listen(1)?.into_std()
    }) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("SKIP: sandbox cannot bind the early-response test server: {error}");
            return;
        }
    };
    let server_address = listener
        .local_addr()
        .expect("bound listener has an address");
    let (factory, _cache) = load_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", server_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "wasi-http-early-response",
        None,
    );
    let resource = http_send_phase_resource(
        "wasi-http-early-response",
        &format!("http://phase-test.invalid:{}/", server_address.port()),
        "POST",
        &"x".repeat(BODY_BYTES),
    );
    let request = create_request_for_test(resource).await;

    // Start the accept deadline after compilation and request preparation.
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "server accept timed out");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("server accept failed: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set accepted socket blocking");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server read timeout");

        let head = read_http_request_head(&mut stream);
        assert_eq!(
            http_request_content_length(&head),
            BODY_BYTES,
            "the guest must dispatch the entire large body"
        );
        stream
            .write_all(response)
            .expect("send response before reading the request body");
        // Drain after shutting down writes so unread request data cannot cause
        // an RST on drop that destroys the buffered response. After the complete
        // Connection: close response, hyper stops sending the request body and
        // drops its body receiver, so the guest's write still fails with Closed.
        stream
            .shutdown(Shutdown::Write)
            .expect("finish response before draining the request body");
        let drain_deadline = Instant::now() + Duration::from_secs(10);
        let mut discarded = [0_u8; 8192];
        while Instant::now() < drain_deadline {
            match stream.read(&mut discarded) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let outcome = provider.provider().create_for_test(&id, request).await;
    server.join().expect("early-response server should finish");
    let state = create_state_for_writeback(outcome.expect("the probe must report its outcome"));

    check_outcome(http_probe_outcome(&state));
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_guest_early_response_with_complete_200_body_succeeds() {
    probe_early_http_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\naccepted",
        |outcome| assert_eq!(outcome, "success"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_guest_early_response_with_complete_413_body_preserves_status() {
    probe_early_http_response(
        b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 9\r\nConnection: close\r\n\r\ntoo large",
        |outcome| assert_eq!(outcome, "status:413"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_guest_early_response_with_truncated_body_is_maybe_sent() {
    probe_early_http_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\ntruncated",
        |outcome| {
            let (phase, code) = outcome
                .split_once(':')
                .expect("a truncated response must report a transport failure");
            assert_eq!(
                phase, "maybe_sent",
                "unexpected truncated response: {outcome}"
            );
            assert_ne!(
                code, "none",
                "the host must retain its body-read error code"
            );
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_guest_connection_reset_after_request_head_is_maybe_sent() {
    const BODY_BYTES: usize = 32 * 1024 * 1024;

    let path = skip_if_no_http_wasm!();
    let listener = match tokio::net::TcpSocket::new_v4().and_then(|socket| {
        // Keep the large body in flight when the server resets the connection.
        socket.set_recv_buffer_size(16 * 1024)?;
        socket.bind((Ipv4Addr::LOCALHOST, 0).into())?;
        socket.listen(1)?.into_std()
    }) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("SKIP: sandbox cannot bind the connection-reset test server: {error}");
            return;
        }
    };
    let server_address = listener
        .local_addr()
        .expect("bound listener has an address");
    let (factory, _cache) = load_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", server_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "wasi-http-reset-after-head",
        None,
    );
    let resource = http_send_phase_resource(
        "wasi-http-reset-after-head",
        &format!("http://phase-test.invalid:{}/", server_address.port()),
        "POST",
        &"x".repeat(BODY_BYTES),
    );
    let request = create_request_for_test(resource).await;

    // Start the accept deadline after compilation and request preparation.
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "server accept timed out");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("server accept failed: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set accepted socket blocking");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server read timeout");

        let head = read_http_request_head(&mut stream);
        assert_eq!(
            http_request_content_length(&head),
            BODY_BYTES,
            "the guest must dispatch the entire large body"
        );

        // No response bytes exist, so even an RST observed before the client's
        // first read must yield a transport failure after request delivery.
        let socket = tokio::net::TcpSocket::from_std_stream(stream);
        // Tokio deprecates linger because nonzero durations can block on drop;
        // zero is intentional here to force an immediate RST.
        #[allow(deprecated)]
        socket
            .set_linger(Some(Duration::ZERO))
            .expect("force an RST when the stream is dropped");
        drop(socket);
    });

    let outcome = provider.provider().create_for_test(&id, request).await;
    server
        .join()
        .expect("connection-reset server should finish");
    let state = create_state_for_writeback(outcome.expect("the probe must report its outcome"));
    let outcome = http_probe_outcome(&state);
    let (phase, code) = outcome
        .split_once(':')
        .expect("a connection reset must report a transport failure");
    eprintln!("connection reset after request head: phase={phase} code={code}");
    assert_eq!(
        phase, "maybe_sent",
        "the server received the request head before resetting: {outcome}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_guest_classifies_post_body_first_byte_timeout_as_maybe_sent() {
    let path = skip_if_no_http_wasm!();
    let body = http_probe_body();
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("SKIP: sandbox cannot bind the loopback test server: {error}");
            return;
        }
    };
    let server_address = listener
        .local_addr()
        .expect("bound listener has an address");
    listener
        .set_nonblocking(true)
        .expect("set loopback listener nonblocking");

    let (factory, _cache) = load_http_factory(&path).await;
    let factory = factory.with_http_test_route("phase-test.invalid", server_address);
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock_http",
        "test.send_phase",
        "wasi-http-first-byte-timeout",
        None,
    );
    let resource = http_send_phase_resource(
        "wasi-http-first-byte-timeout",
        &format!("http://phase-test.invalid:{}/", server_address.port()),
        "POST",
        &body,
    );

    let (release_server, wait_for_release) = std::sync::mpsc::channel();
    // Start both the accept deadline and the silent-server lifetime only after
    // component compilation and provider initialization have completed.
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "server accept timed out");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("server accept failed: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set accepted socket blocking");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server read timeout");
        let mut request = [0_u8; 4096];
        let bytes_read = stream
            .read(&mut request)
            .expect("read request bytes before waiting");
        assert!(bytes_read > 0, "the server must receive request bytes");
        wait_for_release
            .recv_timeout(Duration::from_secs(10))
            .expect("guest did not finish before the silent-server deadline");
        bytes_read
    });

    let outcome = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await;
    release_server
        .send(())
        .expect("silent server should still be waiting");
    let bytes_read = server.join().expect("loopback server should finish");
    let state = create_state_for_writeback(
        outcome.expect("the mock transport probe should report its phase"),
    );

    assert!(bytes_read > 0, "the server must receive request bytes");
    assert_eq!(
        http_probe_outcome(&state),
        "maybe_sent:ConnectionReadTimeout"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_create_polls_until_success() {
    const PENDING_POLLS: u32 = 2;

    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
            .provider()
            .create_for_test(&id, create_request_for_test(resource).await)
            .await
            .expect("pending create should settle"),
    );

    assert_minted_mock_identifier(
        state
            .identifier
            .as_deref()
            .expect("a completed create has an identifier"),
    );
    assert!(state.exists);
    assert_eq!(
        state.attributes.get("__mock_create_polls_served__"),
        Some(&Value::Concrete(ConcreteValue::Int(i64::from(
            PENDING_POLLS
        ))))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_create_read_back_completes_within_budget() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "read-back-create", None);
    let resource = resource_with_int_attribute(
        "test.resource",
        "read-back-create",
        "create_read_back_pending_polls",
        1,
    );

    let state = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource).await)
            .await
            .expect("read-back should settle within the default budget"),
    );

    assert_eq!(
        state.attributes.get("computed"),
        Some(&Value::Concrete(ConcreteValue::String("ready".into())))
    );
    assert!(state.partial_read.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_create_read_back_budget_returns_partial_then_refresh_completes() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "read-back-create-partial",
        None,
    );
    let resource = resource_with_int_attribute(
        "test.short_budget",
        "read-back-create-partial",
        "create_read_back_pending_polls",
        100,
    );

    let outcome = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect("the final attempt should return a partial success");
    let diagnostic = outcome
        .diagnostic()
        .expect("budget exhaustion must keep the partial-read diagnostic");
    assert_eq!(diagnostic.missing_attributes(), &["computed".to_string()]);
    let partial = create_state_for_writeback(outcome);
    assert!(partial.partial_read.is_some());
    let partial_identifier = AppliedIdentifier::new(
        partial
            .identifier
            .clone()
            .expect("a partial create retains its identifier"),
    )
    .expect("the retained identifier is non-empty");
    assert_minted_mock_identifier(partial_identifier.as_str());

    let refreshed = provider
        .provider()
        .read(&id, &partial_identifier, ReadRequest)
        .await
        .expect("the next refresh should re-read the created resource")
        .into_state();
    assert_eq!(
        refreshed.attributes.get("computed"),
        Some(&Value::Concrete(ConcreteValue::String("ready".into())))
    );
    assert!(refreshed.partial_read.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_create_read_back_pending_after_final_preserves_identifier() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "read-back-create-violation",
        None,
    );
    let mut resource = resource_with_int_attribute(
        "test.short_budget",
        "read-back-create-violation",
        "create_read_back_pending_polls",
        100,
    );
    resource.set_attr(
        "create_read_back_pending_after_final",
        Value::Concrete(ConcreteValue::Bool(true)),
    );

    let failure = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("pending/read-back after final-attempt is a protocol violation")
        .into_kind();
    let CreateFailureKind::InFlight { pending, error } = failure else {
        panic!("an issued read-back violation must remain recoverable")
    };
    assert_minted_mock_identifier(
        pending
            .identifier()
            .expect("the violation retains its identifier")
            .as_str(),
    );
    assert!(error.to_string().contains("final-attempt"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_create_read_back_cannot_regress_to_operation_phase() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "read-back-create-phase-regression",
        None,
    );
    let mut resource = resource_with_int_attribute(
        "test.short_budget",
        "read-back-create-phase-regression",
        "create_read_back_pending_polls",
        100,
    );
    resource.set_attr(
        "create_read_back_operation_after_final",
        Value::Concrete(ConcreteValue::Bool(true)),
    );

    let failure = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("read-back must not regress to operation pending")
        .into_kind();
    let CreateFailureKind::InFlight { pending, error } = failure else {
        panic!("a phase regression must retain the issued create")
    };
    assert_minted_mock_identifier(
        pending
            .identifier()
            .expect("the phase regression retains its identifier")
            .as_str(),
    );
    assert!(error.to_string().contains("polling protocol"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_resumed_create_read_back_final_attempt_returns_partial_success() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "resumed-read-back-partial",
        None,
    );
    let token = serde_json::json!({ "phase": "read_back" }).to_string();
    let pending = persisted_create_for_resume(&id, &token, Some("mock-id"));
    let resource = resource_with_int_attribute(
        "test.short_budget",
        "resumed-read-back-partial",
        "create_read_back_pending_polls",
        100,
    );
    let request = ResumeCreateRequest::for_test(pending, create_request_for_test(resource).await)
        .expect("resume fixture route must match");

    let result = provider
        .provider()
        .resume_create(request)
        .await
        .map(ResumeOutcome::into_kind)
        .map_err(ResumeFailure::into_kind);

    let Ok(ResumeOutcomeKind::PartialSuccess(partial)) = result else {
        panic!("final read-back attempt must return a partial success, got {result:?}")
    };
    assert_eq!(partial.identifier().as_str(), "mock-id");
    assert_eq!(
        partial.diagnostic().missing_attributes(),
        &["computed".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_fresh_delete_recovers_pending_controls_from_identifier() {
    let path = wasm_path().expect("build the WASM mock before running this test");
    let (factory, _cache) = load_factory(&path).await;
    let config = ready_config(&factory, &indexmap::IndexMap::new());
    let provider = factory
        .create_initialized_provider(None, &config)
        .await
        .unwrap();
    let resource = resource_with_int_attribute(
        "test.short_budget",
        "fresh-delete",
        "delete_pending_polls",
        100,
    );
    let id = resource.id.clone();
    let created = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource).await)
            .await
            .expect("create resource with asynchronous delete"),
    );
    drop(provider);
    let (factory, _fresh_cache) = load_factory(&path).await;
    let config = ready_config(&factory, &indexmap::IndexMap::new());
    let fresh = factory
        .create_initialized_provider(None, &config)
        .await
        .unwrap();
    let identifier = AppliedIdentifier::new(created.identifier.unwrap()).unwrap();
    let error = fresh
        .provider()
        .delete(&id, &identifier, delete_request_without_schema())
        .await
        .expect_err("a fresh instance must honor the pending delete and exhaust its budget");
    assert!(error.to_string().contains("budget"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_fresh_read_uses_updated_read_back_controls() {
    let path = wasm_path().expect("build the WASM mock before running this test");
    let (factory, _cache) = load_factory(&path).await;
    let config = ready_config(&factory, &indexmap::IndexMap::new());
    let provider = factory
        .create_initialized_provider(None, &config)
        .await
        .unwrap();
    let mut resource = resource_with_int_attribute("test.resource", "fresh-update", "value", 1);
    let id = resource.id.clone();
    let created = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .unwrap(),
    );
    drop(provider);
    let (factory, _update_cache) = load_factory(&path).await;
    let config = ready_config(&factory, &indexmap::IndexMap::new());
    let updater = factory
        .create_initialized_provider(None, &config)
        .await
        .unwrap();
    let identifier = AppliedIdentifier::new(created.identifier.clone().unwrap()).unwrap();
    resource.set_attr("value", Value::Concrete(ConcreteValue::Int(2)));
    resource.set_attr(
        "update_read_back_pending_polls",
        Value::Concrete(ConcreteValue::Int(1)),
    );
    let request = update_request_for_test(
        resource,
        created,
        &["value".into(), "update_read_back_pending_polls".into()],
    )
    .await;
    let updated = updater
        .provider()
        .update(&id, &identifier, request)
        .await
        .unwrap()
        .into_state_for_writeback();
    drop(updater);
    let (factory, _read_cache) = load_factory(&path).await;
    let config = ready_config(&factory, &indexmap::IndexMap::new());
    let reader = factory
        .create_initialized_provider(None, &config)
        .await
        .unwrap();
    let identifier = AppliedIdentifier::new(updated.identifier.unwrap()).unwrap();
    let state = reader
        .provider()
        .read(&id, &identifier, ReadRequest)
        .await
        .unwrap()
        .into_state();
    assert_eq!(
        state.attributes.get("computed"),
        Some(&Value::Concrete(ConcreteValue::String("ready".into()))),
        "a new instance must read the updated payload, not the create-time payload",
    );
    assert_ne!(
        state.attributes.get("value"),
        Some(&Value::Concrete(ConcreteValue::Int(1)))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_update_polls_until_success() {
    const PENDING_POLLS: u32 = 2;

    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "pending-update", None);
    let mut resource = resource_with_int_attribute("test.resource", "pending-update", "value", 1);
    let created = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .expect("setup create should succeed"),
    );
    let created_identifier = AppliedIdentifier::new(created.identifier.clone().unwrap()).unwrap();
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
        .provider()
        .update(&id, &created_identifier, request)
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
async fn test_wasm_update_read_back_completes_within_budget() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "read-back-update", None);
    let mut resource = resource_with_int_attribute("test.resource", "read-back-update", "value", 1);
    let created = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .expect("setup create should succeed"),
    );
    let created_identifier = AppliedIdentifier::new(created.identifier.clone().unwrap()).unwrap();
    resource.set_attr("value", Value::Concrete(ConcreteValue::Int(2)));
    resource.set_attr(
        "update_read_back_pending_polls",
        Value::Concrete(ConcreteValue::Int(1)),
    );
    let request = update_request_for_test(
        resource,
        created,
        &[
            "value".to_string(),
            "update_read_back_pending_polls".to_string(),
        ],
    )
    .await;

    let state = provider
        .provider()
        .update(&id, &created_identifier, request)
        .await
        .expect("update read-back should settle within the default budget")
        .into_state_for_writeback();
    assert_eq!(
        state.attributes.get("computed"),
        Some(&Value::Concrete(ConcreteValue::String("ready".into())))
    );
    assert!(state.partial_read.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_update_read_back_budget_returns_partial_then_refresh_completes() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "read-back-update-partial",
        None,
    );
    let mut resource =
        resource_with_int_attribute("test.short_budget", "read-back-update-partial", "value", 1);
    let created = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .expect("setup create should succeed"),
    );
    let created_identifier = AppliedIdentifier::new(created.identifier.clone().unwrap()).unwrap();
    resource.set_attr("value", Value::Concrete(ConcreteValue::Int(2)));
    resource.set_attr(
        "update_read_back_pending_polls",
        Value::Concrete(ConcreteValue::Int(100)),
    );
    let request = update_request_for_test(
        resource,
        created,
        &[
            "value".to_string(),
            "update_read_back_pending_polls".to_string(),
        ],
    )
    .await;

    let outcome = provider
        .provider()
        .update(&id, &created_identifier, request)
        .await
        .expect("the final update attempt should return a partial success");
    let diagnostic = outcome
        .diagnostic()
        .expect("budget exhaustion must keep the partial-read diagnostic");
    assert_eq!(diagnostic.missing_attributes(), &["computed".to_string()]);
    let partial = outcome.into_state_for_writeback();
    assert!(partial.partial_read.is_some());

    let partial_identifier = AppliedIdentifier::new(partial.identifier.clone().unwrap()).unwrap();
    let refreshed = provider
        .provider()
        .read(&id, &partial_identifier, ReadRequest)
        .await
        .expect("the next refresh should re-read the updated resource")
        .into_state();
    assert_eq!(
        refreshed.attributes.get("computed"),
        Some(&Value::Concrete(ConcreteValue::String("ready".into())))
    );
    assert!(refreshed.partial_read.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_update_read_back_phase_regression_is_protocol_violation_and_recoverable() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "read-back-update-phase-regression",
        None,
    );
    let mut resource = resource_with_int_attribute(
        "test.short_budget",
        "read-back-update-phase-regression",
        "value",
        1,
    );
    let created = create_state_for_writeback(
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .expect("setup create should succeed"),
    );
    let created_identifier = AppliedIdentifier::new(created.identifier.clone().unwrap()).unwrap();
    resource.set_attr("value", Value::Concrete(ConcreteValue::Int(2)));
    resource.set_attr(
        "update_read_back_pending_polls",
        Value::Concrete(ConcreteValue::Int(100)),
    );
    resource.set_attr(
        "update_read_back_operation_after_final",
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    let request = update_request_for_test(
        resource,
        created,
        &[
            "value".to_string(),
            "update_read_back_pending_polls".to_string(),
            "update_read_back_operation_after_final".to_string(),
        ],
    )
    .await;

    let error = provider
        .provider()
        .update(&id, &created_identifier, request)
        .await
        .expect_err("read-back must not regress to operation pending");
    assert!(error.to_string().contains("polling protocol"), "{error}");

    let recovered = provider
        .provider()
        .read(&id, &created_identifier, ReadRequest)
        .await
        .expect("the retained update identifier must remain usable for recovery")
        .into_state();
    assert_eq!(
        recovered.attributes.get("value"),
        Some(&Value::Concrete(ConcreteValue::Int(2)))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_delete_polls_until_success() {
    const PENDING_POLLS: u32 = 2;

    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "pending-delete", None);
    let resource = resource_with_int_attribute(
        "test.resource",
        "pending-delete",
        "delete_pending_polls",
        i64::from(PENDING_POLLS),
    );
    let created = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect("setup create should succeed");
    let identifier = created.identifier().clone();

    let delete_started = Instant::now();
    provider
        .provider()
        .delete(&id, &identifier, delete_request_without_schema())
        .await
        .expect("pending delete should settle");
    assert!(
        delete_started.elapsed() >= Duration::from_secs(u64::from(PENDING_POLLS)),
        "delete must serve every requested one-second pending poll"
    );

    let state = provider
        .provider()
        .read(&id, &identifier, ReadRequest)
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
            .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        provider
            .provider()
            .create_for_test(&pending_id, create_request_for_test(pending).await),
    )
    .await
    .expect_err("the caller timeout should drop the operation between polls");

    let healthy_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-cancel", None);
    let healthy = resource_with_int_attribute("test.resource", "after-cancel", "value", 1);
    provider
        .provider()
        .create_for_test(&healthy_id, create_request_for_test(healthy).await)
        .await
        .expect("cancellation between polls must not poison the WASM instance");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_pending_over_budget_times_out_without_poisoning_instance() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "pending create outcome became unknown for {}: {error}",
            unknown.id()
        ),
        CreateFailureKind::Created { created, error } => panic!(
            "pending create was classified as created for {}: {error}",
            created.identifier()
        ),
    }

    let healthy_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-timeout", None);
    let healthy = resource_with_int_attribute("test.resource", "after-timeout", "value", 1);
    provider
        .provider()
        .create_for_test(&healthy_id, create_request_for_test(healthy).await)
        .await
        .expect("budget exhaustion must not poison the WASM instance");
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_mock_exercises_untracked_created_and_malformed_pending_verdicts() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
        .provider()
        .create_for_test(&created_id, create_request_for_test(created).await)
        .await
        .expect_err("created mode must preserve its observed resource")
        .into_kind();
    let CreateFailureKind::Created { created, error } = created else {
        panic!("created mode returned the wrong failure variant");
    };
    drop(error);
    assert_minted_mock_identifier(created.identifier().as_str());
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
        .provider()
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
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "undecodable token became outcome unknown for {}: {error}",
            unknown.id()
        ),
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("guest encoding failure must remain classified")
        .into_kind();
    match failure {
        CreateFailureKind::Created { created, error } => {
            assert_minted_mock_identifier(created.identifier().as_str());
            assert!(!error.to_string().is_empty());
        }
        CreateFailureKind::Untracked(error) => {
            panic!("issued create was lost as untracked: {error}")
        }
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "issued create lost its identifier as outcome unknown for {}: {error}",
            unknown.id()
        ),
        CreateFailureKind::InFlight { pending, error } => {
            panic!("finished create unexpectedly remained in flight {pending:?}: {error}")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_unissued_pending_token_encode_failure_without_egress_is_untracked() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("an unissued unencodable continuation must trap")
        .into_kind()
    {
        CreateFailureKind::Untracked(error) => {
            assert!(error.to_string().contains("WASM guest trapped"));
        }
        CreateFailureKind::Created { created, error } => panic!(
            "unissued create unexpectedly retained identifier {}: {error}",
            created.identifier()
        ),
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "pre-dispatch token encoding failure became outcome unknown for {}: {error}",
            unknown.id()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
        .create_for_test(&id, create_request_for_test(resource).await)
        .await
        .expect_err("an issued create with no encodable continuation must be retained")
        .into_kind()
    {
        CreateFailureKind::Created { created, error } => {
            assert_minted_mock_identifier(created.identifier().as_str());
            let read_error = match created.observation() {
                CreateObservation::Observed(state) => {
                    panic!("token encoding failure became an observed create: {state:?}")
                }
                CreateObservation::Unobserved(reason) => match reason {
                    UnobservedReason::NotFound => {
                        panic!("token encoding failure became a not-found observation")
                    }
                    UnobservedReason::ReadFailed(read_error) => read_error,
                },
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
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "issued create lost its identifier as outcome unknown for {}: {error}",
            unknown.id()
        ),
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "poll token encoding failure became outcome unknown for {}: {error}",
            unknown.id()
        ),
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
            assert_minted_mock_identifier(
                pending
                    .identifier()
                    .expect("the previous issued continuation retains its identifier")
                    .as_str(),
            );
            assert!(error.to_string().contains("poll_create pending token"));
        }
        CreateFailureKind::Untracked(error) => {
            panic!("issued poll was lost as untracked: {error}")
        }
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "issued poll became outcome unknown for {}: {error}",
            unknown.id()
        ),
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.short_budget",
        "unencodable-repending",
        None,
    );
    let previous_token = serde_json::json!({
        "phase": "operation",
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
        .provider()
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
        Ok(ResumeOutcomeKind::PartialSuccess(outcome)) => {
            panic!("token encoding failure unexpectedly completed partially {outcome:?}")
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
            "phase": "operation",
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
            .provider()
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
                    assert_minted_mock_identifier(state.identifier().as_str());
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
            (expected, Ok(ResumeOutcomeKind::PartialSuccess(outcome))) => {
                panic!("expected {expected}, got partial {outcome:?}")
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
        .provider()
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
        Ok(ResumeOutcomeKind::PartialSuccess(outcome)) => {
            panic!("undecodable resume token unexpectedly completed partially: {outcome:?}")
        }
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
        "phase": "operation",
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
        .provider()
        .resume_create(request)
        .await
        .map(ResumeOutcome::into_kind)
        .map_err(ResumeFailure::into_kind)
    {
        Ok(ResumeOutcomeKind::Resumed(created)) => {
            assert_eq!(created.identifier().as_str(), canonical);
        }
        Ok(ResumeOutcomeKind::PartialSuccess(outcome)) => {
            panic!("expected resumed canonical identifier, got partial {outcome:?}")
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        // The slow initial call reaches the operation deadline. Protocol v15
        // still grants exactly one final poll, so keep the mock pending after
        // that poll to verify that the initial call consumed the budget.
        Value::Concrete(ConcreteValue::Int(2)),
    );

    let error = provider
        .provider()
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
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "slow initial call became outcome unknown for {}: {error}",
            unknown.id()
        ),
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "host-wait-5s", None);
    let resource =
        resource_with_int_attribute("test.resource", "host-wait-5s", "call_delay_millis", 5_000);

    provider
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        provider
            .provider()
            .create_for_test(&id, create_request_for_test(resource).await),
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "wall-timeout", None);
    let resource =
        resource_with_int_attribute("test.resource", "wall-timeout", "call_delay_millis", 10_000);
    let started = Instant::now();

    let error = provider
        .provider()
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
#[cfg(feature = "test-utils")]
async fn test_wasm_queued_sibling_observes_poison_before_dead_store() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(30),
        Duration::from_secs(1),
    ));
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let slow_id =
        ResourceId::with_provider_identity("mock", "test.resource", "queued-timeout", None);
    let slow = create_request_for_test(resource_with_int_attribute(
        "test.resource",
        "queued-timeout",
        "call_delay_millis",
        10_000,
    ))
    .await;
    let sibling_id =
        ResourceId::with_provider_identity("mock", "test.resource", "queued-sibling", None);

    let timed_out = provider.provider().create_for_test(&slow_id, slow);
    let sibling = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        provider
            .provider()
            .read(&sibling_id, &mock_identifier(), ReadRequest)
            .await
    };
    let (timed_out, sibling) = tokio::join!(timed_out, sibling);

    let timed_out = create_failure_error(timed_out.expect_err("the slow create must time out"));
    assert!(matches!(timed_out, ProviderError::Timeout(_)));
    let sibling = sibling.expect_err("the queued sibling must reject the poisoned instance");
    let rendered = sibling.to_string();
    assert!(
        rendered.contains("unusable after wall-clock timeout in create"),
        "the queued sibling must retain the original timeout reason: {rendered}"
    );
    assert!(
        !rendered.contains("cannot enter component instance")
            && !rendered.contains("WASM guest trapped: read call failed"),
        "the queued sibling must never enter the dead store: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_cancelled_guest_call_drops_live_instance_before_followup() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(30),
        Duration::from_secs(30),
    ));
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let slow_id =
        ResourceId::with_provider_identity("mock", "test.resource", "cancelled-call", None);
    let slow = create_request_for_test(resource_with_int_attribute(
        "test.resource",
        "cancelled-call",
        "call_delay_millis",
        10_000,
    ))
    .await;

    tokio::time::timeout(
        Duration::from_millis(500),
        provider.provider().create_for_test(&slow_id, slow),
    )
    .await
    .expect_err("the outer timeout must cancel the in-flight guest call");

    let followup_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-cancel", None);
    let followup = provider
        .provider()
        .read(&followup_id, &mock_identifier(), ReadRequest)
        .await
        .expect_err("cancelling an in-flight call must poison the configured instance");
    let rendered = followup.to_string();
    assert!(
        rendered.contains("prior operation was cancelled"),
        "the follow-up must retain the cancellation reason: {rendered}"
    );
    assert!(
        !rendered.contains("cannot enter component instance")
            && !rendered.contains("WASM guest trapped: read call failed"),
        "the follow-up must not enter the cancelled instance: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_poll_sleep_does_not_block_sibling_operation() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let pending_id =
        ResourceId::with_provider_identity("mock", "test.resource", "sleeping-poll", None);
    let resource =
        resource_with_int_attribute("test.resource", "sleeping-poll", "create_pending_polls", 1);
    let request = create_request_for_test(resource).await;
    let sibling_id =
        ResourceId::with_provider_identity("mock", "test.resource", "sibling-read", None);

    let pending_operation = provider.provider().create_for_test(&pending_id, request);
    let sibling_operation = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::time::timeout(
            Duration::from_millis(500),
            provider
                .provider()
                .read(&sibling_id, &mock_identifier(), ReadRequest),
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
    carina_core::executor::prepare_provider_ready_config(&factory, &clean_config)
        .expect("validate_config should rebuild the timed-out init instance");
    let retry_elapsed = retry_started.elapsed();
    assert!(
        retry_elapsed < Duration::from_secs(1),
        "a clean call on the rebuilt init instance should finish quickly; elapsed {retry_elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_repeated_validation_timeout_survives_interleaved_successes() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(30),
        Duration::from_secs(2),
    ));
    let slow_attributes = || {
        indexmap::IndexMap::from([(
            "validate_delay_millis".to_string(),
            Value::Concrete(ConcreteValue::Int(10_000)),
        )])
    };

    let first_config = resolved_provider_config(factory.name(), slow_attributes());
    carina_core::executor::prepare_provider_ready_config(&factory, &first_config)
        .expect_err("the initial validation instance should time out and become poisoned");

    let clean_config = || resolved_provider_config(factory.name(), indexmap::IndexMap::new());
    let clean_started = Instant::now();
    carina_core::executor::prepare_provider_ready_config(&factory, &clean_config())
        .expect("a different validation input should rebuild and validate normally");
    assert!(
        clean_started.elapsed() < Duration::from_secs(1),
        "a clean validation input should complete quickly"
    );

    let rebuilt_config = resolved_provider_config(factory.name(), slow_attributes());
    let started = Instant::now();
    let error = carina_core::executor::prepare_provider_ready_config(&factory, &rebuilt_config)
        .expect_err("the second timeout for this input must survive an interleaved success");
    let elapsed = started.elapsed();
    let rendered = error.to_string();

    assert!(rendered.contains("validate_config"), "{rendered}");
    assert!(rendered.contains("wall-clock budget"), "{rendered}");
    assert!(
        elapsed >= Duration::from_millis(1_500) && elapsed < Duration::from_secs(6),
        "the second validate_config call should time out after about 2s; elapsed {elapsed:?}"
    );

    carina_core::executor::prepare_provider_ready_config(&factory, &clean_config())
        .expect("the healthy input should still validate after the second timeout");

    let suspended_config = resolved_provider_config(factory.name(), slow_attributes());
    let suspended_started = Instant::now();
    let suspended =
        carina_core::executor::prepare_provider_ready_config(&factory, &suspended_config)
            .expect_err("a repeatedly timed-out validation input must be suspended");
    let suspended_elapsed = suspended_started.elapsed();
    let rendered = suspended.to_string();

    assert_eq!(
        suspended
            .provider_error()
            .map(carina_core::provider::ProviderError::kind),
        Some(ProviderErrorKind::Timeout)
    );
    assert!(
        rendered.contains(
            "provider validation is suspended after repeated wall-clock timeouts for this input"
        ),
        "{rendered}"
    );
    assert!(rendered.contains("validate_config"), "{rendered}");
    assert!(
        rendered.contains(
            "changing this value (or restarting with a new provider version) resumes validation"
        ),
        "{rendered}"
    );
    assert!(
        suspended_elapsed < Duration::from_secs(1),
        "a suspended validation input must fail fast; elapsed {suspended_elapsed:?}"
    );

    for _ in 0..2 {
        carina_core::executor::prepare_provider_ready_config(&factory, &clean_config())
            .expect("the healthy input must remain usable while the slow input is suspended");
        let started = Instant::now();
        carina_core::executor::prepare_provider_ready_config(
            &factory,
            &resolved_provider_config(factory.name(), slow_attributes()),
        )
        .expect_err("alternating back to the suspended input must keep failing fast");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the slow input must consume at most two wall budgets in total"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_validate_config_trap_rebuilds_init_instance() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let attributes = indexmap::IndexMap::from([(
        "__mock_validate_config_trap__".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    )]);
    let trap_config = resolved_provider_config(factory.name(), attributes);

    let error = carina_core::executor::prepare_provider_ready_config(&factory, &trap_config)
        .expect_err("the mock validate_config hook must trap");
    let rendered = error.to_string();
    assert!(rendered.contains("validate_config"), "{rendered}");
    assert!(rendered.contains("WASM guest trapped"), "{rendered}");

    let clean_config = resolved_provider_config(factory.name(), indexmap::IndexMap::new());
    carina_core::executor::prepare_provider_ready_config(&factory, &clean_config)
        .expect("validate_config should rebuild the trapped init instance");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_validation_rebuild_preserves_http_world() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let original_world = factory
        .validation_instance_world_for_test()
        .await
        .expect("the loaded validation instance should be live");
    assert_eq!(original_world, "http-enabled");

    let trap_config = resolved_provider_config(
        factory.name(),
        indexmap::IndexMap::from([(
            "__mock_validate_config_trap__".to_string(),
            Value::Concrete(ConcreteValue::Bool(true)),
        )]),
    );
    carina_core::executor::prepare_provider_ready_config(&factory, &trap_config)
        .expect_err("the mock validate_config hook must poison the validation instance");
    assert_eq!(
        factory.validation_instance_world_for_test().await,
        None,
        "a trapped validation call must leave no live instance before the rebuild"
    );

    let clean_config = resolved_provider_config(factory.name(), indexmap::IndexMap::new());
    carina_core::executor::prepare_provider_ready_config(&factory, &clean_config)
        .expect("the next validation call should rebuild the poisoned instance");
    let rebuilt_world = factory
        .validation_instance_world_for_test()
        .await
        .expect("the rebuilt validation instance should be live");

    assert_eq!(rebuilt_world, original_world);
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_validate_custom_type_rebuilds_after_validate_config_timeout() {
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

    carina_core::executor::prepare_provider_ready_config(&factory, &config)
        .expect_err("validate_config should time out before custom-type validation");

    let factories: Arc<Vec<Box<dyn ProviderFactory>>> = Arc::new(vec![Box::new(factory)]);
    let schemas = carina_core::provider::collect_schemas(&factories);
    let schema = schemas
        .get(
            "mock",
            "test.recovery_probe",
            carina_core::schema::SchemaKind::Resource,
        )
        .expect("the recovery probe schema must cross the host boundary");
    let custom_types =
        carina_core::provider::provider_custom_types_from_factories(Arc::clone(&factories));
    schema
        .validate_with_resolver(
            &HashMap::<String, Value>::from([(
                "value".to_string(),
                Value::Concrete(ConcreteValue::String("clean".into())),
            )]),
            &|_| false,
            &custom_types,
        )
        .expect("validate_custom_type should rebuild the timed-out init instance");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "test-utils")]
async fn test_wasm_repeated_validate_custom_type_timeout_suspends_only_that_value() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let factory = factory.with_guest_call_budget(GuestCallBudget::new(
        Duration::from_secs(30),
        Duration::from_secs(2),
    ));
    let factories: Arc<Vec<Box<dyn ProviderFactory>>> = Arc::new(vec![Box::new(factory)]);
    let schemas = carina_core::provider::collect_schemas(&factories);
    let schema = schemas
        .get(
            "mock",
            "test.timing_probe",
            carina_core::schema::SchemaKind::Resource,
        )
        .expect("the timing probe schema must cross the host boundary");
    let custom_types =
        carina_core::provider::provider_custom_types_from_factories(Arc::clone(&factories));
    let validate = |value: &str| {
        schema.validate_with_resolver(
            &HashMap::<String, Value>::from([(
                "value".to_string(),
                Value::Concrete(ConcreteValue::String(value.to_string().into())),
            )]),
            &|_| false,
            &custom_types,
        )
    };

    for attempt in 1..=2 {
        let started = Instant::now();
        let error = validate("__mock_validate_custom_type_hang__")
            .expect_err("the mock custom-type hook must exceed the wall-clock budget")
            .into_iter()
            .next()
            .expect("custom-type validation must return an error");
        let elapsed = started.elapsed();

        let provider_error = match &error {
            carina_core::schema::TypeError::CustomTypeValidation { error, .. } => error
                .provider_error()
                .expect("WASM validation must retain the typed provider error"),
            other => panic!("expected a custom-type validation error, got {other:?}"),
        };
        assert_eq!(
            provider_error.kind(),
            ProviderErrorKind::Timeout,
            "attempt {attempt}: {error}"
        );
        assert!(
            error.to_string().contains("validate_custom_type"),
            "attempt {attempt}: {error}"
        );
        assert!(
            elapsed >= Duration::from_millis(1_500) && elapsed < Duration::from_secs(6),
            "attempt {attempt} should consume about the 2s wall budget; elapsed {elapsed:?}"
        );
    }

    let suspended_started = Instant::now();
    let suspended = validate("__mock_validate_custom_type_hang__")
        .expect_err("the repeatedly timed-out custom-type value must be suspended")
        .into_iter()
        .next()
        .expect("suspended validation must return an error");
    let suspended_elapsed = suspended_started.elapsed();
    assert!(matches!(
        suspended.host_cause(),
        Some(carina_core::provider::HostErrorCause::ValidationSuspended(
            _
        ))
    ));
    assert!(
        suspended_elapsed < Duration::from_secs(1),
        "the suspended value must fail fast; elapsed {suspended_elapsed:?}"
    );

    validate("healthy-value").expect("a different custom-type value must still validate");
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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

    let first_operation = provider.provider().create_for_test(&first_id, first);
    let second_operation = async {
        tokio::time::sleep(Duration::from_millis(25)).await;
        provider
            .provider()
            .create_for_test(&second_id, second)
            .await
    };
    let fast_operation = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let queued_at = Instant::now();
        let result = provider.provider().create_for_test(&fast_id, fast).await;
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");

    assert_eq!(provider.provider().name(), "mock");

    // Read before create - should return a state with no identifier and empty attributes
    let id = ResourceId::with_provider_identity("mock", "test.resource", "my-resource", None);
    let state = provider
        .provider()
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
            .provider()
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .expect("create should succeed"),
    );
    let created_identifier = AppliedIdentifier::new(
        created
            .identifier
            .clone()
            .expect("a successful create has an identifier"),
    )
    .expect("the created identifier is non-empty");
    assert_minted_mock_identifier(created_identifier.as_str());
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
        .provider()
        .read(&id, &created_identifier, ReadRequest)
        .await
        .expect("read should not error")
        .into_state();
    assert_eq!(
        read_state.identifier.as_deref(),
        Some(created_identifier.as_str())
    );
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "non-finite-read", None);
    let identifier = AppliedIdentifier::new("__mock_non_finite_read__")
        .expect("the non-finite read fixture identifier is non-empty");

    let error = provider
        .provider()
        .read(&id, &identifier, ReadRequest)
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
            .provider()
            .create_for_test(&id, create_request_for_test(resource.clone()).await)
            .await
            .expect("create should succeed"),
    );
    assert_eq!(
        created.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("red".into())))
    );

    // Prepare all four patch variants through the checked core seam.
    let created_identifier = AppliedIdentifier::new(created.identifier.clone().unwrap()).unwrap();
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
        .provider()
        .update(&id, &created_identifier, request)
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

    let updated_identifier = AppliedIdentifier::new(updated.identifier.clone().unwrap()).unwrap();
    // Read to verify update persisted in memory
    let read_state = provider
        .provider()
        .read(&id, &updated_identifier, ReadRequest)
        .await
        .expect("read should not error")
        .into_state();
    assert_eq!(
        read_state.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("blue".into())))
    );

    // Delete
    provider
        .provider()
        .delete(&id, &updated_identifier, delete_request_without_schema())
        .await
        .expect("delete should succeed");

    // Read after delete - should return empty state (no identifier, no attributes)
    let deleted_state = provider
        .provider()
        .read(&id, &updated_identifier, ReadRequest)
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
    let created = provider
        .provider()
        .create_for_test(&id, create_request_for_test(resource.clone()).await)
        .await
        .expect("create should seed the mock cloud store");
    let created_identifier = created.identifier().clone();

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
    .with_identifier(created_identifier.as_str());
    resource.set_attr(
        "color",
        Value::Concrete(ConcreteValue::String("blue".into())),
    );
    let request = update_request_for_test(resource, from, &["color".to_string()]).await;
    let updated = provider
        .provider()
        .update(&id, &created_identifier, request)
        .await
        .expect("update should preserve the unchanged persisted attribute")
        .into_state_for_writeback();
    let updated_identifier = AppliedIdentifier::new(updated.identifier.unwrap()).unwrap();

    let read = provider
        .provider()
        .read(&id, &updated_identifier, ReadRequest)
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
    let mut resources = provider_resources(vec![{
        let mut r = Resource::with_provider("mock", "test.resource", "norm-test", None);
        r.attributes = indexmap::IndexMap::from([(
            "key".into(),
            Value::Concrete(ConcreteValue::String("value".into())),
        )]);
        r
    }]);
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
    let mut resources = provider_resources(vec![
        Resource::with_provider("mock", "test.resource", "enum-echo", None).with_attribute(
            "alias_target",
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "hosted_zone_id".to_string(),
                Value::Concrete(ConcreteValue::CanonicalEnum(hosted_zone)),
            )]))),
        ),
    ]);

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

    let peer_ref = root_resource_ref("peer", "identifier");
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
            DeferredValue::Unknown(ApplyTimeUnknown::UpstreamAllExports {
                binding: absolute_binding_address_for_test("pending"),
            }),
        )])),
    );
    let mut resources = provider_resources(vec![resource]);
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
                [Value::Deferred(DeferredValue::Unknown(
                    ApplyTimeUnknown::UpstreamAllExports { binding }
                ))] if binding.as_str() == "pending"
            )
    ));
    assert_eq!(
        resources[0].attributes.keys().collect::<Vec<_>>(),
        vec!["tags", "pending"],
        "ref/Unknown-bearing attributes must be restored unchanged after the guest call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_normalize_desired_rejects_absent_required_together_attribute() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");
    let mut resource = Resource::with_provider(
        "mock",
        "test.normalize_required_together",
        "absent-companion",
        None,
    );
    resource.set_attr(
        "primary",
        Value::Concrete(ConcreteValue::String("configured".into())),
    );
    let mut resources = provider_resources(vec![resource]);

    let error = normalizer
        .normalize_desired(&mut resources)
        .await
        .expect_err("an absent companion must fail during desired normalization");

    assert_eq!(error.variant_name(), "invalid_input");
    assert_eq!(
        error.detail().context(),
        "test.normalize_required_together companion is required when primary is present"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_normalize_desired_accepts_and_restores_deferred_required_together_attribute() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");
    let mut resource = Resource::with_provider(
        "mock",
        "test.normalize_required_together",
        "deferred-companion",
        None,
    );
    resource.set_attr(
        "primary",
        Value::Concrete(ConcreteValue::String("configured".into())),
    );
    let deferred = root_resource_ref("upstream", "identifier");
    resource.set_attr("companion", deferred.clone());
    let mut resources = provider_resources(vec![resource]);

    normalizer
        .normalize_desired(&mut resources)
        .await
        .expect("a deferred companion must postpone the required-together check");

    assert_eq!(resources[0].get_attr("companion"), Some(&deferred));
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
        &carina_core::differ::exact_comparison_context_for_tests(),
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
        .create_initialized_provider(None, &ready_config(&factory, &attributes))
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
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let trap_id =
        ResourceId::with_provider_identity("mock", "test.resource", "__mock_create_trap__", None);
    let trap_resource =
        Resource::with_provider("mock", "test.resource", "__mock_create_trap__", None);

    let initial_error = provider
        .provider()
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
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let error_id =
        ResourceId::with_provider_identity("mock", "test.resource", "__mock_create_error__", None);
    let error_resource =
        Resource::with_provider("mock", "test.resource", "__mock_create_error__", None);

    let error = provider
        .provider()
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
        .provider()
        .read(&followup_id, &mock_identifier(), ReadRequest)
        .await
        .expect("a provider-returned error must not poison the shared instance")
        .into_state();
    assert!(!state.exists);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_provider_returned_wit_error_does_not_poison_fail_fast_instance() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should initialize");
    let error_id = ResourceId::with_provider_identity("mock", "test.resource", "read-error", None);
    let error_identifier = AppliedIdentifier::new("__mock_read_error__")
        .expect("the read-error fixture identifier is non-empty");

    let error = provider
        .provider()
        .read(&error_id, &error_identifier, ReadRequest)
        .await
        .expect_err("the mock read hook must return a WIT provider-error");
    assert_eq!(error.variant_name(), "internal");
    assert_eq!(error.detail().context(), "intentional mock read error");

    let followup_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-read-error", None);
    let state = provider
        .provider()
        .read(&followup_id, &mock_identifier(), ReadRequest)
        .await
        .expect("a provider-returned WIT error must not poison the FailFast instance")
        .into_state();
    assert!(!state.exists);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_provider_normalizer_errors_are_structured_and_do_not_poison_instance() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let attributes = indexmap::IndexMap::new();
    let provider = factory
        .create_initialized_provider(None, &ready_config(&factory, &attributes))
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
    let mut desired_resources = provider_resources(vec![Resource::with_provider(
        "mock",
        "test.resource",
        "__mock_normalize_desired_error__",
        None,
    )]);
    desired_resources[0].set_attr("peer", root_resource_ref("peer", "identifier"));
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

    let mut recovery_resources = provider_resources(vec![Resource::with_provider(
        "mock",
        "test.resource",
        "after-normalize-desired-error",
        None,
    )]);
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
        .provider()
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
    let mut resources = provider_resources(vec![resource]);
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
    let mut resources = provider_resources(vec![resource]);

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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
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
    let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
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
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "example", None);

    assert_eq!(
        provider
            .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "__mock_required_permissions_trap__",
        None,
    );

    let error = provider
        .provider()
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
        .create_initialized_provider(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "__mock_invalid_satisfier_hint__",
        None,
    );

    let error = provider
        .provider()
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
