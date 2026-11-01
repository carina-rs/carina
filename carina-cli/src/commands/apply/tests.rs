use super::*;
use carina_core::binding_index::LivePreApplyInputs;
use carina_core::data_source_value::{DataSourceValue, DataSourceValues};
use carina_core::effect::{Effect, UnresolvedSavedEffect};
use carina_core::provider::{
    AppliedIdentifier, AppliedState, BoxFuture, CreateRequest, DeleteRequest, NoopNormalizer,
    PatchOp, Provider, ProviderError, ProviderFactory, ProviderNormalizer, ProviderResult,
    ReadOutcome, ReadRequest, UpdateRequest,
};
use carina_core::resource::{
    DataSource, DeferredValue, LiveComposition, LiveCompositionArgument, LiveCompositionSignature,
    ModuleConstraintId, PendingModuleConstraint, PlaceholderKind, ResolvedDataSource,
    ResolvedResource, Resource, ResourceId, ResourceIdentity,
};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
use carina_core::schema::{
    PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
};
use carina_state::{DeposedInstance, DeposedKey, NameOverride, ResourceState};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "tests/cancellation_fixture.rs"]
mod cancellation_fixture;

use cancellation_fixture::ApplyCancellationFixture;

use crate::commands::plan::PlanFileContents;
use crate::commands::shared::cancellation_test_support::MOCK_PROVIDER_ENV_LOCK;
use crate::commands::shared::state_writeback::{ApplyStateSave, build_state_after_apply};
use crate::fixture_read_outcome;
use crate::wiring::{
    OverrideAwareBuildMode, OverrideAwareResources, ParserCanonicalResourcePair,
    resolve_config_block_names_for_test,
};

struct UnusedAwsCredentialProvisioner;

impl carina_plugin_host::AwsCredentialProvisioner for UnusedAwsCredentialProvisioner {
    fn provision(
        &self,
    ) -> BoxFuture<
        '_,
        Result<
            Arc<dyn carina_plugin_host::AwsCredentialSource>,
            carina_plugin_host::AwsCredentialError,
        >,
    > {
        Box::pin(async { panic!("an untrusted test provider must not provision AWS credentials") })
    }
}

fn validate_with_test_factories(
    parsed: &carina_core::parser::InferredFile,
    base_dir: &Path,
    skip_resource_validation: bool,
    factories: Vec<Box<dyn ProviderFactory>>,
    load_errors: HashMap<String, String>,
    duplicate_declarations: &[carina_core::config_loader::DuplicateDeclaration],
) -> Vec<AppError> {
    let providers = crate::wiring::ProviderLoad::for_test_with_load_errors(
        crate::wiring::LoadedProviderFactory::for_test(factories),
        &parsed.providers,
        load_errors,
    );
    crate::commands::validate_and_resolve_errors(
        parsed,
        base_dir,
        &providers,
        skip_resource_validation,
        duplicate_declarations,
    )
}

fn complete_create(state: carina_core::resource::State) -> carina_core::executor::Applied {
    carina_core::executor::Applied::Complete {
        state,
        request: carina_core::executor::AcceptedRequest::create_for_test(),
    }
}

fn resolve_module_file_for_test<E: carina_core::parser::ExportParamLike>(
    parsed: carina_core::parser::File<
        E,
        carina_core::resource::ParserPlaceholder,
        carina_core::resource::ParserPlaceholder,
        carina_core::parser::ParsedDeferredForPhase<carina_core::resource::ParserPlaceholder>,
    >,
) -> carina_core::parser::ModuleResolvedFile<E> {
    let base_dir = parsed
        .module_calls
        .first()
        .and_then(|call| call.source_file().parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    carina_core::module_resolver::resolve_modules_with_providerless_config(
        parsed,
        &base_dir,
        &carina_core::parser::ProviderContext::default(),
        &carina_core::schema::SchemaRegistry::new(),
    )
    .expect("test fixture crosses module resolution")
}

fn resolve_file_for_test<E>(
    parsed: carina_core::parser::File<
        E,
        carina_core::resource::ParserPlaceholder,
        carina_core::resource::ParserPlaceholder,
        carina_core::parser::ParsedDeferredForPhase<carina_core::resource::ParserPlaceholder>,
    >,
) -> carina_core::parser::ExpandedFile<E::Expanded>
where
    E: carina_core::parser::ExportParamLike + carina_core::parser::IntoExpandedExportParam,
{
    resolve_module_file_for_test(parsed)
        .into_expanded()
        .expect("test fixture must contain no parser placeholders")
}

fn complete_update(
    state: carina_core::resource::State,
    sent: &[&str],
    removed: &[&str],
) -> carina_core::executor::Applied {
    carina_core::executor::Applied::Complete {
        state,
        request: carina_core::executor::AcceptedRequest::update_for_test(sent, removed),
    }
}

fn canonical_unresolved_for_test(
    ctx: &WiringContext,
    parsed: &carina_core::parser::InferredFile,
    unresolved: &carina_core::config_loader::UnresolvedConfigSnapshot,
) -> ParserCanonicalResourcePair<carina_core::parser::InferredExportParam> {
    resolve_config_block_names_for_test(
        resolve_module_file_for_test(parsed.clone()),
        unresolved.clone(),
        ctx.schemas(),
    )
    .unwrap_or_else(|errors| {
        panic!(
            "test block-name resolution failed: {}",
            errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        )
    })
}

fn canonical_resolved_unresolved_for_test(
    ctx: &WiringContext,
    parsed: &carina_core::parser::ModuleResolvedInferredFile,
    unresolved: &carina_core::config_loader::UnresolvedConfigSnapshot,
) -> ParserCanonicalResourcePair<carina_core::parser::InferredExportParam> {
    resolve_config_block_names_for_test(parsed.clone(), unresolved.clone(), ctx.schemas())
        .unwrap_or_else(|errors| {
            panic!(
                "test block-name resolution failed: {}",
                errors
                    .into_iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
}

fn canonical_resources_for_test(
    resources: Vec<Resource>,
    _schemas: &SchemaRegistry,
) -> crate::wiring::CanonicalResourcePair<
    carina_core::parser::ParsedExportParam<carina_core::resource::NoPlaceholder>,
> {
    let mut parsed = carina_core::parser::ExpandedParsedFile::default();
    parsed.resources = resources.clone();
    crate::wiring::CanonicalResourcePair::for_test(parsed, resources)
}

fn validated_apply_config_for_test(
    wiring: &ValidatedWiring,
    parsed: &carina_core::parser::InferredFile,
    unresolved: &carina_core::config_loader::UnresolvedConfigSnapshot,
) -> ValidatedConfig {
    let resources = canonical_unresolved_for_test(wiring.context(), parsed, unresolved);
    wiring.validated_parser_pair_for_test(resources)
}

fn validated_resolved_apply_config_for_test(
    wiring: &ValidatedWiring,
    parsed: &carina_core::parser::ModuleResolvedInferredFile,
    unresolved: &carina_core::config_loader::UnresolvedConfigSnapshot,
) -> ValidatedConfig {
    let resources = canonical_resolved_unresolved_for_test(wiring.context(), parsed, unresolved)
        .into_expanded_for_test()
        .expect("resolved test pair must contain no parser placeholders");
    ValidatedConfig::from_wiring_ref_for_test(wiring, resources)
}

fn validated_reload_owner_for_test(wiring: &ValidatedWiring) -> ValidatedConfig {
    let parsed = resolve_module_file_for_test(carina_core::parser::InferredFile::default());
    let unresolved = carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(
        carina_core::parser::ParsedFile::default(),
    );
    let resources = canonical_resolved_unresolved_for_test(wiring.context(), &parsed, &unresolved)
        .into_expanded_for_test()
        .expect("resolved reload pair must contain no parser placeholders");
    ValidatedConfig::from_wiring_ref_for_test(wiring, resources)
}

macro_rules! unexpected_resume_create {
    () => {
        fn resume_create(
            &self,
            _request: carina_core::provider::ResumeCreateRequest,
        ) -> BoxFuture<
            '_,
            Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
        > {
            Box::pin(async { panic!("unexpected resume_create") })
        }
    };
}

fn mock_http_wasm_path() -> Option<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    [
        "carina_provider_mock_http.wasm",
        "carina-provider-mock-http.wasm",
    ]
    .into_iter()
    .map(|name| workspace_root.join("target/wasm32-wasip2/debug").join(name))
    .find(|path| path.exists())
}

fn mock_http_test_egress(
    load: &carina_provider_resolver::VerifiedLoad,
    lock_dir: &Path,
) -> carina_provider_resolver::LockedEgress {
    let declaration = carina_provider_resolver::VerifiedEgressDeclaration::read(load)
        .expect("mock HTTP provider must carry a valid egress declaration");
    let canonical_source = declaration.source().to_string();
    let source = canonical_source
        .strip_prefix("file://")
        .map(|path| format!("file://{}", lock_dir.join(path).display()))
        .unwrap_or(canonical_source);
    let hosts = declaration
        .patterns()
        .iter()
        .map(|pattern| format!("    {:?},", pattern.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!hosts.is_empty(), "mock HTTP provider must declare egress");
    let lock = format!(
        "version = 5\n\n[[provider]]\nname = \"mock_http\"\nsource = {source:?}\nmode = \"file\"\nsha256 = {:?}\n\n[provider.egress]\nconsent = \"explicit\"\nhosts = [\n{hosts}\n]\n",
        declaration.sha256().as_str(),
    );
    std::fs::write(lock_dir.join("carina-providers.lock"), lock)
        .expect("write mock HTTP egress consent");
    carina_provider_resolver::load_locked_egress(lock_dir, load)
        .expect("load mock HTTP egress consent")
        .expect("mock HTTP egress consent must match the fixture")
}

fn spawn_mock_http_server(
    expected_requests: usize,
) -> Option<(SocketAddr, std::thread::JoinHandle<Vec<usize>>)> {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("SKIP: sandbox cannot bind the loopback test server: {error}");
            return None;
        }
    };
    let address = listener.local_addr().expect("loopback listener address");
    listener
        .set_nonblocking(true)
        .expect("set loopback listener nonblocking");
    let server = std::thread::spawn(move || {
        let mut request_sizes = Vec::with_capacity(expected_requests);
        let deadline = Instant::now() + Duration::from_secs(30);
        while request_sizes.len() < expected_requests {
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
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set server read timeout");
            let mut request = vec![0_u8; 32 * 1024];
            let bytes_read = stream.read(&mut request).expect("read dispatched request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("write HTTP response");
            request_sizes.push(bytes_read);
        }
        request_sizes
    });
    Some((address, server))
}

fn mock_http_resource(identity: &str, address: SocketAddr, behavior: &str) -> Resource {
    Resource::with_provider(
        "mock_http",
        "test.send_phase",
        ResourceIdentity::try_from(identity.to_string()).expect("non-empty fixture identity"),
        None,
    )
    .with_attribute(
        "url",
        Value::Concrete(ConcreteValue::String(
            format!("http://phase-test.invalid:{}/", address.port()).into(),
        )),
    )
    .with_attribute(
        "method",
        Value::Concrete(ConcreteValue::String("POST".into())),
    )
    .with_attribute(
        "body",
        Value::Concrete(ConcreteValue::String("x".repeat(16 * 1024).into())),
    )
    .with_attribute(
        "create_behavior",
        Value::Concrete(ConcreteValue::String(behavior.to_string().into())),
    )
}

fn mock_http_ready_config(
    factory: &dyn ProviderFactory,
) -> carina_core::provider::ProviderReadyConfig {
    let mut parsed = carina_core::parser::ExpandedInferredFile::default();
    parsed.providers.push(carina_core::parser::ProviderConfig {
        name: factory.name().to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    });
    let resolved = carina_core::parser::resolve_provider_attributes_with_remote(
        &parsed,
        &HashMap::new(),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("mock HTTP provider config must resolve");
    carina_core::executor::prepare_provider_ready_config(
        factory,
        resolved.iter().next().expect("one provider config"),
    )
    .expect("mock HTTP provider config must validate")
}

fn mock_http_replacement_schema() -> carina_core::schema::ValidatedResourceSchema {
    PatternResourceSchema::new("test.send_phase")
        .attribute(PatternAttributeSchema::required("url", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::required("method", PatternAttributeType::string()).build(),
        )
        .attribute(PatternAttributeSchema::required("body", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::provider_populated(
                "send_phase",
                PatternAttributeType::string(),
            )
            .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("create_behavior", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .with_coexisting_replacement()
        .validate()
        .expect("mock HTTP replacement schema must validate")
}

struct OutcomeUnknownExecutionObserver;

impl carina_core::executor::ExecutionObserver for OutcomeUnknownExecutionObserver {
    fn on_event(&self, _event: &carina_core::executor::ExecutionEvent<'_>) {}
}

async fn execute_mock_http_plan(
    provider: &ProviderRouter,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
    plan: &Plan,
    managed_current: HashMap<ResourceId, carina_core::resource::Current>,
) -> carina_core::executor::ExecutionResult {
    let outcome = carina_core::executor::execute_live_plan(
        provider,
        carina_core::executor::LiveExecutionInput {
            plan,
            provider_check_inputs: carina_core::executor::ProviderCheckInputs::Authored,
            compositions: &[],
            bindings: Default::default(),
            managed_current,
            data_source_values: carina_core::data_source_value::ExecutableDataSourceValues::empty(),
            normalizer: &NoopNormalizer,
            provider_configs: &[],
            factories,
            schemas,
            parallelism: std::num::NonZeroUsize::new(1).expect("one is non-zero"),
            precompleted_success_count: 0,
            checkpoint_sink: carina_core::executor::ExecutionCheckpointSink::noop(),
        },
        &OutcomeUnknownExecutionObserver,
        carina_core::shutdown::ShutdownToken::running(),
    )
    .await;
    let carina_core::executor::ExecutionOutcome::Completed(result) = outcome else {
        panic!("an uncancelled mock HTTP execution must complete");
    };
    result
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_trap_after_egress_writeback_preserves_fresh_and_replacement_recovery_state() {
    let Some(wasm_path) = mock_http_wasm_path() else {
        eprintln!(
            "SKIP: mock HTTP WASM binary not found; build carina-provider-mock-http for wasm32-wasip2"
        );
        return;
    };
    let Some((server_address, server)) = spawn_mock_http_server(2) else {
        return;
    };
    let cache = tempfile::tempdir().expect("create WASM cache directory");
    let wasm_bytes = std::fs::read(wasm_path).expect("read mock HTTP WASM provider");
    let fresh_verified = carina_provider_resolver::VerifiedLoad::for_test(
        &wasm_bytes,
        carina_provider_resolver::ProviderLoadIdentity::for_test(
            "mock_http",
            carina_provider_resolver::ProviderTrustClass::Untrusted,
        ),
    );
    let locked_egress = mock_http_test_egress(&fresh_verified, cache.path());
    let fresh_factory =
        carina_plugin_host::WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
            fresh_verified,
            cache.path(),
            Some(&locked_egress),
            &UnusedAwsCredentialProvisioner,
        )
        .await
        .expect("load mock HTTP WASM provider")
        .with_http_test_route("phase-test.invalid", server_address);
    let fresh_ready = mock_http_ready_config(&fresh_factory);
    let fresh_initialized = fresh_factory
        .create_initialized_provider(None, &fresh_ready)
        .await
        .expect("initialize mock HTTP provider");
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(fresh_factory)];
    let mut schemas = carina_core::provider::collect_schemas(&factories);
    schemas.insert("mock_http", mock_http_replacement_schema());
    let mut fresh_provider = ProviderRouter::new();
    fresh_provider.add_provider("mock_http".to_string(), fresh_initialized);

    let fresh_resource = mock_http_resource("fresh-trap", server_address, "trap_after_send");
    let fresh_id = fresh_resource.id.clone();
    let fresh_resources = vec![ResolvedResource::new(fresh_resource)];
    let mut fresh_plan = Plan::new();
    fresh_plan.add(Effect::Create(fresh_resources[0].clone()));
    let fresh_result = execute_mock_http_plan(
        &fresh_provider,
        &factories,
        &schemas,
        &fresh_plan,
        HashMap::new(),
    )
    .await;
    let fresh_entry = fresh_result.applied_states.get(&fresh_id);
    assert!(
        matches!(
            fresh_entry,
            Some(carina_core::executor::Applied::CreateOutcomeUnknown(unknown))
                if unknown.id() == &fresh_id
        ),
        "{fresh_entry:?}"
    );
    let fresh_state = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &fresh_resources,
            runtime_synthesized_resources: &fresh_result.runtime_synthesized_resources,
            managed_current: &fresh_result.managed_current,
            applied_states: &fresh_result.applied_states,
            plan: &fresh_plan,
            successfully_deleted: &fresh_result.successfully_deleted,
            failed_refreshes: &fresh_result.failed_refreshes,
            schemas: &schemas,
        },
        None,
    )
    .expect("fresh unknown outcome must be written to state");
    assert!(matches!(
        fresh_state
            .find_resource_for_id(&fresh_id)
            .expect("fresh state row")
            .current(),
        carina_state::CurrentInstance::CreateOutcomeUnknown { .. }
    ));

    // A guest trap permanently poisons its configured runtime instance. Use a
    // separately loaded factory so this half exercises replacement handling,
    // not the poisoned instance's fail-fast path.
    let replacement_verified = carina_provider_resolver::VerifiedLoad::for_test(
        &wasm_bytes,
        carina_provider_resolver::ProviderLoadIdentity::for_test(
            "mock_http",
            carina_provider_resolver::ProviderTrustClass::Untrusted,
        ),
    );
    let replacement_factory =
        carina_plugin_host::WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
            replacement_verified,
            cache.path(),
            Some(&locked_egress),
            &UnusedAwsCredentialProvisioner,
        )
        .await
        .expect("load replacement mock HTTP WASM provider")
        .with_http_test_route("phase-test.invalid", server_address);
    let replacement_ready = mock_http_ready_config(&replacement_factory);
    let replacement_initialized = replacement_factory
        .create_initialized_provider(None, &replacement_ready)
        .await
        .expect("initialize replacement mock HTTP provider");
    let mut replacement_provider = ProviderRouter::new();
    replacement_provider.add_provider("mock_http".to_string(), replacement_initialized);

    let mut desired = mock_http_resource("replacement-trap", server_address, "trap_after_send");
    desired.directives.create_before_destroy = true;
    let replacement_id = desired.id.clone();
    let old_resource = mock_http_resource("replacement-trap", server_address, "normal");
    let old_state = State::existing(
        replacement_id.clone(),
        old_resource.attributes.into_iter().collect(),
    )
    .with_identifier("old-applied-id");
    let current_states = HashMap::from([(replacement_id.clone(), old_state)]);
    let replacement_resources = vec![ResolvedResource::new(desired)];
    let replacement_raw_resources = replacement_resources
        .iter()
        .map(|resource| resource.as_resource().clone())
        .collect::<Vec<_>>();
    let plan_input_current = crate::plan_input_current_from_states(
        current_states.clone(),
        &schemas,
        &replacement_raw_resources,
    );
    let replacement_plan = carina_core::differ::create_plan(
        &replacement_resources,
        &[],
        &replacement_provider,
        carina_core::differ::PlanPriorStates::new(
            &plan_input_current,
            &carina_core::differ::Orphans::new(),
        ),
        &HashMap::new(),
        &schemas,
        &crate::empty_lifted_saved_attrs(),
        &HashMap::new(),
        &[],
    )
    .expect("create-only behavior change must plan a replacement");
    assert!(
        replacement_plan
            .replace_display_info()
            .next()
            .is_some_and(|replacement| replacement.create_before_destroy),
        "fixture must be a create-before-destroy replacement: {:?}",
        replacement_plan.effects()
    );

    let mut initial_state = StateFile::new();
    let old_url = format!("http://phase-test.invalid:{}/", server_address.port());
    let old_row = ResourceState::try_applied(
        "test.send_phase",
        "replacement-trap",
        "mock_http",
        "old-applied-id",
    )
    .expect("old identifier is valid")
    .with_attribute("url", serde_json::json!(old_url))
    .with_attribute("method", serde_json::json!("POST"))
    .with_attribute("body", serde_json::json!("x".repeat(16 * 1024)))
    .with_attribute("create_behavior", serde_json::json!("normal"));
    initial_state
        .upsert_resource(old_row)
        .expect("insert old applied state");

    let replacement_result = execute_mock_http_plan(
        &replacement_provider,
        &factories,
        &schemas,
        &replacement_plan,
        crate::managed_current_from_states(current_states),
    )
    .await;
    let replacement_entry = replacement_result.applied_states.get(&replacement_id);
    assert!(
        matches!(
            replacement_entry,
            Some(carina_core::executor::Applied::CreateOutcomeUnknown(unknown))
                if unknown.id() == &replacement_id
        ),
        "{replacement_entry:?}"
    );
    let mut replacement_state = build_state_after_apply(
        ApplyStateSave {
            state_file: initial_state,
            sorted_resources: &replacement_resources,
            runtime_synthesized_resources: &replacement_result.runtime_synthesized_resources,
            managed_current: &replacement_result.managed_current,
            applied_states: &replacement_result.applied_states,
            plan: &replacement_plan,
            successfully_deleted: &replacement_result.successfully_deleted,
            failed_refreshes: &replacement_result.failed_refreshes,
            schemas: &schemas,
        },
        None,
    )
    .expect("replacement unknown outcome must be written to state");
    let replacement_row = replacement_state
        .find_resource_for_id(&replacement_id)
        .expect("replacement state row");
    assert!(matches!(
        replacement_row.current(),
        carina_state::CurrentInstance::CreateOutcomeUnknown { .. }
    ));
    assert!(
        replacement_row
            .deposed
            .iter()
            .any(|generation| generation.identifier == "old-applied-id"),
        "the old applied identifier must remain as a deposed generation"
    );

    let address = carina_core::parser::StateBlockAddress::new(
        "mock_http",
        "test.send_phase",
        "replacement-trap",
    );
    let forgotten = replacement_state
        .find_outcome_unknown(&address)
        .expect("replacement row must authorize outcome-unknown forget");
    StateFile::forget(forgotten);
    let shell = replacement_state
        .find_resource_for_id(&replacement_id)
        .expect("forget must retain the deposed shell");
    assert!(matches!(
        shell.current(),
        carina_state::CurrentInstance::None
    ));
    assert!(
        shell
            .deposed
            .iter()
            .any(|generation| generation.identifier == "old-applied-id"),
        "state forget must keep the old deposed generation"
    );

    let request_sizes = server.join().expect("loopback server must finish");
    assert_eq!(request_sizes.len(), 2);
    assert!(
        request_sizes.iter().all(|size| *size > 0),
        "both guest creates must dispatch HTTP bytes: {request_sizes:?}"
    );
}

#[test]
fn saved_plan_secret_verification_errors_are_typed_and_distinct() {
    let target = ApplyTarget::SavedPlan {
        plan_file: Path::new("reviewed-plan.json"),
        source_dir: Path::new("infra"),
    };
    let hash = saved_plan_secret_rehydration_error(
        carina_core::value::SecretRehydrationSiteError::new(
            "mock.test.resource.example.token",
            carina_core::value::SecretRehydrationError::HashMismatch,
        ),
        target,
    );
    assert!(matches!(hash, AppError::SavedPlanSecretVerification(_)));
    assert!(hash.to_string().contains("secret changed"));
    assert!(
        std::error::Error::source(&hash).is_none(),
        "classified saved-plan diagnostic must be terminal"
    );

    let shape = saved_plan_secret_rehydration_error(
        carina_core::value::SecretRehydrationSiteError::new(
            "mock.test.resource.example.token",
            carina_core::value::SecretRehydrationError::ShapeMismatch,
        ),
        target,
    );
    assert!(shape.to_string().contains("configuration changed"));
    assert!(shape.to_string().contains("no longer exists"));
    assert!(!shape.to_string().contains("secret changed"));

    let unavailable = saved_plan_secret_evaluation_error(
        AppError::Config("environment variable TOKEN is not set".to_string()),
        target,
    );
    let unavailable_rendered = unavailable.to_string();
    assert!(unavailable_rendered.contains("could not be evaluated"));
    assert_eq!(
        unavailable_rendered
            .matches("environment variable TOKEN is not set")
            .count(),
        1,
        "{unavailable_rendered}"
    );
    assert!(std::error::Error::source(&unavailable).is_none());

    let scope_mismatch = saved_plan_secret_preparation_error(
        AppError::SavedPlanProviderScopeMismatch(Box::new(
            crate::error::SavedPlanProviderScopeMismatchError {
                instance: carina_core::provider::ProviderInstanceKey::new(
                    "test",
                    Some("west".to_string()),
                ),
                recorded: carina_core::provider::ReferenceScope::Unavailable,
                initialized: Some(
                    carina_core::provider::ReferenceScope::available(
                        "aws",
                        "us-west-2",
                        "123456789012",
                    )
                    .expect("fixture scope is valid"),
                ),
                replan_command: "carina plan --out reviewed-plan.json infra".to_string(),
            },
        )),
        target,
    );
    assert!(matches!(
        scope_mismatch,
        AppError::SavedPlanProviderScopeMismatch(_)
    ));

    let serialization = saved_plan_secret_rehydration_error(
        carina_core::value::SecretRehydrationSiteError::new(
            "mock.test.resource.example.token",
            carina_core::value::SecretRehydrationError::Serialization(
                carina_core::value::SerializationError::UnresolvedResourceRef {
                    path: "mock.test.resource.other.token".to_string(),
                    context: carina_core::value::SerializationContext::SecretRedaction,
                },
            ),
        ),
        target,
    );
    assert!(matches!(
        &serialization,
        AppError::SavedPlanSecretVerification(error)
            if matches!(error.as_ref(), SavedPlanSecretVerificationError::Serialization { .. })
    ));
    let serialization_rendered = serialization.to_string();
    let encoding_failure =
        "cannot serialize at secret redaction: unresolved reference mock.test.resource.other.token";
    assert_eq!(
        serialization_rendered.matches(encoding_failure).count(),
        1,
        "{serialization_rendered}"
    );
    assert!(std::error::Error::source(&serialization).is_none());

    let orphan = saved_plan_secret_orphan_vanished_error("mock.test.resource.orphan", target);
    assert!(orphan.to_string().contains("orphan"));
    assert!(orphan.to_string().contains("no longer exists"));
    assert!(
        std::error::Error::source(&orphan).is_none(),
        "classified orphan diagnostic must be terminal"
    );
}

#[tokio::test]
async fn migrated_legacy_reserved_attributes_do_not_report_saved_plan_drift() {
    let json = serde_json::json!({
        "version": 10,
        "serial": 1,
        "lineage": "legacy-reserved-drift",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "legacy",
            "provider": "mock",
            "identifier": "resource-123",
            "attributes": {
                "name": "kept",
                "_binding": "legacy_binding",
            },
        }],
        "exports": {},
    });
    let state_file = carina_state::check_and_migrate(&json.to_string())
        .expect("legacy state should migrate")
        .into_state();
    let resource = Resource::with_provider("mock", "test.Resource", "legacy", None)
        .with_attribute("name", string_value("kept"));
    let id = resource.id.clone();
    let planned_state = match state_file.build_state_for_resource(&id) {
        carina_core::resource::Current::Absent(state) => state.into_state(),
        carina_core::resource::Current::Present(present) => present.into_parts().0,
        carina_core::resource::Current::MissingIdentifier(state) => state.into_state(),
        carina_core::resource::Current::CreateInFlight(_) => {
            panic!("legacy fixture must not produce an in-flight create")
        }
        carina_core::resource::Current::CreateOutcomeUnknown(_) => {
            panic!("legacy fixture must not produce an unknown create outcome")
        }
    };
    assert_eq!(
        planned_state.attributes.keys().collect::<Vec<_>>(),
        vec![&"name".to_string()]
    );

    let shared = ApplyTimeReadShared::default();
    shared.created_roles.lock().unwrap().insert(
        "legacy".to_string(),
        HashMap::from([("name".to_string(), string_value("kept"))]),
    );
    let result = detect_drift_for_test(
        &[resource],
        &[crate::commands::plan::ReloadedCurrentStateEntry::managed(
            id,
            planned_state,
        )],
        &HistoricalResourceAddresses::from_plan(&Plan::new()),
        &ApplyTimeReadProvider { shared },
        None,
        &carina_core::schema::SchemaRegistry::new(),
        ApplyTarget::Project(Path::new(".")),
    )
    .await
    .expect("drift check should succeed");

    assert_eq!(result.drift_messages(), None);
}

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

fn resolved_provider_configs(
    parsed: &carina_core::parser::InferredFile,
) -> carina_core::parser::ResolvedProviderConfigs {
    let parsed = parsed
        .clone()
        .into_expanded()
        .expect("test provider configuration must contain no parser placeholders");
    carina_core::parser::resolve_provider_attributes_with_remote(
        &parsed,
        &HashMap::new(),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("test provider configs must resolve")
}

fn empty_resolved_provider_configs() -> carina_core::parser::ResolvedProviderConfigs {
    carina_core::parser::resolve_provider_attributes_with_remote(
        &carina_core::parser::ExpandedInferredFile::default(),
        &HashMap::new(),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("empty provider configs must resolve")
}

fn expanded_provider_configs(parsed: &carina_core::parser::InferredFile) -> Vec<ProviderConfig> {
    parsed
        .clone()
        .into_expanded()
        .expect("test provider configs must contain no parser placeholders")
        .providers
}

fn inspected_saved_plan(plan_file: PlanFile) -> InspectedPlanFile {
    InspectedPlanFile::inspect(plan_file)
}

trait IntoValidatedTestSchema {
    fn into_validated_test_schema(self) -> carina_core::schema::ValidatedResourceSchema;
}

impl IntoValidatedTestSchema for carina_core::schema::ResourceSchema {
    fn into_validated_test_schema(self) -> carina_core::schema::ValidatedResourceSchema {
        self.validate().expect("schema fixture must be valid")
    }
}

impl IntoValidatedTestSchema for carina_core::schema::ValidatedResourceSchema {
    fn into_validated_test_schema(self) -> carina_core::schema::ValidatedResourceSchema {
        self
    }
}

fn mock_schema_registry<T>(schemas: impl IntoIterator<Item = T>) -> SchemaRegistry
where
    T: IntoValidatedTestSchema,
{
    let mut registry = SchemaRegistry::new();
    for schema in schemas {
        registry.insert("mock", schema.into_validated_test_schema());
    }
    registry
}

fn record_with_test_context(contents: PlanFileContents) -> PlanFile {
    let ctx = ValidatedConfig::from_factories_for_test(
        (),
        Vec::new(),
        contents.provider_configs.as_provider_configs(),
    );
    contents
        .record_for_test(&ctx, Vec::new())
        .expect("test plan provider evidence must be valid")
}

fn record_with_test_schemas(contents: PlanFileContents, schemas: &SchemaRegistry) -> PlanFile {
    let additional_schemas = schemas
        .iter()
        .map(|(provider, _resource_type, _kind, schema)| (provider.to_string(), schema.clone()))
        .collect::<Vec<_>>();
    let ctx = ValidatedConfig::from_factories_with_additional_schemas_for_test(
        (),
        Vec::new(),
        contents.provider_configs.as_provider_configs(),
        additional_schemas,
    );
    contents
        .record_for_test(&ctx, Vec::new())
        .expect("test plan provider evidence must be valid")
}

fn record_with_builtin_mock_schemas(contents: PlanFileContents) -> PlanFile {
    record_with_test_schemas(
        contents,
        &mock_schema_registry(carina_provider_mock::builtin_schemas()),
    )
}

#[test]
fn current_plan_format_round_trip_includes_format_29_loop_provenance_and_deferred_targets() {
    let parsed = carina_core::parser::parse_file(
        r#"
let orgs = upstream_state { source = "../orgs" }

for name in ["one"] {
    mock.test.resource { name = name }
}

let bound_items = for account_id in orgs.accounts {
    mock.test.resource { name = account_id }
}

for account_id in orgs.accounts {
    mock.test.resource { name = account_id }
}
"#,
        Path::new("main.crn"),
        &ProviderContext::default(),
    )
    .expect("parse saved-plan loop provenance fixture");
    let parsed = carina_core::module_resolver::resolve_modules_providerless(parsed, Path::new("."))
        .expect("cross the deferred-body resolution seam")
        .into_expanded()
        .expect("saved-plan loop fixture must contain only loop slots");

    let mut deferred = parsed.deferred_for_expressions().to_vec().into_iter();

    let mut resource = parsed
        .resources
        .into_iter()
        .next()
        .expect("concrete anonymous-loop resource");
    resource.id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "mock_test_resource_96081a11",
        None,
    );
    let origin = resource
        .loop_provenance
        .display_origin()
        .expect("concrete loop provenance")
        .clone();

    let bound = deferred.next().expect("bound deferred loop");
    let unbound = deferred.next().expect("unbound deferred loop");
    assert!(matches!(
        bound.target(),
        carina_core::parser::DeferredForTarget::Bound(target)
            if target.binding() == "bound_items"
    ));
    assert!(matches!(
        unbound.target(),
        carina_core::parser::DeferredForTarget::Unbound { .. }
    ));

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(resource.clone())));
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "legacy",
            None,
        )),
        to: carina_core::resource::ResolvedResourceId::new(resource.id.clone()),
        origin: Some(origin.clone()),
    });
    plan.add(Effect::DeferredCreate(
        carina_core::effect::DeferredCreatePayload::new(Box::new(bound)),
    ));
    plan.add(Effect::DeferredCreate(
        carina_core::effect::DeferredCreatePayload::new(Box::new(unbound)),
    ));

    let plan_file = record_with_builtin_mock_schemas(PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-30T00:00:00Z".to_string(),
        source_path: "infra".to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: saved_provider_configs(vec![ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        }]),
        backend_config: None,
        plan,
        sorted_resources: vec![resource.clone()],
        unresolved_resources: vec![resource],
        compositions: Vec::new(),
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: Vec::new(),
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    });

    let encoded = serde_json::to_value(&plan_file).expect("serialize current-format plan");
    assert_eq!(
        encoded["version"],
        serde_json::json!(PlanFile::CURRENT_VERSION)
    );
    let decoded: PlanFile =
        serde_json::from_value(encoded).expect("deserialize current-format saved plan");
    let decoded = decoded.into_contents();

    assert_eq!(decoded.version, PlanFile::CURRENT_VERSION);
    assert_eq!(
        decoded.sorted_resources[0]
            .loop_provenance
            .display_origin()
            .expect("resource origin after round-trip")
            .display_suffix(),
        origin.display_suffix(),
    );
    let move_origin = decoded
        .plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            UnresolvedSavedEffect::Move { origin, .. } => origin.as_ref(),
            _ => None,
        })
        .expect("Move origin after round-trip");
    assert_eq!(move_origin.display_suffix(), origin.display_suffix());

    let targets = decoded
        .plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            UnresolvedSavedEffect::DeferredCreate(payload) => Some(payload.template.target()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(targets.iter().any(|target| matches!(
        target,
        carina_core::parser::DeferredForTarget::Bound(bound)
            if bound.binding() == "bound_items"
    )));
    assert!(targets.iter().any(|target| matches!(
        target,
        carina_core::parser::DeferredForTarget::Unbound { .. }
    )));
}

async fn run_saved_plan_schema_fingerprint_case(
    planning_schemas: &SchemaRegistry,
    resource_type: &str,
    identity: &str,
    secret_bearing: bool,
) -> (Result<Option<Duration>, AppError>, String) {
    let user_dirs = carina_core::TestUserDirs::new();
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mock_state_path = tmp.path().join("mock-provider-state.json");
    let op_log_path = tmp.path().join("provider-operations.log");
    std::fs::write(&mock_state_path, "{}").expect("empty mock provider state");
    let _mock_state = crate::commands::shared::cancellation_test_support::ScopedEnv::set(
        "CARINA_MOCK_STATE_FILE",
        &mock_state_path,
    );
    let _op_log = crate::commands::shared::cancellation_test_support::ScopedEnv::set(
        "CARINA_MOCK_OP_LOG",
        &op_log_path,
    );

    let mut resource = Resource::with_provider(
        "mock",
        resource_type,
        ResourceIdentity::try_from(identity.to_string()).expect("non-empty fixture identity"),
        None,
    )
    .with_attribute("name", string_value(identity));
    if secret_bearing {
        resource.set_attr(
            "name",
            Value::Deferred(DeferredValue::Secret(Box::new(string_value(identity)))),
        );
        resource = carina_core::value::redact_secrets_in_resource(&resource)
            .expect("redact saved-plan secret fixture");
    }
    let resource_id = resource.id.clone();
    let mut plan = Plan::new();
    plan.add(carina_core::effect::Effect::Create(resolved(
        resource.clone(),
    )));
    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-27T00:00:00Z".to_string(),
        source_path: fixture.config_path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: saved_provider_configs(vec![ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        }]),
        backend_config: None,
        plan,
        sorted_resources: vec![resource.clone()],
        unresolved_resources: vec![resource],
        compositions: Vec::new(),
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: vec![crate::commands::plan::CurrentStateEntry::managed(
            resource_id.clone(),
            State::not_found(resource_id),
        )],
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };
    let plan_file = record_with_test_schemas(contents, planning_schemas);

    let observer_factory = fixture.observer_factory();
    let result = run_apply_from_plan_locked(
        &user_dirs,
        inspected_saved_plan(plan_file),
        &ProviderContext::default(),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("reviewed-plan.json"),
            source_dir: Path::new("."),
        },
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await;
    let operations = std::fs::read_to_string(op_log_path).unwrap_or_default();
    (result, operations)
}

#[tokio::test]
async fn saved_plan_apply_rejects_removed_provider_type_before_provider_call() {
    let planning_schemas = mock_schema_registry([ResourceSchema::new("removed.Widget")
        .attribute(AttributeSchema::required("name", AttributeType::string()).build())]);

    let (result, operations) =
        run_saved_plan_schema_fingerprint_case(&planning_schemas, "removed.Widget", "gone", false)
            .await;
    let error = result.expect_err("a removed planned type must invalidate the saved plan");
    let AppError::SavedPlanSchemaMismatch(mismatch) = &error else {
        panic!("expected typed schema mismatch, got {error:?}");
    };
    let changed_types = &mismatch.providers["mock"];
    assert!(
        changed_types
            .iter()
            .any(|change| { change.contains("removed.Widget") && change.contains("missing") })
    );
    assert_eq!(
        mismatch.replan_command,
        "carina plan --out reviewed-plan.json"
    );
    assert!(
        error.to_string().contains("Re-run `carina plan --out"),
        "error must include the saved-plan re-plan command: {error}"
    );
    assert_eq!(
        operations, "",
        "provider was called before schema rejection"
    );
}

#[tokio::test]
async fn secret_bearing_saved_plan_checks_schema_before_secret_rehydration() {
    let planning_schemas = mock_schema_registry([ResourceSchema::new("removed.Widget")
        .attribute(AttributeSchema::required("name", AttributeType::string()).build())]);

    let (result, operations) =
        run_saved_plan_schema_fingerprint_case(&planning_schemas, "removed.Widget", "secret", true)
            .await;
    let error = result.expect_err("schema mismatch must win over unavailable secret inputs");
    assert!(
        matches!(error, AppError::SavedPlanSchemaMismatch(_)),
        "expected SavedPlanSchemaMismatch before a secret error, got {error:?}"
    );
    assert_eq!(
        operations, "",
        "provider was called before schema rejection"
    );
}

#[tokio::test]
async fn saved_plan_apply_rejects_changed_attribute_flag_before_provider_call() {
    let builtin_schema = carina_provider_mock::builtin_schemas()
        .into_iter()
        .find(|schema| schema.parts().resource_type == "test.resource")
        .expect("built-in test.resource schema");
    let mut wire =
        carina_core::protocol_convert::resource_schema_to_protocol(&builtin_schema).unwrap();
    wire.attributes
        .get_mut("comment")
        .expect("built-in comment attribute")
        .write_only = true;
    let planned_schema = carina_core::protocol_convert::protocol_to_resource_schema(&wire).unwrap();
    let planning_schemas = mock_schema_registry([planned_schema]);

    let (result, operations) = run_saved_plan_schema_fingerprint_case(
        &planning_schemas,
        "test.resource",
        "changed",
        false,
    )
    .await;
    let error = result.expect_err("a changed write_only flag must invalidate the saved plan");
    assert!(matches!(
        &error,
        AppError::SavedPlanSchemaMismatch(mismatch)
            if mismatch.providers["mock"].iter().any(|change| {
                change.contains("test.resource") && change.contains("changed")
            })
    ));
    assert_eq!(
        operations, "",
        "provider was called before schema rejection"
    );
}

#[tokio::test]
async fn saved_plan_apply_accepts_unchanged_provider_schema() {
    let planning_schemas = mock_schema_registry(carina_provider_mock::builtin_schemas());

    let (result, operations) = run_saved_plan_schema_fingerprint_case(
        &planning_schemas,
        "test.resource",
        "unchanged",
        false,
    )
    .await;
    result.expect("an unchanged provider schema must remain executable");
    assert!(
        operations
            .lines()
            .any(|operation| operation == "create test.resource.unchanged"),
        "the unchanged resource did not reach the provider: {operations}"
    );
}

#[test]
fn saved_plan_schema_check_preserves_current_state_only_references_after_reload() {
    let fixture = ApplyCancellationFixture::new();
    let schemas = mock_schema_registry(carina_provider_mock::builtin_schemas());
    let id = ResourceId::with_provider_identity("mock", "example.thing", "gone", None);
    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-28T00:00:00Z".to_string(),
        source_path: fixture.config_path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: saved_provider_configs(vec![ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        }]),
        backend_config: None,
        plan: Plan::new(),
        sorted_resources: Vec::new(),
        unresolved_resources: Vec::new(),
        compositions: Vec::new(),
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: vec![crate::commands::plan::CurrentStateEntry::managed(
            id.clone(),
            State::not_found(id),
        )],
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };
    let plan_file = record_with_test_schemas(contents, &schemas);

    let bound = ProviderBoundPlanFile::for_test(
        inspected_saved_plan(plan_file),
        ProviderLoad::for_test::<carina_core::resource::NoPlaceholder>(Vec::new(), &[])
            .into_complete()
            .expect("test provider load is complete"),
        Path::new("."),
    )
    .expect("placeholder-free plan must bind its provider load");
    let reloaded = bound
        .reload_in_flight_from_locked_state(None, Path::new("."))
        .expect("current-state snapshot should reload");
    let ReloadedProviderBoundPlanFile::Verified { plan, providers } = reloaded else {
        panic!("placeholder-free plan changed typestate during reload")
    };
    let checked = plan.verify_provider_load(
        providers,
        ApplyTarget::SavedPlan {
            plan_file: Path::new("reviewed-plan.json"),
            source_dir: Path::new("."),
        },
    );

    assert!(
        checked.is_ok(),
        "the immutable pre-reload reference inventory must verify unchanged schemas"
    );
}

fn checked_exports(
    params: &[carina_core::parser::ExpandedInferredExportParam],
) -> carina_core::validation::ExportsCheckedPreApply<carina_core::parser::ExpandedInferredExportParam>
{
    carina_core::validation::check_export_values_pre_apply(
        params,
        &carina_core::binding_index::ResolvedBindings::default(),
        &carina_core::parser::LoadedProviderContext::empty_for_test(),
    )
    .expect("test exports must pass the secret boundary")
}

fn fixture_unchecked_export_static_type() -> carina_core::static_type::ExportStaticType {
    carina_core::static_type::ExportStaticType::Inferred(
        carina_core::static_type::StaticType::Unchecked(
            carina_core::static_type::InferredUncheckedReason::NotInferable(
                carina_core::static_type::NotInferableReason::SchemaUnavailable {
                    binding: "test fixture".to_string(),
                },
            ),
        ),
    )
}

fn resolve_and_materialize_exports_for_test(
    input: ResolveExportValuesPostApplyInput<'_>,
) -> Result<crate::commands::shared::state_writeback::ExportResolution, AppError> {
    let values = resolve_export_values_post_apply(input)?;
    materialize_exports(&values)
}

fn saved_provider_configs(
    configs: Vec<ProviderConfig>,
) -> carina_core::parser::SavedProviderConfigs {
    let mut parsed = carina_core::parser::ExpandedInferredFile::default();
    parsed.providers = configs;
    let resolved = carina_core::parser::resolve_provider_attributes_with_remote(
        &parsed,
        &HashMap::new(),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("test provider configs must resolve");
    carina_core::parser::SavedProviderConfigs::from(&resolved)
}

fn deserialize_saved_provider_configs(
    configs: Vec<ProviderConfig>,
) -> carina_core::parser::SavedProviderConfigs {
    serde_json::from_value(serde_json::to_value(configs).unwrap()).unwrap()
}

fn saved_composition_argument<P: PlaceholderKind + PartialEq>(
    value: Value<P>,
) -> LiveCompositionArgument<P> {
    LiveCompositionArgument::from_value(
        value,
        carina_core::validation::resolve_consumer_static_type_for_signature(
            &carina_core::parser::TypeExpr::String,
            &carina_core::parser::ProviderContext::default(),
        ),
        carina_core::parser::ArgumentOrigin::Defaulted,
    )
}

fn saved_module_composition<P: PlaceholderKind + PartialEq>(
    argument: Value<P>,
    rejected: &str,
) -> LiveComposition<P> {
    let argument_name = "value";
    LiveComposition {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "_virtual",
            "root.checked",
        )),
        signature: LiveCompositionSignature {
            arguments: indexmap::IndexMap::from([(
                argument_name.to_string(),
                saved_composition_argument(argument),
            )]),
            attributes: indexmap::IndexMap::new(),
            pending_constraints: vec![PendingModuleConstraint {
                id: ModuleConstraintId::argument_validation(argument_name, 0),
                expression: carina_core::parser::ValidateExpr::Compare {
                    lhs: Box::new(carina_core::parser::ValidateExpr::Var(
                        argument_name.to_string(),
                    )),
                    op: carina_core::parser::CompareOp::Ne,
                    rhs: Box::new(carina_core::parser::ValidateExpr::String(
                        rejected.to_string(),
                    )),
                },
                message: format!("value must not be {rejected}"),
            }],
        },
        binding: Some("checked".to_string()),
        dependency_bindings: BTreeSet::new(),
        module_name: "checked_module".to_string(),
        instance: "root.checked".to_string(),
        call_path: crate::test_composition_call_path(),
        quoted_string_attrs: HashSet::new(),
    }
}

#[test]
fn saved_plan_import_rehydration_respects_current_provider_instance() {
    let planned_id = ResourceId::with_provider_identity(
        "mock",
        "test.resource",
        "example",
        Some("planned".to_string()),
    );
    let mut plan = Plan::new();
    plan.add(carina_core::effect::Effect::Import {
        id: carina_core::resource::ResolvedResourceId::new(planned_id.clone()),
        identifier: string_value("provider-id"),
    });

    let mut parsed = carina_core::parser::ExpandedInferredFile::default();
    parsed.resources = vec![Resource::with_provider(
        "mock",
        "test.resource",
        "example",
        Some("current".to_string()),
    )];
    parsed.state_blocks = vec![carina_core::parser::StateBlock::Import {
        to: carina_core::parser::StateBlockAddress::new("mock", "test.resource", "example"),
        to_span: None,
        id: string_value("provider-id"),
    }];
    let identifiers = import_identifiers_from_current_config(&plan, &parsed);

    assert!(
        !identifiers.contains_key(&planned_id),
        "a current import routed to a different provider instance must not rehydrate the saved effect"
    );
}

#[tokio::test]
async fn saved_plan_does_not_match_anonymous_compositions_by_missing_binding() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let saved_source = tmp.path().join("saved\u{1b}[31m.crn");
    let current_source = tmp.path().join("current.crn");
    std::fs::write(&saved_source, "saved fixture").unwrap();
    std::fs::write(&current_source, "current fixture").unwrap();
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(string_value("public"))));
    let mut saved = saved_module_composition(secret.clone(), "rejected");
    saved.id = carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
        "_virtual",
        "saved.anonymous",
    ));
    saved.binding = None;
    saved.call_path = crate::test_composition_call_path_in(&saved_source);
    let mut source_identities = CompositionSourceIdentityResolver::new(tmp.path())
        .expect("create composition source identity resolver");
    let saved = saved
        .into_saved_with_resolver(&mut source_identities)
        .expect("save anonymous composition");
    let saved = carina_core::value::redact_secrets_in_saved_composition(&saved)
        .expect("redact saved composition");

    let mut current = saved_module_composition(secret, "rejected");
    current.id = carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
        "_virtual",
        "current.anonymous",
    ));
    current.binding = None;
    current.call_path = crate::test_composition_call_path_in(&current_source);
    let mut parsed = carina_core::parser::ExpandedInferredFile::default();
    parsed.compositions = vec![current];
    let provider_inputs = load_and_resolve_provider_configs(
        &parsed,
        tmp.path(),
        &ProviderContext::default(),
        UpstreamMissingStatePolicy::Strict,
        ProviderUpstreamScope::All,
    )
    .await
    .expect("resolve empty provider config");
    let candidates = SavedPlanSecretCandidates::from_test_parts(parsed, provider_inputs);
    let plan_file = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-16T00:00:00Z".to_string(),
        source_path: tmp.path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: saved_provider_configs(Vec::new()),
        backend_config: None,
        plan: Plan::new(),
        sorted_resources: Vec::new(),
        unresolved_resources: Vec::new(),
        compositions: vec![saved],
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: Vec::new(),
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    }
    .record_for_test(
        &ValidatedConfig::from_factories_for_test((), Vec::new(), &[]),
        Vec::new(),
    )
    .expect("test plan provider evidence must be valid");

    let mut plan_file = plan_file
        .reload_in_flight_from_locked_state(None)
        .expect("test plan should cross the locked-state reload boundary")
        .resolve_with_schemas(&carina_core::schema::SchemaRegistry::new())
        .expect("test plan should cross schema resolution");
    let result = rehydrate_desired_plan_file_from_parts(
        &mut plan_file,
        candidates.parsed(),
        candidates.provider_inputs(),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("plan.json"),
            source_dir: tmp.path(),
        },
    );

    let error = result.expect_err(
        "two anonymous compositions must not match merely because both bindings are None",
    );
    let rendered = error.to_string();
    assert!(
        !rendered.contains('\u{1b}'),
        "a saved source path injected a terminal escape byte",
    );
    assert!(
        rendered.contains(r"saved\u{1b}[31m.crn"),
        "the escaped source path was not retained in the moved-call diagnostic",
    );

    std::fs::remove_file(&current_source).expect("remove current composition source");
    let error = rehydrate_desired_plan_file_from_parts(
        &mut plan_file,
        candidates.parsed(),
        candidates.provider_inputs(),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("plan.json"),
            source_dir: tmp.path(),
        },
    )
    .expect_err("an unreadable live call source must fail with its typed identity error");
    let AppError::CompositionSourceIdentity(identity_error) = error else {
        panic!("apply flattened the composition identity error");
    };
    assert!(
        std::error::Error::source(&identity_error)
            .is_some_and(|source| source.downcast_ref::<std::io::Error>().is_some()),
        "apply dropped the composition identity io source",
    );
}

#[tokio::test]
async fn saved_plan_rejects_invalid_source_root_relative_composition_paths_as_corrupt() {
    let user_dirs = carina_core::TestUserDirs::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let source_file = tmp.path().join("source.crn");
    std::fs::write(&source_file, "source fixture").expect("write composition source");

    let mut composition = saved_module_composition(string_value("public"), "rejected");
    composition.call_path = crate::test_composition_call_path_in(&source_file);
    let mut source_identities = CompositionSourceIdentityResolver::new(tmp.path())
        .expect("create composition source identity resolver");
    let saved = composition
        .into_saved_with_resolver(&mut source_identities)
        .expect("save composition");
    let plan_file = record_with_test_context(PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-29T00:00:00Z".to_string(),
        source_path: tmp.path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: saved_provider_configs(Vec::new()),
        backend_config: None,
        plan: Plan::new(),
        sorted_resources: Vec::new(),
        unresolved_resources: Vec::new(),
        compositions: vec![saved],
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: Vec::new(),
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    });
    let encoded = serde_json::to_value(plan_file).expect("serialize plan fixture");
    let overlong = "a".repeat(4097);
    let too_many_components = (0..257).map(|_| "a").collect::<Vec<_>>().join("/");

    for (case, invalid_path, expected_detail) in [
        ("absolute", "/tmp/module.crn", "relative"),
        ("nul", "module\0.crn", "NUL"),
        ("overlong", overlong.as_str(), "4096 bytes"),
        ("current-directory", "module/./source.crn", "`.`"),
        (
            "too-many-components",
            too_many_components.as_str(),
            "256 components",
        ),
    ] {
        let mut malformed = encoded.clone();
        malformed["compositions"][0]["origin_id"]["root"]["source_file"]["path"] =
            serde_json::Value::String(invalid_path.to_string());
        let plan_path = tmp.path().join(format!("{case}.plan.json"));
        std::fs::write(
            &plan_path,
            serde_json::to_vec(&malformed).expect("serialize malformed plan"),
        )
        .expect("write malformed plan");

        let error = run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            NonZeroUsize::new(1).unwrap(),
            false,
            &ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await
        .expect_err("an invalid saved composition path must reject the plan");
        let AppError::CorruptPlanFile { error, .. } = error else {
            panic!("{case}: expected typed corrupt-plan error, got {error:?}");
        };
        assert_eq!(
            error.classify(),
            serde_json::error::Category::Data,
            "{case}: path validation must be a typed data error: {error}",
        );
        assert!(
            error.to_string().contains(expected_detail),
            "{case}: validation detail was missing: {error}",
        );
    }
}

#[tokio::test]
async fn saved_plan_persists_pending_state_migration_before_drift_return() {
    let user_dirs = carina_core::TestUserDirs::new();
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_path = tmp.path().join("carina.state.json");
    let mock_state_path = tmp.path().join("mock-provider-state.json");
    let lineage = "11111111-1111-1111-1111-111111111111";
    std::fs::write(
        &state_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 6,
            "serial": 7,
            "lineage": lineage,
            "carina_version": "0.0.0-test",
            "resources": []
        }))
        .expect("serialize v6 state"),
    )
    .expect("write v6 state");
    std::fs::write(
        &mock_state_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "test.resource.example": { "name": "drifted" }
        }))
        .expect("serialize provider state"),
    )
    .expect("write provider state");

    let mut resource = Resource::with_provider("mock", "test.resource", "example", None);
    resource.set_attr("name", string_value("planned"));
    let id = resource.id.clone();
    let planned_state = State::existing(
        id.clone(),
        HashMap::from([("name".to_string(), string_value("planned"))]),
    )
    .with_identifier("mock-id");
    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-16T00:00:00Z".to_string(),
        source_path: tmp.path().display().to_string(),
        state_lineage: Some(lineage.to_string()),
        state_serial: Some(7),
        provider_configs: saved_provider_configs(vec![ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        }]),
        backend_config: None,
        plan: Plan::new(),
        sorted_resources: vec![resource.clone()],
        unresolved_resources: vec![resource],
        compositions: Vec::new(),
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: vec![crate::commands::plan::CurrentStateEntry::managed(
            id,
            planned_state,
        )],
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };
    let plan_file = record_with_builtin_mock_schemas(contents);
    let backend =
        Arc::<dyn StateBackend>::from(crate::commands::local_backend_for_test(state_path.clone()));
    unsafe {
        std::env::set_var("CARINA_MOCK_STATE_FILE", &mock_state_path);
    }

    let observer_factory = fixture.observer_factory();
    let result = run_apply_from_plan_locked(
        &user_dirs,
        inspected_saved_plan(plan_file),
        &ProviderContext::default(),
        true,
        backend,
        StateLock::disabled_for_test(),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("plan.json"),
            source_dir: tmp.path(),
        },
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).expect("non-zero parallelism"),
        false,
    )
    .await;

    unsafe {
        std::env::remove_var("CARINA_MOCK_STATE_FILE");
    }
    let error = result.expect_err("drift must still abort the saved-plan apply");
    assert!(error.to_string().contains("drift"), "error: {error}");
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).expect("read state after drift return"))
            .expect("parse persisted state");
    assert_eq!(
        persisted["version"],
        serde_json::json!(StateFile::CURRENT_VERSION),
        "the pending schema migration must be saved before drift can return early"
    );
}

fn state_file_from_json(json: serde_json::Value) -> StateFile {
    carina_state::check_and_migrate(&json.to_string())
        .expect("test state should pass the version gate")
        .into_state()
}

struct FailBCreateFactory;

impl ProviderFactory for FailBCreateFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Mock provider with B create failure"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        Box::pin(async {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(FailBCreateProvider)))
        })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                PatternResourceSchema::new("test.resource")
                    .attribute(
                        PatternAttributeSchema::optional("id", PatternAttributeType::string())
                            .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional("name", PatternAttributeType::string())
                            .build(),
                    ),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_plan_apply_reports_provider_load_error_before_artifact_mismatch() {
    let user_dirs = carina_core::TestUserDirs::new();
    let project = tempfile::tempdir().expect("tempdir");
    let source_path = project.path().join("broken-provider.wasm");
    let installed_path = project
        .path()
        .join(".carina/providers/file/broken-provider/broken-provider.wasm");
    std::fs::create_dir_all(installed_path.parent().unwrap()).unwrap();
    std::fs::write(&source_path, b"not a wasm component").unwrap();
    std::fs::write(&installed_path, b"not a wasm component").unwrap();
    let provider_config = ProviderConfig {
        name: "mock".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: Some(format!("file://{}", source_path.display()).parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    };
    let planning_context = ValidatedConfig::from_factories_for_test(
        (),
        vec![Box::new(FailBCreateFactory)],
        std::slice::from_ref(&provider_config),
    );
    let plan_file = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-29T00:00:00Z".to_string(),
        source_path: project.path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: saved_provider_configs(vec![provider_config]),
        backend_config: None,
        plan: Plan::new(),
        sorted_resources: Vec::new(),
        unresolved_resources: Vec::new(),
        compositions: Vec::new(),
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: Vec::new(),
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    }
    .record_for_test(&planning_context, Vec::new())
    .expect("planning context must record provider evidence");
    let plan_path = project.path().join("plan.json");
    std::fs::write(
        &plan_path,
        carina_core::utils::pretty_with_newline(&plan_file).unwrap(),
    )
    .expect("write saved plan");
    let state_path = project
        .path()
        .join(carina_state::LocalBackend::DEFAULT_STATE_FILE);
    let lock_path = state_path.with_extension("lock");
    carina_state::BackendLock::local_default()
        .save(project.path())
        .unwrap();
    let backend = crate::commands::local_backend_for_test(state_path);
    let held_lock = backend.acquire_lock("test").await.unwrap();
    assert!(lock_path.exists(), "fixture must hold the state lock");

    let error = run_apply_from_plan(
        &user_dirs,
        &plan_path,
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        false,
        &ProviderContext::default(),
        ShutdownToken::running(),
    )
    .await
    .expect_err("the failed provider load must abort saved-plan apply");
    let rendered = error.to_string();

    assert!(
        rendered.contains("failed to read provider component"),
        "provider load diagnostic is missing: {rendered}"
    );
    assert!(
        !rendered.contains("Provider artifact mismatch"),
        "artifact evidence must not replace the provider load error: {rendered}"
    );
    assert!(
        lock_path.exists(),
        "provider load diagnostics must fail before state lock acquisition"
    );
    backend.release_lock(&held_lock).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_plan_helper_releases_passed_lock_when_provider_load_hard_fails() {
    let user_dirs = carina_core::TestUserDirs::new();
    let project = tempfile::tempdir().expect("tempdir");
    let source = format!(
        "file://{}",
        project.path().join("missing-provider.wasm").display()
    )
    .parse()
    .unwrap();
    let provider_config = ProviderConfig {
        name: "mock".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: Some(source),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    };
    let provider_configs = vec![provider_config.clone(), provider_config];
    let planning_context = ValidatedConfig::from_factories_for_test(
        (),
        vec![Box::new(FailBCreateFactory)],
        &provider_configs,
    );
    let plan_file = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-29T00:00:00Z".to_string(),
        source_path: project.path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: deserialize_saved_provider_configs(provider_configs),
        backend_config: None,
        plan: Plan::new(),
        sorted_resources: Vec::new(),
        unresolved_resources: Vec::new(),
        compositions: Vec::new(),
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: Vec::new(),
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    }
    .record_for_test(&planning_context, Vec::new())
    .expect("planning context must record provider evidence");
    let state_path = project.path().join("state.json");
    let lock_path = state_path.with_extension("lock");
    let backend: Arc<dyn StateBackend> =
        Arc::from(crate::commands::local_backend_for_test(state_path));
    let lock = backend.acquire_lock("apply").await.unwrap();
    assert!(lock_path.exists(), "fixture must hold the lock");

    let error = run_apply_from_plan_locked(
        &user_dirs,
        inspected_saved_plan(plan_file),
        &ProviderContext::default(),
        true,
        Arc::clone(&backend),
        StateLock::held(lock),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("plan.json"),
            source_dir: project.path(),
        },
        ShutdownToken::running(),
        &cli_observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("duplicate sourced providers must hard-fail loading");

    assert!(
        error.to_string().contains("declared more than once"),
        "unexpected loader error: {error}"
    );
    assert!(!lock_path.exists(), "load failure must release the lock");
}

#[tokio::test]
async fn secret_bearing_saved_plan_apply_loads_providers_once() {
    let user_dirs = carina_core::TestUserDirs::new();
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new();
    let config = format!(
        r#"backend local {{ path = "{}" }}
provider mock {{}}
let secret_resource = mock.test.resource {{
  name = "single-load-secret"
  comment = secret("single-load-plaintext")
}}
"#,
        fixture.state_path().display()
    );
    let fixture = fixture.with_raw_config(config);
    let provider_state_path = fixture.config_path().join("mock-provider-state.json");
    let _provider_state = crate::commands::shared::cancellation_test_support::ScopedEnv::set(
        "CARINA_MOCK_STATE_FILE",
        &provider_state_path,
    );
    let plan_path = fixture.config_path().join("plan.json");
    crate::commands::plan::run_plan(
        &user_dirs,
        fixture.config_path(),
        Some(&plan_path),
        None,
        None,
        DetailLevel::None,
        false,
        false,
        false,
        false,
        false,
        fixture.provider_context(),
    )
    .await
    .expect("secret-bearing plan must be recorded");

    let load_count = crate::wiring::track_provider_loads_for_test();
    run_apply_from_plan(
        &user_dirs,
        &plan_path,
        true,
        false,
        NonZeroUsize::new(1).unwrap(),
        false,
        fixture.provider_context(),
        fixture.cancel_token(),
    )
    .await
    .expect("secret-bearing saved plan must apply");

    assert_eq!(
        load_count.count(),
        1,
        "current-config secret validation and locked verification must share one provider load"
    );
}

#[tokio::test]
async fn state_bucket_bootstrap_prepares_resolved_upstream_provider_config() {
    let (_temp, downstream, parsed) =
        crate::provider_upstream_test_support::provider_upstream_fixture();
    let expanded = parsed
        .into_expanded()
        .expect("provider upstream fixture has no parser placeholders");
    let inputs = load_apply_provider_inputs(&expanded, &downstream, &ProviderContext::default())
        .await
        .expect("bootstrap provider inputs must resolve");
    let received_role_arn = Arc::new(Mutex::new(None));
    let ctx = ValidatedConfig::from_factories_for_test(
        (),
        vec![Box::new(
            crate::provider_upstream_test_support::ProviderConfigCaptureFactory {
                received_role_arn: Arc::clone(&received_role_arn),
            },
        )],
        inputs.configs().as_provider_configs(),
    );
    create_state_bucket_bootstrap_provider(&ctx, inputs.configs(), "test")
        .await
        .expect("bootstrap must construct the provider from resolved inputs");

    assert_eq!(
        received_role_arn.lock().unwrap().as_ref(),
        Some(&Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::123456789012:role/upstream".into()
        )))
    );
}

#[tokio::test]
async fn apply_provider_loading_remains_all_strict_for_tag_only_upstreams() {
    let (_temp, base_dir, parsed) =
        crate::provider_upstream_test_support::provider_default_tags_missing_upstream_fixture();
    let parsed = parsed
        .into_expanded()
        .expect("provider default-tags fixture has no parser placeholders");

    let error =
        match load_apply_provider_inputs(&parsed, &base_dir, &ProviderContext::default()).await {
            Ok(_) => panic!("apply must load every declared upstream strictly"),
            Err(error) => error,
        };

    let message = error.to_string();
    assert!(message.contains("upstream_state 'mgmt'"), "{message}");
    assert!(message.contains("no state found"), "{message}");
}

#[tokio::test]
async fn apply_verifies_backend_before_loading_bootstrap_provider_upstreams() {
    let user_dirs = carina_core::TestUserDirs::new();
    let temp = tempfile::tempdir().unwrap();
    let upstream = temp.path().join("upstream");
    let downstream = temp.path().join("downstream");
    std::fs::create_dir_all(&upstream).unwrap();
    std::fs::create_dir_all(&downstream).unwrap();
    std::fs::write(
        upstream.join("main.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .unwrap();
    std::fs::write(
        downstream.join("main.crn"),
        r#"
            provider mock { target = mgmt.target }
            let mgmt = upstream_state { source = "../upstream" }
        "#,
    )
    .unwrap();

    let error = run_apply_with_observer_factory(
        &user_dirs,
        &downstream,
        true,
        false,
        NonZeroUsize::new(1).unwrap(),
        false,
        &ProviderContext::default(),
        ShutdownToken::running(),
        &cli_observer_factory,
    )
    .await
    .expect_err("apply without a backend lock must require init first");

    let message = error.to_string();
    assert!(message.contains("Backend lock file not found"), "{message}");
    assert!(!message.contains("upstream_state 'mgmt'"), "{message}");
}

#[tokio::test]
async fn bootstrap_reloaded_snapshot_feeds_fresh_provider_resolution() {
    let user_dirs = carina_core::TestUserDirs::new();
    let temp = tempfile::tempdir().unwrap();
    let upstream = temp.path().join("upstream");
    let downstream = temp.path().join("downstream");
    std::fs::create_dir_all(&upstream).unwrap();
    std::fs::create_dir_all(&downstream).unwrap();
    std::fs::write(
        upstream.join("main.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .unwrap();
    std::fs::write(
        downstream.join("backend.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .unwrap();
    std::fs::write(
        downstream.join("providers.crn"),
        "provider test { role_arn = mgmt.role_arn }\n",
    )
    .unwrap();
    std::fs::write(
        downstream.join("main.crn"),
        "let mgmt = upstream_state { source = \"../upstream\" }\n",
    )
    .unwrap();

    let write_upstream = |role_arn: &str| {
        let mut state = StateFile::new();
        state.insert_export_json_for_test("role_arn", serde_json::json!(role_arn));
        std::fs::write(
            upstream.join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    };
    write_upstream("arn:aws:iam::111111111111:role/old");
    let loaded = load_configuration_with_config(
        &downstream,
        &ProviderContext::default(),
        &SchemaRegistry::new(),
    )
    .unwrap();
    let parsed = loaded.parsed;
    let providers = ProviderLoad::load(&user_dirs, &parsed.providers, &downstream).unwrap();
    let unresolved_parsed = loaded.unresolved_parsed;
    let wiring = ValidatedWiring::from_load_for_test(providers);
    let validated_config = validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed);
    let expanded = parsed
        .clone()
        .into_expanded()
        .expect("provider upstream fixture has no parser placeholders");
    let stale_inputs =
        load_apply_provider_inputs(&expanded, &downstream, &ProviderContext::default())
            .await
            .unwrap();
    assert_eq!(
        stale_inputs.configs().as_provider_configs()[0]
            .attributes
            .get("role_arn"),
        Some(&Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::111111111111:role/old".into()
        )))
    );

    write_upstream("arn:aws:iam::222222222222:role/new");
    let reloaded = reload_apply_configuration_after_bootstrap(
        &downstream,
        &downstream,
        &ProviderContext::default(),
        validated_config,
        unresolved_parsed,
    )
    .expect("bootstrap reload must succeed");
    let received_role_arn = Arc::new(Mutex::new(None));
    let ctx = ValidatedWiring::from_factories_for_test(
        vec![Box::new(
            crate::provider_upstream_test_support::ProviderConfigCaptureFactory {
                received_role_arn: Arc::clone(&received_role_arn),
            },
        )],
        &reloaded.resources().parsed().providers,
    );
    let backend = Arc::<dyn StateBackend>::from(crate::commands::local_backend_for_test(
        downstream.join("apply.state.json"),
    ));

    let reloaded =
        ValidatedConfig::from_wiring_for_test(ctx, reloaded.into_resources_for_fixture());
    run_apply_locked_canonical(
        reloaded,
        true,
        backend,
        StateLock::disabled_for_test(),
        &downstream,
        &ProviderContext::default(),
        ShutdownToken::running(),
        &cli_observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect("the production locked path must resolve providers from the reloaded snapshot");

    assert_eq!(
        received_role_arn.lock().unwrap().as_ref(),
        Some(&Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::222222222222:role/new".into()
        ))),
        "the lock-held provider must ignore stale pre-reload bootstrap inputs"
    );
}

#[test]
fn bootstrap_reload_rejects_nested_forwarding_mismatch() {
    let user_dirs = carina_core::TestUserDirs::new();
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("nested module directories");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"provider mock {}
let outer = use { source = './outer' }
outer { a = 'wrong' }
"#,
    )
    .expect("root configuration");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner = use { source = './inner' }
arguments { a: String }
inner { x = a }
"#,
    )
    .expect("outer module");
    std::fs::write(inner.join("main.crn"), "arguments { x: Int }\n").expect("inner module");
    let loaded = load_configuration_with_config(
        fixture.path(),
        &ProviderContext::default(),
        &SchemaRegistry::new(),
    )
    .expect("load unresolved bootstrap snapshot");
    let providers =
        ProviderLoad::load(&user_dirs, &loaded.parsed.providers, fixture.path()).unwrap();
    let wiring = ValidatedWiring::from_load_for_test(providers);
    // The reload API consumes the previous validation owner only for its
    // provider wiring. This fixture is intentionally invalid before reload,
    // so it cannot mint a resolved-file witness for the authored graph.
    let validated_config = validated_reload_owner_for_test(&wiring);
    let unresolved = loaded.unresolved_parsed;

    let error = reload_apply_configuration_after_bootstrap(
        fixture.path(),
        fixture.path(),
        &ProviderContext::default(),
        validated_config,
        unresolved,
    )
    .expect_err("apply bootstrap reload must retain the reference-type gate");
    let message = crate::error::format_error_with_source_chain(&error);
    assert!(
        message.contains("Reference type validation failed")
            && message.contains("module call 'inner'")
            && message.contains("expected Int, got String (from a)"),
        "{message}"
    );
}

fn write_module_output_mismatch_fixture(nested: bool) -> tempfile::TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    if nested {
        let mid = fixture.path().join("mid");
        let leaf = mid.join("leaf");
        std::fs::create_dir_all(&leaf).expect("nested module directories");
        std::fs::write(
            fixture.path().join("main.crn"),
            "provider mock {}\nlet mid = use { source = './mid' }\nmid { a = 'hello' }\n",
        )
        .expect("root configuration");
        std::fs::write(mid.join("arguments.crn"), "arguments { a: String }\n")
            .expect("mid arguments");
        std::fs::write(
            mid.join("calls.crn"),
            r#"let leaf = use { source = './leaf' }
let x = leaf { n = 1 }
let y = leaf { n = x.s }
"#,
        )
        .expect("nested calls");
        std::fs::write(leaf.join("arguments.crn"), "arguments { n: Int }\n")
            .expect("leaf arguments");
        std::fs::write(
            leaf.join("attributes.crn"),
            "attributes { s: String = 'str' }\n",
        )
        .expect("leaf attributes");
    } else {
        let module = fixture.path().join("m");
        std::fs::create_dir_all(&module).expect("module directory");
        std::fs::write(
            fixture.path().join("main.crn"),
            r#"provider mock {}
let m = use { source = './m' }
let x = m { n = 1 }
let y = m { n = x.s }
"#,
        )
        .expect("root configuration");
        std::fs::write(module.join("arguments.crn"), "arguments { n: Int }\n")
            .expect("module arguments");
        std::fs::write(
            module.join("attributes.crn"),
            "attributes { s: String = 'str' }\n",
        )
        .expect("module attributes");
    }
    fixture
}

#[test]
fn bootstrap_reload_rejects_module_output_forwarding_mismatch() {
    let user_dirs = carina_core::TestUserDirs::new();
    for nested in [false, true] {
        let fixture = write_module_output_mismatch_fixture(nested);
        let loaded = load_configuration_with_config(
            fixture.path(),
            &ProviderContext::default(),
            &SchemaRegistry::new(),
        )
        .expect("load unresolved bootstrap snapshot");
        let providers =
            ProviderLoad::load(&user_dirs, &loaded.parsed.providers, fixture.path()).unwrap();
        let wiring = ValidatedWiring::from_load_for_test(providers);
        // The reload API consumes the previous validation owner only for its
        // provider wiring. This fixture is intentionally invalid before
        // reload, so use an empty resolved payload owned by the same wiring.
        let validated_config = validated_reload_owner_for_test(&wiring);
        let unresolved = loaded.unresolved_parsed;
        let error = reload_apply_configuration_after_bootstrap(
            fixture.path(),
            fixture.path(),
            &ProviderContext::default(),
            validated_config,
            unresolved,
        )
        .expect_err("apply bootstrap reload must reject module-output type mismatch");
        let message = crate::error::format_error_with_source_chain(&error);
        assert!(message.contains("module call 'y'"), "{message}");
        assert!(message.contains("expected Int"), "{message}");
        assert!(message.contains("got String"), "{message}");
        assert!(message.contains("from x.s"), "{message}");
        assert!(
            !message.contains("module call 'mid_"),
            "diagnostic must retain authored call names: {message}"
        );
    }
}

struct FailBCreateProvider;

impl Provider for FailBCreateProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let id = id.clone();
        Box::pin(async move { Ok(ReadOutcome::not_found(id)) })
    }

    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(State::not_found(id)) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        let id = id.clone();
        Box::pin(async move {
            if id.identity_str().expect("resolved identity") == "b" {
                return Err(
                    attempt.untracked(ProviderError::api_error("create failed").for_resource(id))
                );
            }
            let resource = request.resource().as_resource().clone();
            Ok(attempt
                .issued(
                    AppliedIdentifier::new("mock-id").expect("fixture identifier is non-empty"),
                    carina_core::provider::Sent::AllRequestAttributes,
                )
                .succeeded(resource.resolved_attributes()))
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        let id = id.clone();
        Box::pin(async move { Err(ProviderError::internal("unexpected update").for_resource(id)) })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move { Err(ProviderError::internal("unexpected delete").for_resource(id)) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

#[derive(Clone, Default)]
struct ApplyTimeReadShared {
    created_roles: Arc<Mutex<HashMap<String, HashMap<String, Value>>>>,
    operations: Arc<Mutex<Vec<String>>>,
    consumer_description: Arc<Mutex<Option<Value>>>,
    read_calls: Arc<AtomicUsize>,
    allow_internet_gateway_updates: Arc<AtomicBool>,
}

struct ApplyTimeReadFactory {
    shared: ApplyTimeReadShared,
}

impl ProviderFactory for ApplyTimeReadFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Mock provider for apply-time data-source reads"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        let shared = self.shared.clone();
        Box::pin(async move {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(ApplyTimeReadProvider { shared })))
        })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                PatternResourceSchema::new("iam.Role")
                    .attribute(
                        PatternAttributeSchema::optional(
                            "role_name",
                            PatternAttributeType::string(),
                        )
                        .create_only()
                        .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional("path", PatternAttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional(
                            "max_session_duration",
                            PatternAttributeType::int(),
                        )
                        .create_only()
                        .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional(
                            "description",
                            PatternAttributeType::string(),
                        )
                        .build(),
                    ),
                PatternResourceSchema::new("iam.Roles")
                    .attribute(
                        PatternAttributeSchema::optional(
                            "name_regex",
                            PatternAttributeType::string(),
                        )
                        .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional(
                            "names",
                            PatternAttributeType::list(PatternAttributeType::string()),
                        )
                        .build(),
                    )
                    .as_data_source(),
                PatternResourceSchema::new("ec2.InternetGateway")
                    .attribute(
                        PatternAttributeSchema::optional("name", PatternAttributeType::string())
                            .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional(
                            "tags",
                            PatternAttributeType::map(PatternAttributeType::string()),
                        )
                        .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional(
                            "settings",
                            PatternAttributeType::struct_(
                                "InternetGatewaySettings",
                                vec![
                                    PatternStructField::optional(
                                        "enabled",
                                        PatternAttributeType::bool(),
                                    )
                                    .build(),
                                    PatternStructField::optional(
                                        "description",
                                        PatternAttributeType::string(),
                                    )
                                    .build(),
                                ],
                            ),
                        )
                        .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional(
                            "rules",
                            PatternAttributeType::list(PatternAttributeType::struct_(
                                "InternetGatewayRule",
                                vec![
                                    PatternStructField::optional(
                                        "port",
                                        PatternAttributeType::int(),
                                    )
                                    .build(),
                                    PatternStructField::optional(
                                        "description",
                                        PatternAttributeType::string(),
                                    )
                                    .build(),
                                ],
                            )),
                        )
                        .build(),
                    ),
                PatternResourceSchema::new("ec2.NatGateway").attribute(
                    PatternAttributeSchema::optional(
                        "tags",
                        PatternAttributeType::map(PatternAttributeType::string()),
                    )
                    .build(),
                ),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

struct ApplyTimeReadProvider {
    shared: ApplyTimeReadShared,
}

#[derive(Clone, Default)]
struct ApplyCascadeShared {
    creates: Arc<Mutex<ApplyCascadeCreateCalls>>,
    deletes: Arc<Mutex<ApplyCascadeDeleteCalls>>,
    fail_delete_identifiers: Arc<Mutex<HashSet<String>>>,
}

type ApplyCascadeCreateCalls = Vec<(String, HashMap<String, Value>)>;
type ApplyCascadeDeleteCalls = Vec<(String, String)>;

impl ApplyCascadeShared {
    fn create_calls(&self) -> ApplyCascadeCreateCalls {
        self.creates.lock().unwrap().clone()
    }

    fn delete_calls(&self) -> ApplyCascadeDeleteCalls {
        self.deletes.lock().unwrap().clone()
    }

    fn fail_delete_identifier(&self, identifier: &str) {
        self.fail_delete_identifiers
            .lock()
            .unwrap()
            .insert(identifier.to_string());
    }
}

struct ApplyCascadeAwsccFactory {
    shared: ApplyCascadeShared,
}

impl ProviderFactory for ApplyCascadeAwsccFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWSCC apply cascade test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        let shared = self.shared.clone();
        Box::pin(async move {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(ApplyCascadeProvider { shared })))
        })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                PatternResourceSchema::new("ec2.Vpc")
                    .attribute(
                        PatternAttributeSchema::optional(
                            "cidr_block",
                            PatternAttributeType::string(),
                        )
                        .create_only()
                        .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::provider_populated(
                            "vpc_id",
                            PatternAttributeType::string(),
                        )
                        .build(),
                    )
                    .with_coexisting_replacement(),
                PatternResourceSchema::new("ec2.Subnet")
                    .attribute(
                        PatternAttributeSchema::optional("vpc_id", PatternAttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional(
                            "cidr_block",
                            PatternAttributeType::string(),
                        )
                        .create_only()
                        .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::optional(
                            "availability_zone",
                            PatternAttributeType::string(),
                        )
                        .create_only()
                        .build(),
                    )
                    .attribute(
                        PatternAttributeSchema::provider_populated(
                            "subnet_id",
                            PatternAttributeType::string(),
                        )
                        .build(),
                    )
                    .with_coexisting_replacement(),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

struct ApplyCascadeProvider {
    shared: ApplyCascadeShared,
}

impl Provider for ApplyCascadeProvider {
    fn name(&self) -> &str {
        "awscc"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let id = id.clone();
        let identifier = identifier.as_str().to_string();
        Box::pin(async move {
            let state = match (id.resource_type.as_str(), identifier.as_str()) {
                ("ec2.Vpc", "vpc-new") => State::existing(
                    id,
                    HashMap::from([
                        ("cidr_block".to_string(), string_value("10.221.0.0/16")),
                        ("vpc_id".to_string(), string_value("vpc-new")),
                    ]),
                )
                .with_identifier("vpc-new"),
                ("ec2.Vpc", "vpc-old" | "vpc-older" | "vpc-oldest") => State::existing(
                    id,
                    HashMap::from([
                        ("cidr_block".to_string(), string_value("10.220.0.0/16")),
                        ("vpc_id".to_string(), string_value(identifier.as_str())),
                    ]),
                )
                .with_identifier(identifier.as_str()),
                ("ec2.Subnet", "subnet-old") => State::existing(
                    id,
                    HashMap::from([
                        ("vpc_id".to_string(), string_value("vpc-old")),
                        ("cidr_block".to_string(), string_value("10.220.1.0/24")),
                        (
                            "availability_zone".to_string(),
                            string_value("ap-northeast-1c"),
                        ),
                    ]),
                )
                .with_identifier("subnet-old"),
                _ => State::not_found(id),
            };
            Ok(fixture_read_outcome(state))
        })
    }

    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(State::not_found(id)) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        let id = id.clone();
        let attrs = request.resource().as_resource().resolved_attributes();
        self.shared
            .creates
            .lock()
            .unwrap()
            .push((id.to_string(), attrs.clone()));

        Box::pin(async move {
            match id.resource_type.as_str() {
                "ec2.Vpc" => Ok(attempt
                    .issued(
                        AppliedIdentifier::new("vpc-new").expect("fixture identifier is non-empty"),
                        carina_core::provider::Sent::AllRequestAttributes,
                    )
                    .succeeded(HashMap::from([(
                        "vpc_id".to_string(),
                        string_value("vpc-new"),
                    )]))),
                "ec2.Subnet" => Ok(attempt
                    .issued(
                        AppliedIdentifier::new("subnet-new")
                            .expect("fixture identifier is non-empty"),
                        carina_core::provider::Sent::AllRequestAttributes,
                    )
                    .succeeded(HashMap::from([(
                        "subnet_id".to_string(),
                        string_value("subnet-new"),
                    )]))),
                _ => {
                    Err(attempt
                        .untracked(ProviderError::internal(format!("unexpected create {id}"))))
                }
            }
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        let id = id.clone();
        Box::pin(async move { Err(ProviderError::internal("unexpected update").for_resource(id)) })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &carina_core::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let shared = self.shared.clone();
        Box::pin(async move {
            shared
                .deletes
                .lock()
                .unwrap()
                .push((id.to_string(), identifier.clone()));
            if shared
                .fail_delete_identifiers
                .lock()
                .unwrap()
                .contains(&identifier)
            {
                Err(
                    ProviderError::api_error(format!("delete failed for {identifier}"))
                        .for_resource(id),
                )
            } else {
                Ok(())
            }
        })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

fn string_value(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.to_string().into()))
}

fn apply_cascade_crn(state_path: &std::path::Path, include_subnet: bool) -> String {
    let subnet = if include_subnet {
        r#"
awscc.ec2.Subnet {
  vpc_id = vpc.vpc_id
  cidr_block = "10.221.1.0/24"
  availability_zone = "ap-northeast-1c"
  directives {
    create_before_destroy = true
  }
}
"#
    } else {
        ""
    };
    format!(
        r#"backend local {{ path = "{}" }}
provider awscc {{}}

let vpc = awscc.ec2.Vpc {{
  cidr_block = "10.221.0.0/16"
  directives {{
    create_before_destroy = true
  }}
}}
{subnet}"#,
        state_path.display()
    )
}

fn concrete_subnet_identity(
    ctx: &WiringContext,
    providers: &[ProviderConfig],
    vpc_id: &str,
    cidr_block: &str,
) -> ResourceIdentity {
    let mut resources = vec![
        Resource::pending_with_provider("awscc", "ec2.Subnet", None)
            .with_attribute("vpc_id", string_value(vpc_id))
            .with_attribute("cidr_block", string_value(cidr_block))
            .with_attribute("availability_zone", string_value("ap-northeast-1c")),
    ];
    let canonical =
        carina_core::value::canonicalize_resources_with_schemas(&mut resources, ctx.schemas())
            .unwrap();
    let errors = crate::wiring::compute_anonymous_identifiers_with_ctx(ctx, canonical, providers);
    assert!(errors.is_empty(), "state id setup failed: {errors:?}");
    resources[0]
        .id
        .identity()
        .expect("resolved identity")
        .clone()
}

fn seed_apply_cascade_state(
    fixture: &ApplyCancellationFixture,
    ctx: &WiringContext,
    providers: &[ProviderConfig],
    include_subnet: bool,
) {
    let mut vpc_state = ResourceState::try_applied("ec2.Vpc", "vpc", "awscc", "vpc-old")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.220.0.0/16"))
        .with_attribute("vpc_id", serde_json::json!("vpc-old"));
    vpc_state.binding = Some("vpc".to_string());

    let mut state_file = carina_state::StateFile::new();
    state_file
        .upsert_resource(vpc_state)
        .expect("test state setup must be valid");

    if include_subnet {
        let mut subnet_state = ResourceState::try_applied(
            "ec2.Subnet",
            concrete_subnet_identity(ctx, providers, "vpc-old", "10.220.1.0/24"),
            "awscc",
            "subnet-old",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("vpc_id", serde_json::json!("vpc-old"))
        .with_attribute("cidr_block", serde_json::json!("10.220.1.0/24"))
        .with_attribute("availability_zone", serde_json::json!("ap-northeast-1c"));
        subnet_state.dependency_bindings.insert("vpc".to_string());
        state_file
            .upsert_resource(subnet_state)
            .expect("test state setup must be valid");
    }

    fixture.write_state(&state_file);
}

fn seed_apply_cascade_state_with_deposed(
    fixture: &ApplyCancellationFixture,
    deposed_identifiers: &[&str],
) {
    let mut vpc_state = ResourceState::try_applied("ec2.Vpc", "vpc", "awscc", "vpc-new")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.221.0.0/16"))
        .with_attribute("vpc_id", serde_json::json!("vpc-new"));
    vpc_state.binding = Some("vpc".to_string());
    for identifier in deposed_identifiers {
        vpc_state.deposed.push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            (*identifier).to_string(),
            None,
            HashMap::from([
                ("cidr_block".to_string(), serde_json::json!("10.220.0.0/16")),
                ("vpc_id".to_string(), serde_json::json!(identifier)),
            ]),
            BTreeSet::new(),
        ));
    }

    let mut state_file = carina_state::StateFile::new();
    state_file
        .upsert_resource(vpc_state)
        .expect("test state setup must be valid");
    fixture.write_state(&state_file);
}

fn deposed_identifiers(state_path: &std::path::Path) -> Vec<String> {
    let raw = std::fs::read_to_string(state_path).expect("state file must be written");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("state must be valid JSON");
    let vpc = state_json_resource(&state, "awscc", "ec2.Vpc", "vpc");
    vpc.get("deposed")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|entry| {
            entry["identifier"]
                .as_str()
                .expect("deposed identifier must be a string")
                .to_string()
        })
        .collect()
}

async fn run_apply_cascade_fixture(
    fixture: &ApplyCancellationFixture,
    shared: &ApplyCascadeShared,
) -> Result<Option<Duration>, AppError> {
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = validate_with_test_factories(
        &parsed,
        base_dir,
        false,
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let observer_factory = fixture.observer_factory();
    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
}

fn state_json_resource<'a>(
    state: &'a serde_json::Value,
    provider: &str,
    resource_type: &str,
    identity: &str,
) -> &'a serde_json::Value {
    state["resources"]
        .as_array()
        .expect("state resources must be an array")
        .iter()
        .find(|row| {
            row["provider"] == provider
                && row["resource_type"] == resource_type
                && row["identity"] == identity
        })
        .unwrap_or_else(|| panic!("missing state row {provider}.{resource_type}.{identity}"))
}

fn assert_vpc_new_current_and_old_deposed(state_path: &std::path::Path) {
    let raw = std::fs::read_to_string(state_path).expect("state file must be written");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("state must be valid JSON");
    let vpc = state_json_resource(&state, "awscc", "ec2.Vpc", "vpc");
    assert_eq!(vpc["current"]["identifier"], serde_json::json!("vpc-new"));

    let deposed = vpc
        .get("deposed")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("expected vpc-old to be recorded as deposed; row: {vpc:#?}"));
    assert_eq!(deposed.len(), 1, "expected exactly one deposed VPC");
    assert_eq!(deposed[0]["identifier"], serde_json::json!("vpc-old"));
}

impl Provider for ApplyTimeReadProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let id = id.clone();
        let shared = self.shared.clone();
        Box::pin(async move {
            let roles = shared.created_roles.lock().unwrap();
            if let Some(attrs) = roles.get(id.identity_str().expect("resolved identity")) {
                let mut attrs = attrs.clone();
                if id.resource_type == "ec2.InternetGateway" {
                    attrs
                        .entry("tags".to_string())
                        .or_insert_with(|| Value::Concrete(ConcreteValue::Map(IndexMap::new())));
                }
                Ok(fixture_read_outcome(
                    State::existing(id, attrs).with_identifier("mock-id"),
                ))
            } else {
                Ok(ReadOutcome::not_found(id))
            }
        })
    }

    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        let shared = self.shared.clone();
        Box::pin(async move {
            shared.read_calls.fetch_add(1, Ordering::SeqCst);
            shared.operations.lock().unwrap().push(format!(
                "read:{}",
                resource.id.identity_str().expect("resolved identity")
            ));

            let expected = resource
                .attributes
                .get("name_regex")
                .and_then(value_string)
                .and_then(exact_regex_literal)
                .unwrap_or_default();

            let names = shared
                .created_roles
                .lock()
                .unwrap()
                .values()
                .filter_map(|attrs| attrs.get("role_name").and_then(value_string))
                .filter(|role_name| role_name == &expected)
                .map(|role_name| Value::Concrete(ConcreteValue::String(role_name.into())))
                .collect();

            Ok(State::existing(
                resource.id.clone(),
                HashMap::from([(
                    "names".to_string(),
                    Value::Concrete(ConcreteValue::List(names)),
                )]),
            )
            .with_identifier("mock-id"))
        })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        let id = id.clone();
        let shared = self.shared.clone();
        Box::pin(async move {
            let mut attrs = request.resource().as_resource().resolved_attributes();
            if id.resource_type == "iam.Role" {
                attrs
                    .entry("path".to_string())
                    .or_insert_with(|| Value::Concrete(ConcreteValue::String("/".into())));
                attrs
                    .entry("max_session_duration".to_string())
                    .or_insert_with(|| Value::Concrete(ConcreteValue::Int(3600)));
            }
            if id.identity_str().expect("resolved identity") == "consumer" {
                *shared.consumer_description.lock().unwrap() = attrs.get("description").cloned();
            }
            shared.created_roles.lock().unwrap().insert(
                id.identity_str().expect("resolved identity").to_string(),
                attrs.clone(),
            );
            shared.operations.lock().unwrap().push(format!(
                "create:{}",
                id.identity_str().expect("resolved identity")
            ));

            Ok(attempt
                .issued(
                    AppliedIdentifier::new("mock-id").expect("fixture identifier is non-empty"),
                    carina_core::provider::Sent::AllRequestAttributes,
                )
                .succeeded(attrs))
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        id: &ResourceId,
        identifier: &carina_core::provider::AppliedIdentifier,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.clone();
        let shared = self.shared.clone();
        Box::pin(async move {
            let supports_update = id.resource_type == "ec2.NatGateway"
                || (id.resource_type == "ec2.InternetGateway"
                    && shared.allow_internet_gateway_updates.load(Ordering::SeqCst));
            if !supports_update {
                return Err(ProviderError::internal("unexpected update").for_resource(id));
            }

            let mut attrs = request.from().attributes.clone();
            for op in &request.patch().ops {
                match op {
                    PatchOp::Add { key, value }
                    | PatchOp::Replace { key, value }
                    | PatchOp::Reassert { key, value } => {
                        attrs.insert(key.clone(), value.clone());
                    }
                    PatchOp::Remove { key } => {
                        attrs.remove(key);
                    }
                }
            }
            shared.created_roles.lock().unwrap().insert(
                id.identity_str().expect("resolved identity").to_string(),
                attrs.clone(),
            );
            Ok(carina_core::provider::UpdateOutcome::Success {
                state: AppliedState::from_identifier(id, identifier, attrs),
            })
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move { Err(ProviderError::internal("unexpected delete").for_resource(id)) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

fn value_string(value: &Value) -> Option<String> {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => Some(value.expose_plaintext().to_owned()),
        _ => None,
    }
}

fn exact_regex_literal(value: String) -> Option<String> {
    value
        .strip_prefix('^')
        .and_then(|value| value.strip_suffix('$'))
        .map(ToString::to_string)
}

fn apply_time_read_crn(state_path: &std::path::Path) -> String {
    format!(
        r#"backend local {{ path = "{}" }}
provider mock {{}}

let target_role = mock.iam.Role {{
  role_name = "carina-acc-test-3666-target"
}}

let roles = read mock.iam.Roles {{
  name_regex = "^${{target_role.role_name}}$"
}}

let consumer = mock.iam.Role {{
  role_name = "carina-acc-test-3666-consumer"
  description = roles.names[0]
}}
"#,
        state_path.display()
    )
}

fn module_data_source_fixture() -> (ApplyCancellationFixture, ApplyTimeReadShared) {
    let fixture = ApplyCancellationFixture::new();
    let module_dir = fixture.config_path().join("registry");
    std::fs::create_dir_all(&module_dir).expect("create module fixture directory");
    std::fs::write(
        module_dir.join("read.crn"),
        r#"
arguments {
  consumer_name: String
}

let caller = read mock.iam.Roles {
  name_regex = "^existing$"
}
"#,
    )
    .expect("write module data-source fixture");
    std::fs::write(
        module_dir.join("resource.crn"),
        r#"
let consumer = mock.iam.Role {
  role_name  = consumer_name
  description = "role:${caller.names[0]}"
}
"#,
    )
    .expect("write module resource fixture");

    let root = format!(
        r#"backend local {{ path = "{}" }}
provider mock {{}}

let registry = use {{
  source = "./registry"
}}

let registry_publish = registry {{
  consumer_name = "module-consumer"
}}
"#,
        fixture.state_path().display()
    );
    let fixture = fixture.with_raw_config(root);

    let shared = ApplyTimeReadShared::default();
    shared.created_roles.lock().unwrap().insert(
        "existing".to_string(),
        HashMap::from([("role_name".to_string(), string_value("existing"))]),
    );

    (fixture, shared)
}

fn load_and_resolve_module_data_source_fixture(
    fixture: &ApplyCancellationFixture,
    shared: &ApplyTimeReadShared,
) -> (
    carina_core::parser::ModuleResolvedInferredFile,
    carina_core::config_loader::UnresolvedConfigSnapshot,
) {
    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("module data-source fixture must load");
    let unresolved_parsed = loaded.unresolved_parsed;
    let providers = crate::wiring::ProviderLoad::for_test_with_load_errors(
        crate::wiring::LoadedProviderFactory::for_test(vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })]),
        &loaded.parsed.providers,
        HashMap::new(),
    );
    let parsed = providers
        .validation_report(
            &loaded.parsed,
            get_base_dir(fixture.config_path()),
            false,
            &loaded.duplicate_declarations,
        )
        .into_result()
        .unwrap_or_else(|errors| {
            panic!(
                "module data-source fixture must validate, got: {:?}",
                errors.as_slice()
            )
        });

    (parsed, unresolved_parsed)
}

#[tokio::test]
async fn plan_reads_module_data_source_and_resolves_consumer_interpolation() {
    use carina_core::effect::Effect;

    let (fixture, shared) = module_data_source_fixture();
    let (parsed, unresolved_parsed) =
        load_and_resolve_module_data_source_fixture(&fixture, &shared);

    assert_eq!(
        parsed.data_sources[0].binding.as_deref(),
        Some("registry_publish.caller"),
        "module resolution must prefix the data-source binding"
    );

    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let expanded = parsed
        .clone()
        .into_expanded()
        .expect("module data-source fixture has no parser placeholders");
    let provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
        &expanded,
        &HashMap::new(),
        &ProviderContext::default(),
    )
    .expect("module data-source provider configs must resolve");
    let validated = validated_resolved_apply_config_for_test(&wiring, &parsed, &unresolved_parsed);
    let plan_ctx = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &None,
        true,
        crate::commands::shared::progress::ProgressSink::Human,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("module data-source plan should build");

    assert_eq!(
        plan_ctx
            .resources()
            .data_sources
            .iter()
            .map(|data_source| data_source.id.identity_str().expect("resolved identity"))
            .collect::<Vec<_>>(),
        ["registry_publish.caller"],
        "the expanded module data source must be in the planning set"
    );
    let consumer = plan_ctx
        .resources()
        .plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            Effect::Create(resource)
                if resource.id.identity_str().expect("resolved identity")
                    == "registry_publish.consumer" =>
            {
                Some(resource)
            }
            _ => None,
        })
        .expect("module consumer must have a create effect");
    assert_eq!(
        consumer.attributes.get("description"),
        Some(&string_value("role:existing")),
        "the module-scoped interpolation must resolve before planning"
    );
    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        1,
        "planning must read the module data source exactly once"
    );
}

#[tokio::test]
async fn apply_reads_module_data_source_and_resolves_consumer_interpolation() {
    let (fixture, shared) = module_data_source_fixture();
    let (parsed, unresolved_parsed) =
        load_and_resolve_module_data_source_fixture(&fixture, &shared);
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let observer_factory = fixture.observer_factory();

    run_apply_locked(
        validated_resolved_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        fixture.config_path(),
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect("apply must read the module data source before creating its consumer");

    assert_eq!(
        shared.operations.lock().unwrap().as_slice(),
        [
            "read:registry_publish.caller",
            "create:registry_publish.consumer"
        ],
        "apply must include the prefixed read before its module consumer"
    );
    let created_roles = shared.created_roles.lock().unwrap();
    let consumer = created_roles
        .get("registry_publish.consumer")
        .expect("module consumer must be created");
    assert_eq!(
        consumer.get("description"),
        Some(&string_value("role:existing")),
        "apply must not send a deferred interpolation to the provider"
    );
}

#[tokio::test]
async fn live_apply_reports_phase2_module_constraint_as_structured_plan_error() {
    let fixture = ApplyCancellationFixture::new();
    let crn = format!(
        r#"backend local {{ path = "{}" }}
provider mock {{}}

let lookup = read mock.iam.Roles {{
  name_regex = "^existing$"
}}
"#,
        fixture.state_path().display()
    );
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyTimeReadShared::default();
    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("phase-2 module constraint fixture must load");
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let validation_errors = validate_with_test_factories(
        &parsed,
        get_base_dir(fixture.config_path()),
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(validation_errors.is_empty(), "{validation_errors:?}");
    let mut composition = saved_module_composition(
        Value::<carina_core::resource::ParserPlaceholder>::Concrete(ConcreteValue::String(
            "bad".into(),
        )),
        "bad",
    );
    composition.signature.attributes.insert(
        "value".to_string(),
        carina_core::resource::LiveCompositionAttribute::from_value(
            Value::<carina_core::resource::ParserPlaceholder>::Concrete(ConcreteValue::String(
                "^existing$".to_string().into(),
            )),
            None,
        ),
    );
    parsed.compositions.push(composition);
    parsed.data_sources[0].set_attr(
        "name_regex",
        Value::resource_ref("checked", "value", Vec::new()),
    );
    let expanded_provider_configs = parsed
        .clone()
        .into_expanded()
        .expect("phase-2 constraint fixture has no leftover parser placeholders")
        .providers;
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        &expanded_provider_configs,
    );
    let observer_factory = fixture.observer_factory();

    let error = run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        fixture.config_path(),
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("the module constraint must abort direct apply");

    assert!(
        matches!(&error, AppError::Validation(message) if message == "plan failed with 1 error(s)"),
        "phase-2 module constraints must use the structured plan-error path: {error:?}"
    );
    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        0,
        "the rejected data source must not reach the provider"
    );
}

#[test]
fn total_apply_line_formats_duration() {
    assert_eq!(
        format_total_apply_line(Duration::from_secs(5)),
        "Done in 5.0s."
    );
}

#[test]
fn apply_parallelism_default_is_eight() {
    assert_eq!(crate::DEFAULT_PARALLELISM.get(), 8);
}

fn legacy_override_state() -> StateFile {
    let mut state = StateFile::new();
    let mut resource = ResourceState::try_applied("test.resource", "legacy", "mock", "legacy-id")
        .expect("fixture identifier is non-empty");
    resource.name_overrides.insert(
        "name".to_string(),
        NameOverride {
            temp_value: "legacy-temp".to_string(),
            original_value: None,
        },
    );
    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");
    state
}

fn plan_with_name_override(id: &ResourceId, temp_value: &str, original_value: &str) -> Plan {
    let mut plan = Plan::new();
    plan.fixture_add_permanent_name_override(
        carina_core::resource::ResolvedResourceId::new(id.clone()),
        "name",
        temp_value,
        Some(original_value.to_string()),
    );
    plan
}

#[test]
fn export_only_fast_path_is_disabled_when_deferred_reads_exist() {
    let mut data_source = DataSource::with_provider("mock", "iam.Roles", "roles", None);
    data_source.binding = Some("roles".to_string());
    data_source.attributes.insert(
        "name_regex".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: carina_core::resource::AccessPath::new("target_role", "role_name"),
        }),
    );
    let mut plan = Plan::new();
    plan.add(carina_core::effect::Effect::Read {
        resource: carina_core::resource::ResolvedDataSource::new(data_source.clone()),
    });
    let mut target = Resource::with_provider("mock", "iam.Role", "target_role", None);
    target.binding = Some("target_role".to_string());
    let data_sources = [data_source];
    let mut classified = crate::wiring::classify_apply_time_data_source_read_inputs(
        &data_sources,
        &[target],
        &HashMap::new(),
    );
    let values = DataSourceValues::from_data_sources(&data_sources, |data_source| {
        DataSourceValue::deferred_to_apply(
            classified
                .remove(&data_source.id)
                .expect("missing managed input must defer the read"),
        )
    });
    let executable_values = values
        .into_executable()
        .expect("deferred fixture must be executable");

    assert!(
        !can_use_export_only_fast_path(&plan, &executable_values),
        "deferred reads are executable effects and must not be silently skipped"
    );
}

#[test]
fn apply_time_classification_uses_folded_data_source_preserved_dependency() {
    let mut data_source =
        DataSource::with_provider("mock", "iam.Roles", "registry_publish.roles", None);
    data_source.binding = Some("registry_publish.roles".to_string());
    data_source
        .attributes
        .insert("name_regex".to_string(), string_value("^target-role$"));
    data_source
        .dependency_bindings
        .insert("registry_publish.target_role".to_string());
    let id = data_source.id.clone();
    let mut target =
        Resource::with_provider("mock", "iam.Role", "registry_publish.target_role", None);
    target.binding = Some("registry_publish.target_role".to_string());

    let data_sources = [data_source];
    let mut classified = crate::wiring::classify_apply_time_data_source_read_inputs(
        &data_sources,
        &[target],
        &HashMap::new(),
    );
    let values = DataSourceValues::from_data_sources(&data_sources, |data_source| {
        DataSourceValue::deferred_to_apply(
            classified
                .remove(&data_source.id)
                .expect("preserved dependency must defer the read"),
        )
    });
    let executable_values = values
        .into_executable()
        .expect("deferred fixture must be executable");

    assert!(
        executable_values.deferred_inputs(&id).is_some(),
        "saved-plan apply must preserve a deferred read after parsing folded its input value"
    );
}

#[test]
fn saved_plan_data_source_outcomes_must_match_configured_set_exactly() {
    let configured = DataSource::with_provider("mock", "iam.Roles", "configured", None);
    let unexpected: DataSource = DataSource::with_provider("mock", "iam.Roles", "unexpected", None);

    let missing = data_source_values_from_saved_plan(std::slice::from_ref(&configured), &[])
        .expect_err("a missing saved outcome must be rejected");
    assert!(missing.to_string().contains("has no outcome"));

    let unexpected_outcome = crate::commands::plan::SavedDataSourceValue {
        id: unexpected.id.clone(),
        value: DataSourceValue::deferred_to_apply(Vec::new()),
    };
    let extra = data_source_values_from_saved_plan::<carina_core::resource::ModuleSource>(
        &[],
        &[unexpected_outcome],
    )
    .expect_err("an outcome outside the configured set must be rejected");
    assert!(extra.to_string().contains("unknown data source"));

    let configured_outcome = crate::commands::plan::SavedDataSourceValue {
        id: configured.id.clone(),
        value: DataSourceValue::deferred_to_apply(Vec::new()),
    };
    let duplicate = data_source_values_from_saved_plan(
        std::slice::from_ref(&configured),
        &[configured_outcome.clone(), configured_outcome],
    )
    .expect_err("duplicate saved outcomes must be rejected");
    assert!(duplicate.to_string().contains("duplicate outcomes"));
}

#[tokio::test]
async fn saved_plan_apply_reconstructs_and_dispatches_deferred_data_source_read() {
    let user_dirs = carina_core::TestUserDirs::new();
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mock_state_path = tmp.path().join("mock-provider-state.json");
    std::fs::write(
        &mock_state_path,
        serde_json::json!({
            "iam.Roles.lookup": {
                "names": ["target"]
            }
        })
        .to_string(),
    )
    .expect("mock provider state");

    unsafe {
        std::env::set_var("CARINA_MOCK_STATE_FILE", &mock_state_path);
    }

    let target_id = ResourceId::with_provider_identity("mock", "test.resource", "target", None);
    let mut target = Resource::with_provider("mock", "test.resource", "target", None);
    target.id = target_id.clone();
    target.binding = Some("target".to_string());
    target.set_attr(
        "name",
        Value::Concrete(ConcreteValue::String("target".into())),
    );

    let lookup_id = ResourceId::with_provider_identity("mock", "iam.Roles", "lookup", None);
    let mut lookup = DataSource::with_provider("mock", "iam.Roles", "lookup", None);
    lookup.id = lookup_id.clone();
    lookup.binding = Some("lookup".to_string());
    lookup.set_attr(
        "name_regex",
        Value::Deferred(DeferredValue::ResourceRef {
            path: carina_core::resource::AccessPath::new("target", "name"),
        }),
    );

    let consumer_id = ResourceId::with_provider_identity("mock", "test.resource", "consumer", None);
    let mut consumer = Resource::with_provider("mock", "test.resource", "consumer", None);
    consumer.id = consumer_id.clone();
    consumer.binding = Some("consumer".to_string());
    consumer.set_attr(
        "name",
        Value::Concrete(ConcreteValue::String("consumer".into())),
    );
    consumer.set_attr(
        "comment",
        Value::Deferred(DeferredValue::ResourceRef {
            path: carina_core::resource::AccessPath::with_fields_and_subscripts(
                "lookup",
                "names",
                Vec::new(),
                vec![carina_core::resource::Subscript::Int { index: 0 }],
            ),
        }),
    );

    let mut plan = Plan::new();
    plan.add(carina_core::effect::Effect::Create(resolved(
        target.clone(),
    )));
    plan.add(carina_core::effect::Effect::Read {
        resource: ResolvedDataSource::new(lookup.clone()),
    });
    plan.add(carina_core::effect::Effect::Create(resolved(
        consumer.clone(),
    )));

    let provider_configs = saved_provider_configs(vec![ProviderConfig {
        name: "mock".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }]);
    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-07-02T00:00:00Z".to_string(),
        source_path: fixture.config_path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs,
        backend_config: None,
        plan,
        sorted_resources: vec![target.clone(), consumer.clone()],
        unresolved_resources: vec![target, consumer],
        compositions: Vec::new(),
        data_sources: vec![lookup.clone()],
        data_source_origins: vec![lookup.clone()],
        data_source_values: vec![crate::commands::plan::SavedDataSourceValue {
            id: lookup.id.clone(),
            value: DataSourceValue::deferred_to_apply(Vec::new()),
        }],
        current_states: vec![
            crate::commands::plan::CurrentStateEntry::managed(
                target_id,
                State::not_found(ResourceId::with_provider_identity(
                    "mock",
                    "test.resource",
                    "target",
                    None,
                )),
            ),
            crate::commands::plan::CurrentStateEntry::managed(
                consumer_id,
                State::not_found(ResourceId::with_provider_identity(
                    "mock",
                    "test.resource",
                    "consumer",
                    None,
                )),
            ),
        ],
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };
    let plan_file = record_with_builtin_mock_schemas(contents);
    let recorded = &plan_file.schema_fingerprints().providers["mock"].schemas;
    assert!(recorded.iter().any(|schema| {
        schema.kind == carina_core::schema::SchemaKind::Resource
            && schema.resource_type == "test.resource"
    }));
    assert!(recorded.iter().any(|schema| {
        schema.kind == carina_core::schema::SchemaKind::DataSource
            && schema.resource_type == "iam.Roles"
    }));

    let observer_factory = fixture.observer_factory();
    let result = run_apply_from_plan_locked(
        &user_dirs,
        inspected_saved_plan(plan_file),
        &ProviderContext::default(),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("plan.json"),
            source_dir: tmp.path(),
        },
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(4).unwrap(),
        false,
    )
    .await;

    unsafe {
        std::env::remove_var("CARINA_MOCK_STATE_FILE");
    }

    result.expect("saved-plan apply should execute the deferred read before consumer create");
    let state = fixture.read_state().await;
    let consumer = state
        .find_resource("mock", "test.resource", "consumer")
        .expect("consumer should be persisted");
    assert_eq!(
        consumer
            .persisted_attributes()
            .expect("consumer has applied attributes")
            .get("comment"),
        carina_core::value::json_to_dsl_value(&serde_json::json!("target")),
        "consumer must receive the value published by the apply-time read"
    );
}

#[tokio::test]
async fn saved_plan_apply_rejects_module_constraint_learned_from_upstream_create() {
    let user_dirs = carina_core::TestUserDirs::new();
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mock_state_path = tmp.path().join("mock-provider-state.json");
    let op_log_path = tmp.path().join("op.log");
    std::fs::write(&mock_state_path, "{}").expect("empty mock provider state");

    unsafe {
        std::env::set_var("CARINA_MOCK_STATE_FILE", &mock_state_path);
        std::env::set_var("CARINA_MOCK_OP_LOG", &op_log_path);
    }

    let mut producer = Resource::with_provider("mock", "test.resource", "producer", None);
    producer.binding = Some("producer".to_string());
    producer.set_attr("name", string_value("bad"));
    let producer_id = producer.id.clone();

    let mut consumer = Resource::with_provider("mock", "test.resource", "consumer", None);
    consumer.binding = Some("consumer".to_string());
    consumer.set_attr("name", string_value("consumer"));
    consumer.set_attr(
        "comment",
        Value::resource_ref("producer", "identifier", vec![]),
    );
    consumer.dependency_bindings.insert("producer".to_string());
    let consumer_id = consumer.id.clone();

    let composition = saved_module_composition(
        Value::resource_ref("producer", "identifier", vec![]),
        "bad-id",
    );
    let mut source_identities = CompositionSourceIdentityResolver::new(tmp.path())
        .expect("create composition source identity resolver");
    let saved_composition = composition
        .into_saved_with_resolver(&mut source_identities)
        .expect("save composition");
    let mut plan = Plan::new();
    plan.add(carina_core::effect::Effect::Create(resolved(
        producer.clone(),
    )));
    plan.add(carina_core::effect::Effect::Create(resolved(
        consumer.clone(),
    )));
    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-09-29T00:00:00Z".to_string(),
        source_path: fixture.config_path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: saved_provider_configs(vec![ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        }]),
        backend_config: None,
        plan,
        sorted_resources: vec![producer.clone(), consumer.clone()],
        unresolved_resources: vec![producer, consumer],
        compositions: vec![saved_composition],
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: vec![
            crate::commands::plan::CurrentStateEntry::managed(
                producer_id.clone(),
                State::not_found(producer_id.clone()),
            ),
            crate::commands::plan::CurrentStateEntry::managed(
                consumer_id.clone(),
                State::not_found(consumer_id),
            ),
        ],
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };
    let plan_file = record_with_builtin_mock_schemas(contents);

    let observer_factory = fixture.observer_factory();
    let result = run_apply_from_plan_locked(
        &user_dirs,
        inspected_saved_plan(plan_file),
        &ProviderContext::default(),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("plan.json"),
            source_dir: tmp.path(),
        },
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await;

    unsafe {
        std::env::remove_var("CARINA_MOCK_STATE_FILE");
        std::env::remove_var("CARINA_MOCK_OP_LOG");
    }

    let error = result.expect_err("saved-plan module constraint must fail apply");
    assert!(error.to_string().contains("1 failed"), "error: {error}");
    let operations = std::fs::read_to_string(&op_log_path).expect("producer operation log");
    assert!(
        operations
            .lines()
            .any(|operation| operation == "create test.resource.producer"),
        "the producing resource must reach the provider: {operations}"
    );
    assert!(
        !operations.lines().any(|operation| {
            operation == "create test.resource.consumer"
                || operation == "normalize_desired test.resource.consumer"
        }),
        "the consuming resource must not reach the provider: {operations}"
    );
}

#[tokio::test]
async fn saved_plan_noop_rejects_unconsumed_module_constraint() {
    let user_dirs = carina_core::TestUserDirs::new();
    let fixture = ApplyCancellationFixture::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut source_identities = CompositionSourceIdentityResolver::new(tmp.path())
        .expect("create composition source identity resolver");
    let saved_composition = saved_module_composition(string_value("bad"), "bad")
        .into_saved_with_resolver(&mut source_identities)
        .expect("save composition");
    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-09-29T00:00:00Z".to_string(),
        source_path: fixture.config_path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: saved_provider_configs(Vec::new()),
        backend_config: None,
        plan: Plan::new(),
        sorted_resources: Vec::new(),
        unresolved_resources: Vec::new(),
        compositions: vec![saved_composition],
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: Vec::new(),
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };
    let plan_file = record_with_test_context(contents);

    let observer_factory = fixture.observer_factory();
    let error = run_apply_from_plan_locked(
        &user_dirs,
        inspected_saved_plan(plan_file),
        &ProviderContext::default(),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("plan.json"),
            source_dir: tmp.path(),
        },
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("saved-plan no-op must run the terminal module gate");

    assert!(
        matches!(&error, AppError::ModuleConstraint(_)),
        "terminal module failures must retain their typed AppError variant: {error:?}"
    );
    let message = error.to_string();
    for expected in [
        "module 'checked_module' (call 'checked')",
        "value must not be bad",
        "'bad'",
    ] {
        assert!(message.contains(expected), "message: {message}");
    }
}

#[tokio::test]
async fn saved_plan_provider_validation_preserves_typed_terminal_error() {
    let user_dirs = carina_core::TestUserDirs::new();
    let fixture = ApplyCancellationFixture::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-14T00:00:00Z".to_string(),
        source_path: fixture.config_path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: deserialize_saved_provider_configs(vec![ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::from([(
                "region".to_string(),
                Value::resource_ref("mgmt", "region", vec![]),
            )]),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        }]),
        backend_config: None,
        plan: Plan::new(),
        sorted_resources: Vec::new(),
        unresolved_resources: Vec::new(),
        compositions: Vec::new(),
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: Vec::new(),
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };
    let plan_file = record_with_test_context(contents);
    let plan_path = tmp.path().join("unresolved-provider-config.plan.json");
    std::fs::write(
        &plan_path,
        carina_core::utils::pretty_with_newline(&plan_file).unwrap(),
    )
    .expect("write saved plan");
    let plan_file: PlanFile = serde_json::from_slice(
        &std::fs::read(&plan_path).expect("read saved plan after serialization"),
    )
    .expect("saved plan must round-trip through its on-disk representation");
    let observer_factory = fixture.observer_factory();

    let error = run_apply_from_plan_locked(
        &user_dirs,
        inspected_saved_plan(plan_file),
        &ProviderContext::default(),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        ApplyTarget::SavedPlan {
            plan_file: &plan_path,
            source_dir: tmp.path(),
        },
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("saved plan with unresolved provider config must be rejected");

    assert!(
        matches!(&error, AppError::SavedProviderConfig(_)),
        "ProviderConfigInvariantError must remain typed in AppError: {error:?}"
    );
    let rendered = error.to_string();
    assert_eq!(rendered.matches("mgmt.region").count(), 1, "{rendered}");
    assert!(std::error::Error::source(&error).is_none());
}

#[tokio::test]
async fn deferred_default_tags_are_accepted_by_saved_plan_apply() {
    let user_dirs = carina_core::TestUserDirs::new();
    let fixture = ApplyCancellationFixture::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_provider_temp, downstream, _) =
        crate::provider_upstream_test_support::provider_upstream_fixture();
    let upstream_state_path = downstream
        .parent()
        .unwrap()
        .join("upstream")
        .join(carina_state::LocalBackend::DEFAULT_STATE_FILE);
    std::fs::remove_file(&upstream_state_path).expect("remove upstream state");
    std::fs::write(
        downstream.join("providers.crn"),
        "provider mock { default_tags = { Owner = mgmt.owner } }\n",
    )
    .expect("write provider config");
    let parsed = carina_core::config_loader::load_configuration_with_config(
        &downstream,
        &ProviderContext::default(),
        &SchemaRegistry::new(),
    )
    .expect("load plan fixture")
    .parsed;
    let parsed = parsed
        .into_expanded()
        .expect("saved-plan provider fixture has no parser placeholders");
    let plan_inputs = load_and_resolve_provider_configs(
        &parsed,
        &downstream,
        &ProviderContext::default(),
        UpstreamMissingStatePolicy::Lenient,
        ProviderUpstreamScope::All,
    )
    .await
    .expect("plan must preserve deferred default_tags when upstream state is absent");
    assert!(matches!(
        plan_inputs.configs().as_provider_configs()[0]
            .default_tags
            .get("Owner"),
        Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
    ));
    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-14T00:00:00Z".to_string(),
        source_path: fixture.config_path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: carina_core::parser::SavedProviderConfigs::from(plan_inputs.configs()),
        backend_config: None,
        plan: Plan::new(),
        sorted_resources: Vec::new(),
        unresolved_resources: Vec::new(),
        compositions: Vec::new(),
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: Vec::new(),
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };
    let plan_file = record_with_test_context(contents);
    let plan_path = tmp.path().join("deferred-default-tags.plan.json");
    std::fs::write(
        &plan_path,
        carina_core::utils::pretty_with_newline(&plan_file).unwrap(),
    )
    .expect("write saved plan");
    let plan_file: PlanFile = serde_json::from_slice(
        &std::fs::read(&plan_path).expect("read saved plan after serialization"),
    )
    .expect("saved plan must round-trip through its on-disk representation");
    let observer_factory = fixture.observer_factory();

    run_apply_from_plan_locked(
        &user_dirs,
        inspected_saved_plan(plan_file),
        &ProviderContext::default(),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        ApplyTarget::SavedPlan {
            plan_file: &plan_path,
            source_dir: tmp.path(),
        },
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect("apply --plan must accept deferred default_tags");
}

#[tokio::test]
async fn live_apply_noop_rejects_unconsumed_module_constraint() {
    let fixture = ApplyCancellationFixture::new();
    let mut parsed = carina_core::parser::InferredFile::default();
    parsed.compositions = vec![saved_module_composition(
        Value::<carina_core::resource::ParserPlaceholder>::resource_ref("missing", "value", vec![]),
        "bad",
    )];
    let unresolved_parsed = carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(
        carina_core::parser::ParsedFile::default(),
    );
    let wiring = ValidatedWiring::from_factories_for_test(Vec::new(), &[]);
    let observer_factory = fixture.observer_factory();

    let error = run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        get_base_dir(fixture.config_path()),
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("live-apply no-op must run the terminal module gate");

    assert!(
        matches!(&error, AppError::ModuleConstraint(_)),
        "terminal module failures must retain their typed AppError variant: {error:?}"
    );
    let message = error.to_string();
    for expected in [
        "module 'checked_module' (call 'checked')",
        "value must not be bad",
        "constraint inputs are still unresolved at end of apply",
        "missing.value",
    ] {
        assert!(message.contains(expected), "message: {message}");
    }
}

#[test]
fn apply_refuses_v7_state_without_accept_flag() {
    let state = legacy_override_state();

    let err = check_legacy_name_overrides(&state, false, ApplyTarget::Project(Path::new(".")))
        .expect_err("legacy state must require opt-in");

    assert!(matches!(err, AppError::Validation(_)));
    let msg = err.to_string();
    assert!(msg.contains("mock.test.resource.legacy"));
    assert!(msg.contains("carina plan"));
    assert!(msg.contains("carina apply --accept-legacy-name-overrides"));
}

#[test]
fn legacy_override_hint_names_live_project_apply_target() {
    let state = legacy_override_state();
    let project_dir = Path::new("infra/foo");

    let err = check_legacy_name_overrides(&state, false, ApplyTarget::Project(project_dir))
        .expect_err("legacy state must require opt-in");
    let msg = err.to_string();

    assert!(msg.contains("Run `carina plan infra/foo` first"), "{msg}");
    assert!(!msg.contains("--out"), "{msg}");
    assert!(
        msg.contains("re-run `carina apply infra/foo --accept-legacy-name-overrides`"),
        "{msg}",
    );
}

#[test]
fn legacy_override_hint_names_saved_plan_apply_target() {
    let state = legacy_override_state();

    let err = check_legacy_name_overrides(
        &state,
        false,
        ApplyTarget::SavedPlan {
            plan_file: Path::new("reviewed-plan.json"),
            source_dir: Path::new("/canonical/project"),
        },
    )
    .expect_err("legacy state must require opt-in");
    let msg = err.to_string();

    assert!(
        msg.contains("Run `carina plan --out reviewed-plan.json /canonical/project` first"),
        "{msg}",
    );
    assert!(
        msg.contains("re-run `carina apply reviewed-plan.json --accept-legacy-name-overrides`"),
        "{msg}",
    );
    assert!(!msg.contains("carina apply /canonical/project"), "{msg}");
}

#[test]
fn saved_plan_drift_hint_regenerates_the_applied_plan_file() {
    assert_eq!(
        drift_replan_hint(ApplyTarget::SavedPlan {
            plan_file: Path::new("reviewed-plan.json"),
            source_dir: Path::new("infra/foo"),
        }),
        "Please re-run `carina plan --out reviewed-plan.json infra/foo` to create a new plan that reflects the current state."
    );
}

#[test]
fn live_apply_drift_hint_does_not_add_saved_plan_output() {
    let hint = drift_replan_hint(ApplyTarget::Project(Path::new("infra/foo")));

    assert_eq!(
        hint,
        "Please re-run `carina plan infra/foo` to create a new plan that reflects the current state."
    );
    assert!(!hint.contains("--out"));
}

#[test]
fn apply_proceeds_with_accept_flag_against_legacy_state() {
    let state = legacy_override_state();

    check_legacy_name_overrides(&state, true, ApplyTarget::Project(Path::new(".")))
        .expect("accept flag should permit legacy state");
}

#[test]
fn apply_emits_legacy_override_warning_to_stderr() {
    let state = legacy_override_state();
    let resource = Resource::with_provider("mock", "test.resource", "legacy", None).with_attribute(
        "name",
        Value::Concrete(ConcreteValue::String("legacy".into())),
    );
    let current_states = HashMap::new();
    let remote_bindings = HashMap::new();
    let wait_aliases = Vec::new();
    let projected_state = crate::wiring::ProjectedStateRead::for_test_unmoved(Some(&state));
    let (override_aware, _) = OverrideAwareResources::build_pair_with_stability(
        &canonical_resources_for_test(vec![resource], &SchemaRegistry::new()),
        &projected_state,
        LivePreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &current_states,
            data_source_values: &carina_core::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote_bindings,
            wait_aliases: &wait_aliases,
        },
        &HashMap::new(),
        OverrideAwareBuildMode::Apply,
    )
    .expect("override-aware resources should build");

    let mut stderr = Vec::new();
    crate::legacy_name_overrides::write_legacy_override_warnings(
        &override_aware,
        &projected_state,
        &mut stderr,
    )
    .expect("warning write should succeed");
    let stderr = String::from_utf8(stderr).expect("warning should be utf8");

    assert!(stderr.contains("warning: applied legacy name override for mock.test.resource.legacy"));
    assert!(stderr.contains("override attribute 'name' set to 'legacy-temp'"));
    assert!(stderr.contains("original DSL"));
}

#[test]
fn apply_does_not_require_flag_after_v7_to_v8_migration() {
    let state_file = legacy_override_state();
    let mut resource = Resource::with_provider("mock", "test.resource", "legacy", None);
    resource.set_attr(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("legacy".into())),
    );
    let id = resource.id.clone();
    let sorted_resources = vec![resolved(resource)];
    let applied_state = State::existing(
        id.clone(),
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("legacy-temp".into())),
        )]),
    )
    .with_identifier("legacy-id");
    let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);
    let plan = plan_with_name_override(&id, "legacy-temp", "legacy");

    let migrated = build_state_after_apply(
        ApplyStateSave {
            state_file,
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        },
        None,
    )
    .expect("writeback should migrate legacy override");

    assert!(!migrated.has_legacy_name_overrides());
    check_legacy_name_overrides(&migrated, false, ApplyTarget::Project(Path::new(".")))
        .expect("second apply should not require the legacy accept flag");
}

#[tokio::test]
async fn run_apply_cancelled_after_partial_execution_persists_state_and_releases_lock() {
    let user_dirs = carina_core::TestUserDirs::new();
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["first", "second", "third"])
        .cancel_after_successes(1);
    let token = fixture.cancel_token();
    let load_count = crate::wiring::track_provider_loads_for_test();

    let observer_factory = fixture.observer_factory();
    let err = run_apply_with_observer_factory(
        &user_dirs,
        fixture.config_path(),
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        false,
        fixture.provider_context(),
        token,
        &observer_factory,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, AppError::Interrupted),
        "expected Interrupted, got {err:?}"
    );
    assert_eq!(load_count.count(), 1, "live apply must load providers once");

    let state = fixture.read_state().await;
    assert!(fixture.state_path().exists());
    assert_eq!(
        state.resources().len(),
        1,
        "state must contain exactly the completed resource"
    );
    assert_eq!(
        state.serial, 1,
        "state serial should advance once from the initial empty state"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "first")
            .is_some()
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "second")
            .is_none()
    );
    assert!(!fixture.lock_path().exists());
}

#[tokio::test]
async fn apply_reports_heartbeat_lock_loss_instead_of_user_interrupt() {
    let user_dirs = carina_core::TestUserDirs::new();
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["slow"])
        .with_delayed_create("slow", Duration::from_millis(75));
    let _mock_state = crate::commands::shared::cancellation_test_support::ScopedEnv::set(
        "CARINA_MOCK_STATE_FILE",
        fixture.config_path().join("source-apply-mock-state.json"),
    );
    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let providers = ProviderLoad::load(&user_dirs, &parsed.providers, base_dir)
        .expect("mock provider must load");
    let validated_config = providers
        .validate_config_pair(
            parsed,
            unresolved_parsed,
            base_dir,
            false,
            &loaded.duplicate_declarations,
        )
        .expect("fixture must validate");
    let observer_factory = fixture.observer_factory();
    let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(15));
    let backend = fixture.lock_loss_backend_handle();

    let error = run_apply_locked_canonical(
        validated_config,
        true,
        backend.clone(),
        StateLock::held(lock),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("heartbeat lock loss must fail apply");

    assert!(matches!(error, AppError::StateLockLost { .. }));
    let rendered = error.to_string();
    assert!(rendered.contains("state may be behind the cloud"));
    assert_eq!(
        rendered.matches("test heartbeat lock was replaced").count(),
        1,
        "{rendered}"
    );
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(backend.locked_writes(), 0, "lost lock must not save state");
    assert_eq!(backend.releases(), 0, "lost lock must not be released");
}

#[tokio::test]
async fn saved_plan_apply_does_not_save_or_release_after_heartbeat_lock_loss() {
    let user_dirs = carina_core::TestUserDirs::new();
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["slow"])
        .with_delayed_create("slow", Duration::from_millis(75));
    let _mock_state = crate::commands::shared::cancellation_test_support::ScopedEnv::set(
        "CARINA_MOCK_STATE_FILE",
        fixture.config_path().join("saved-plan-mock-state.json"),
    );
    let mut resource = Resource::with_provider("mock", "test.resource", "slow", None);
    resource.binding = Some("slow".to_string());
    resource.set_attr("name", string_value("slow"));
    let resource_id = resource.id.clone();
    let mut plan = Plan::new();
    plan.add(carina_core::effect::Effect::Create(resolved(
        resource.clone(),
    )));
    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: "test".to_string(),
        timestamp: "2026-10-21T00:00:00Z".to_string(),
        source_path: fixture.config_path().display().to_string(),
        state_lineage: None,
        state_serial: None,
        provider_configs: saved_provider_configs(vec![ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        }]),
        backend_config: None,
        plan,
        sorted_resources: vec![resource.clone()],
        unresolved_resources: vec![resource],
        compositions: Vec::new(),
        data_sources: Vec::new(),
        data_source_origins: Vec::new(),
        data_source_values: Vec::new(),
        current_states: vec![crate::commands::plan::CurrentStateEntry::managed(
            resource_id.clone(),
            State::not_found(resource_id),
        )],
        upstream_snapshot: HashMap::new(),
        upstream_sources: Vec::new(),
        wait_bindings: Vec::new(),
    };
    let plan_file = record_with_builtin_mock_schemas(contents);
    let observer_factory = fixture.observer_factory();
    let backend = fixture.lock_loss_backend_handle();
    let lock = LockInfo::for_test_with_timeout("apply", Duration::from_millis(15));

    let error = run_apply_from_plan_locked(
        &user_dirs,
        inspected_saved_plan(plan_file),
        fixture.provider_context(),
        true,
        backend.clone(),
        StateLock::held(lock),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("plan.json"),
            source_dir: fixture.config_path(),
        },
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("heartbeat lock loss must fail saved-plan apply");

    assert!(matches!(error, AppError::StateLockLost { .. }));
    assert_eq!(backend.locked_writes(), 0, "lost lock must not save state");
    assert_eq!(backend.releases(), 0, "lost lock must not be released");
}

#[tokio::test]
async fn apply_cancel_token_integration_persists_completed_state_releases_lock_and_returns_interrupted()
 {
    let user_dirs = carina_core::TestUserDirs::new();
    // Regression test for #3498: a cancelled apply must persist completed
    // resources to state, release the lock, and return AppError::Interrupted.
    // The resource names mirror the publish-ALB scenario that surfaced this
    // bug in production.
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["alb", "listener", "target_group"])
        .cancel_after_successes(1);
    let token = fixture.cancel_token();

    let observer_factory = fixture.observer_factory();
    let err = run_apply_with_observer_factory(
        &user_dirs,
        fixture.config_path(),
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        false,
        fixture.provider_context(),
        token,
        &observer_factory,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, AppError::Interrupted));
    let state = fixture.read_state().await;
    assert!(
        state
            .find_resource("mock", "test.resource", "alb")
            .is_some(),
        "alb must be persisted to state (it completed before cancel)"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "listener")
            .is_none(),
        "listener must not be in state (cancelled before completion)"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "target_group")
            .is_none(),
        "target_group must not be in state (cancelled before completion)"
    );
    assert!(!fixture.lock_path().exists(), "lock file must be released");
}

#[tokio::test]
async fn apply_cleanup_priority_persists_completed_state_and_releases_lock() {
    let user_dirs = carina_core::TestUserDirs::new();
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    let fixture = ApplyCancellationFixture::new()
        .with_resources(["a_completed", "z_blocked"])
        .with_blocked_create("z_blocked");
    let shutdown = fixture.cancel_token();
    let observer_factory = fixture.observer_factory();

    let apply = run_apply_with_observer_factory(
        &user_dirs,
        fixture.config_path(),
        true,
        true,
        NonZeroUsize::new(1).unwrap(),
        false,
        fixture.provider_context(),
        shutdown,
        &observer_factory,
    );
    tokio::pin!(apply);
    tokio::select! {
        () = fixture.wait_for_blocked_create() => {}
        result = &mut apply => panic!("apply returned before the blocking create started: {result:?}"),
    }
    fixture.prioritize_cleanup();
    let err = apply.await.unwrap_err();

    assert!(matches!(err, AppError::Interrupted));
    let state = fixture.read_state().await;
    assert!(
        state
            .find_resource("mock", "test.resource", "a_completed")
            .is_some(),
        "the result folded before cleanup priority must be flushed"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "z_blocked")
            .is_none(),
        "the abandoned create must not be persisted"
    );
    assert!(
        !fixture.lock_path().exists(),
        "cleanup must release the lock"
    );
}

#[tokio::test]
async fn run_apply_locked_with_create_failure_persists_resolved_export_only() {
    let _env_guard = MOCK_PROVIDER_ENV_LOCK.lock().await;
    unsafe {
        std::env::remove_var("CARINA_MOCK_STATE_FILE");
    }
    let fixture = ApplyCancellationFixture::new()
        .with_resources_and_exports(["a", "b"], &[("ax", "a.name"), ("bx", "b.id")]);
    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = validate_with_test_factories(
        &parsed,
        base_dir,
        false,
        vec![Box::new(FailBCreateFactory)],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let wiring = ValidatedWiring::from_factories_for_test(vec![Box::new(FailBCreateFactory)], &[]);
    let observer_factory = fixture.observer_factory();
    let err = run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .unwrap_err();

    let message = err.to_string();
    assert!(
        message.contains("1 succeeded")
            && message.contains("1 failed")
            && message.contains("1 export not written"),
        "partial apply must report both the resource and export failures, got {err:?}",
    );

    let state = fixture.read_state().await;
    assert!(
        state.find_resource("mock", "test.resource", "a").is_some(),
        "successful resource A must be persisted"
    );
    assert!(
        state.find_resource("mock", "test.resource", "b").is_none(),
        "resource B must not be persisted"
    );
    assert_eq!(
        state
            .exports
            .get("ax")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::json!("a"))
    );
    assert!(!state.exports.contains_key("bx"));
    assert_eq!(state.serial, 1);
}

#[tokio::test]
async fn run_apply_locked_defers_value_resolvable_data_source_read_until_referenced_create_exists()
{
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_time_read_crn(fixture.state_path());
    let fixture = fixture.with_raw_config(crn);

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let shared = ApplyTimeReadShared::default();
    let validation_errors = validate_with_test_factories(
        &parsed,
        base_dir,
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let observer_factory = fixture.observer_factory();

    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(4).unwrap(),
        false,
    )
    .await
    .expect("apply should defer the read until target_role has been created");

    assert_eq!(
        shared.operations.lock().unwrap().as_slice(),
        ["create:target_role", "read:roles", "create:consumer"],
        "apply-time read must run between the upstream create and downstream consumer create"
    );
    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        1,
        "read must execute exactly once at apply time, not once during refresh and again at apply"
    );
    assert_eq!(
        *shared.consumer_description.lock().unwrap(),
        Some(Value::Concrete(ConcreteValue::String(
            "carina-acc-test-3666-target".into()
        ))),
        "consumer create must receive the data-source output resolved from the apply-time read"
    );

    let state = fixture.read_state().await;
    assert!(
        state
            .find_resource("mock", "iam.Role", "target_role")
            .is_some(),
        "target role must be persisted"
    );
    assert!(
        state
            .find_resource("mock", "iam.Role", "consumer")
            .is_some(),
        "consumer role must be persisted"
    );
}

#[tokio::test]
async fn run_apply_locked_deposes_old_cbd_instance_when_delete_fails() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), false);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    shared.fail_delete_identifier("vpc-old");
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let ctx = wiring.context();

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = validate_with_test_factories(
        &parsed,
        base_dir,
        false,
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    seed_apply_cascade_state(&fixture, ctx, &expanded_provider_configs(&parsed), false);

    let observer_factory = fixture.observer_factory();
    let err = run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("old VPC delete should fail after replacement create succeeds");

    assert!(
        err.to_string()
            .contains("Apply failed. 1 succeeded, 1 failed."),
        "partial apply must surface the delete failure, got {err:?}"
    );
    assert_eq!(
        shared.delete_calls(),
        vec![("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string())]
    );
    assert_vpc_new_current_and_old_deposed(fixture.state_path());
}

#[tokio::test]
async fn run_apply_locked_deposes_old_cbd_instance_when_delete_is_skipped_by_dependency_failure() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), true);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    shared.fail_delete_identifier("subnet-old");
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let ctx = wiring.context();

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = validate_with_test_factories(
        &parsed,
        base_dir,
        false,
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    seed_apply_cascade_state(&fixture, ctx, &expanded_provider_configs(&parsed), true);

    let observer_factory = fixture.observer_factory();
    let err = run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("old subnet delete failure should skip old VPC delete");

    assert!(
        err.to_string().contains("1 skipped"),
        "partial apply must include a dependency-failure skip, got {err:?}"
    );
    let delete_calls = shared.delete_calls();
    assert_eq!(
        delete_calls.len(),
        1,
        "old VPC delete should be skipped after old subnet delete fails; calls: {delete_calls:?}"
    );
    assert_eq!(delete_calls[0].1, "subnet-old");
    assert_vpc_new_current_and_old_deposed(fixture.state_path());

    let raw = std::fs::read_to_string(fixture.state_path()).expect("state file must be written");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("state must be valid JSON");
    let old_subnet_identity = concrete_subnet_identity(
        ctx,
        &expanded_provider_configs(&parsed),
        "vpc-old",
        "10.220.1.0/24",
    );
    let subnet_rows = state["resources"]
        .as_array()
        .expect("state resources must be an array")
        .iter()
        .filter(|row| row["provider"] == "awscc" && row["resource_type"] == "ec2.Subnet")
        .collect::<Vec<_>>();
    let [subnet] = subnet_rows.as_slice() else {
        panic!("expected exactly one persisted subnet row: {subnet_rows:#?}")
    };
    assert_ne!(
        subnet["identity"],
        old_subnet_identity.as_str(),
        "Move+Replace writeback must not leave the anonymous subnet under its old hash",
    );
    assert_eq!(
        subnet["current"]["identifier"],
        serde_json::json!("subnet-new")
    );
    let subnet_deposed = subnet
        .get("deposed")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| {
            panic!("expected subnet-old to be recorded as deposed; row: {subnet:#?}")
        });
    assert_eq!(
        subnet_deposed.len(),
        1,
        "expected exactly one deposed subnet"
    );
    assert_eq!(
        subnet_deposed[0]["identifier"],
        serde_json::json!("subnet-old")
    );
}

#[tokio::test]
async fn run_apply_locked_deletes_successful_deposed_generation_and_keeps_current_row() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), false);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    seed_apply_cascade_state_with_deposed(&fixture, &["vpc-old"]);

    run_apply_cascade_fixture(&fixture, &shared)
        .await
        .expect("deposed-only delete should succeed");

    assert_eq!(
        shared.delete_calls(),
        vec![("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string())],
        "apply must execute the deposed generation delete, not the current instance"
    );
    let state = fixture.read_state().await;
    let row = state
        .find_resource("awscc", "ec2.Vpc", "vpc")
        .expect("current row must survive a successful deposed delete");
    assert_eq!(
        row.identifier().map(|value| value.as_str()),
        Some("vpc-new")
    );
    assert!(
        row.deposed.is_empty(),
        "successful deposed delete should remove exactly that generation"
    );
}

#[tokio::test]
async fn run_apply_locked_keeps_failed_deposed_generation_and_replans_it() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), false);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    shared.fail_delete_identifier("vpc-old");
    seed_apply_cascade_state_with_deposed(&fixture, &["vpc-old"]);

    let first = run_apply_cascade_fixture(&fixture, &shared)
        .await
        .expect_err("failed deposed delete should fail apply");
    assert!(
        first.to_string().contains("Apply failed"),
        "unexpected first apply error: {first:?}"
    );
    assert_eq!(deposed_identifiers(fixture.state_path()), vec!["vpc-old"]);

    let second = run_apply_cascade_fixture(&fixture, &shared)
        .await
        .expect_err("failed deposed delete should be planned again");
    assert!(
        second.to_string().contains("Apply failed"),
        "unexpected second apply error: {second:?}"
    );
    assert_eq!(
        shared.delete_calls(),
        vec![
            ("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string()),
            ("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string()),
        ],
        "the failed deposed generation must be retried on the next apply"
    );
    assert_eq!(deposed_identifiers(fixture.state_path()), vec!["vpc-old"]);
}

#[tokio::test]
async fn run_apply_locked_removes_only_successful_deposed_generation() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_cascade_crn(fixture.state_path(), false);
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    shared.fail_delete_identifier("vpc-older");
    seed_apply_cascade_state_with_deposed(&fixture, &["vpc-old", "vpc-older"]);

    let err = run_apply_cascade_fixture(&fixture, &shared)
        .await
        .expect_err("one failed deposed delete should fail apply");
    assert!(
        err.to_string().contains("Apply failed"),
        "unexpected apply error: {err:?}"
    );

    assert_eq!(
        shared.delete_calls(),
        vec![
            ("awscc.ec2.Vpc.vpc".to_string(), "vpc-old".to_string()),
            ("awscc.ec2.Vpc.vpc".to_string(), "vpc-older".to_string()),
        ],
        "each deposed generation should be independently deletable"
    );
    assert_eq!(
        deposed_identifiers(fixture.state_path()),
        vec!["vpc-older"],
        "only the successful deposed generation should be removed"
    );
}

#[tokio::test]
async fn run_apply_locked_rekeys_anonymous_child_unresolved_source_by_effect_id() {
    let fixture = ApplyCancellationFixture::new();
    let crn = format!(
        r#"backend local {{ path = "{}" }}
provider awscc {{}}

let vpc = awscc.ec2.Vpc {{
  cidr_block = "10.221.0.0/16"
  directives {{
    create_before_destroy = true
  }}
}}

awscc.ec2.Subnet {{
  vpc_id = vpc.vpc_id
  cidr_block = "10.221.1.0/24"
  availability_zone = "ap-northeast-1c"
}}
"#,
        fixture.state_path().display()
    );
    let fixture = fixture.with_raw_config(crn);
    let shared = ApplyCascadeShared::default();
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let ctx = wiring.context();

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let validation_errors = validate_with_test_factories(
        &parsed,
        base_dir,
        false,
        vec![Box::new(ApplyCascadeAwsccFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );

    let mut vpc_state = ResourceState::try_applied("ec2.Vpc", "vpc", "awscc", "vpc-old")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.220.0.0/16"))
        .with_attribute("vpc_id", serde_json::json!("vpc-old"));
    vpc_state.binding = Some("vpc".to_string());

    let mut subnet_state = ResourceState::try_applied(
        "ec2.Subnet",
        concrete_subnet_identity(
            ctx,
            &expanded_provider_configs(&parsed),
            "vpc-old",
            "10.220.1.0/24",
        ),
        "awscc",
        "subnet-old",
    )
    .expect("fixture identifier is non-empty")
    .with_attribute("vpc_id", serde_json::json!("vpc-old"))
    .with_attribute("cidr_block", serde_json::json!("10.220.1.0/24"))
    .with_attribute("availability_zone", serde_json::json!("ap-northeast-1c"));
    subnet_state.dependency_bindings.insert("vpc".to_string());

    let mut state_file = carina_state::StateFile::new();
    state_file
        .upsert_resource(vpc_state)
        .expect("test state setup must be valid");
    state_file
        .upsert_resource(subnet_state)
        .expect("test state setup must be valid");
    fixture.write_state(&state_file);

    let observer_factory = fixture.observer_factory();
    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect("apply should replace the VPC and anonymous subnet");

    let create_calls = shared.create_calls();
    let subnet_call = create_calls
        .iter()
        .find(|(id, _)| id.contains(".ec2.Subnet."))
        .unwrap_or_else(|| panic!("subnet create missing from calls: {create_calls:?}"));
    assert_eq!(
        subnet_call.1.get("vpc_id"),
        Some(&string_value("vpc-new")),
        "anonymous subnet create must re-resolve vpc.vpc_id from the replacement VPC state; calls: {create_calls:?}"
    );
}

#[tokio::test]
async fn post_apply_plan_refreshes_existing_resource_data_source_and_is_idempotent() {
    let fixture = ApplyCancellationFixture::new();
    let crn = apply_time_read_crn(fixture.state_path());
    let fixture = fixture.with_raw_config(crn);

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let shared = ApplyTimeReadShared::default();
    let validation_errors = validate_with_test_factories(
        &parsed,
        base_dir,
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let ctx = wiring.context();
    let observer_factory = fixture.observer_factory();

    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(4).unwrap(),
        false,
    )
    .await
    .expect("initial apply should succeed");

    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        1,
        "first run should read at apply time exactly once"
    );

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must reload");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let validation_errors = validate_with_test_factories(
        &parsed,
        base_dir,
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate for plan, got: {validation_errors:?}"
    );

    let state_file = Some(fixture.read_state().await);
    let target_state = state_file
        .as_ref()
        .and_then(|state| state.find_resource("mock", "iam.Role", "target_role"))
        .expect("target role must be persisted before post-apply plan");
    assert_eq!(
        target_state
            .persisted_attributes()
            .expect("target has applied attributes")
            .get("path"),
        carina_core::value::json_to_dsl_value(&serde_json::json!("/")),
        "fixture must mirror AWS read-back defaults that are absent from config"
    );
    assert_eq!(
        target_state
            .persisted_attributes()
            .expect("target has applied attributes")
            .get("max_session_duration"),
        carina_core::value::json_to_dsl_value(&serde_json::json!(3600)),
        "fixture must mirror AWS read-back defaults that are absent from config"
    );

    let provider_configs = resolved_provider_configs(&parsed);
    let canonical_resources = canonical_unresolved_for_test(ctx, &parsed, &unresolved_parsed);
    let validated = wiring.validated_parser_pair_for_test(canonical_resources);
    let plan_ctx = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &state_file,
        true,
        crate::commands::shared::progress::ProgressSink::Human,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("post-apply plan should succeed");

    assert!(
        !plan_ctx.resources().plan.has_mutations(),
        "post-apply plan must be idempotent"
    );
    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        2,
        "post-apply plan must refresh the data source instead of deferring it"
    );
}

#[tokio::test]
async fn noop_writeback_does_not_turn_canonical_empty_tags_into_an_explicit_removal() {
    let fixture = ApplyCancellationFixture::new();
    let state_path = fixture.state_path().to_path_buf();
    let config = |nat_tag: &str| {
        format!(
            r#"backend local {{ path = "{}" }}
provider mock {{}}

let igw = mock.ec2.InternetGateway {{}}

let nat = mock.ec2.NatGateway {{
  tags = {{ version = "{nat_tag}" }}
}}
"#,
            state_path.display()
        )
    };
    let fixture = fixture.with_raw_config(config("one"));
    let base_dir = get_base_dir(fixture.config_path());
    let shared = ApplyTimeReadShared::default();
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let ctx = wiring.context();
    let load = || {
        let loaded = load_configuration_with_config(
            fixture.config_path(),
            fixture.provider_context(),
            &SchemaRegistry::new(),
        )
        .expect("fixture must load");
        let parsed = loaded.parsed;
        let validation_errors = validate_with_test_factories(
            &parsed,
            base_dir,
            false,
            vec![Box::new(ApplyTimeReadFactory {
                shared: shared.clone(),
            })],
            HashMap::new(),
            &loaded.duplicate_declarations,
        );
        assert!(
            validation_errors.is_empty(),
            "fixture must validate, got: {validation_errors:?}"
        );
        (parsed, loaded.unresolved_parsed)
    };

    let (parsed, unresolved_parsed) = load();
    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &fixture.observer_factory(),
        NonZeroUsize::new(2).unwrap(),
        false,
    )
    .await
    .expect("initial apply should create both resources");

    let step1_state = fixture.read_state().await;
    let step1_row = serde_json::to_value(
        step1_state
            .find_resource("mock", "ec2.InternetGateway", "igw")
            .expect("IGW row must exist after create"),
    )
    .expect("IGW row must serialize");
    assert_eq!(
        step1_row["current"]["attributes"],
        serde_json::json!({}),
        "the create response intentionally omits canonical read defaults"
    );
    assert_eq!(
        step1_row["explicit"],
        serde_json::json!({ "kind": "struct", "children": {} }),
        "an empty resource records that the user authored no fields"
    );

    let (parsed, unresolved_parsed) = load();
    let provider_configs = resolved_provider_configs(&parsed);
    let step1_state = Some(step1_state);
    let canonical_resources = canonical_unresolved_for_test(ctx, &parsed, &unresolved_parsed);
    let validated = wiring.validated_parser_pair_for_test(canonical_resources);
    let clean_plan = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &step1_state,
        true,
        crate::commands::shared::progress::ProgressSink::Human,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("plan after create should succeed");
    assert!(
        !clean_plan.resources().plan.has_mutations(),
        "the first refresh plan must ignore the read-only canonical tags default: {:#?}",
        clean_plan.resources().plan
    );

    std::fs::write(fixture.config_path().join("main.crn"), config("two"))
        .expect("update sibling configuration");
    let (parsed, unresolved_parsed) = load();
    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &fixture.observer_factory(),
        NonZeroUsize::new(2).unwrap(),
        false,
    )
    .await
    .expect("sibling-only update should apply");

    let step2_state = fixture.read_state().await;
    let step2_row = serde_json::to_value(
        step2_state
            .find_resource("mock", "ec2.InternetGateway", "igw")
            .expect("IGW row must survive the sibling update"),
    )
    .expect("IGW row must serialize");
    assert_eq!(
        step2_row["current"]["attributes"]["tags"],
        serde_json::json!({}),
        "the provider read must canonicalize absent tags to an empty map"
    );
    assert_eq!(
        step2_row["explicit"],
        serde_json::json!({ "kind": "struct", "children": {} }),
        "provider-read defaults must not become authored fields during sibling writeback"
    );

    let (parsed, unresolved_parsed) = load();
    let provider_configs = resolved_provider_configs(&parsed);
    let step2_state = Some(step2_state);
    let canonical_resources = canonical_unresolved_for_test(ctx, &parsed, &unresolved_parsed);
    let validated = wiring.validated_parser_pair_for_test(canonical_resources);
    let final_plan = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &step2_state,
        true,
        crate::commands::shared::progress::ProgressSink::Human,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("plan after sibling update should succeed");
    assert!(
        !final_plan.resources().plan.has_mutations(),
        "changing only the sibling must leave the IGW clean: {:#?}",
        final_plan.resources().plan,
    );
}

#[tokio::test]
async fn failed_update_carries_live_pending_tags_removal_and_replans_it() {
    let fixture = ApplyCancellationFixture::new();
    let state_path = fixture.state_path().to_path_buf();
    let config = |with_tags: bool| {
        let tags = if with_tags {
            "\n  tags = { environment = \"production\" }"
        } else {
            ""
        };
        format!(
            r#"backend local {{ path = "{}" }}
provider mock {{}}

let igw = mock.ec2.InternetGateway {{{tags}
  name = "x"
}}
"#,
            state_path.display()
        )
    };
    let fixture = fixture.with_raw_config(config(true));
    let base_dir = get_base_dir(fixture.config_path());
    let shared = ApplyTimeReadShared::default();
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let ctx = wiring.context();
    let load = || {
        let loaded = load_configuration_with_config(
            fixture.config_path(),
            fixture.provider_context(),
            &SchemaRegistry::new(),
        )
        .expect("fixture must load");
        let parsed = loaded.parsed;
        let validation_errors = validate_with_test_factories(
            &parsed,
            base_dir,
            false,
            vec![Box::new(ApplyTimeReadFactory {
                shared: shared.clone(),
            })],
            HashMap::new(),
            &loaded.duplicate_declarations,
        );
        assert!(
            validation_errors.is_empty(),
            "fixture must validate, got: {validation_errors:?}"
        );
        (parsed, loaded.unresolved_parsed)
    };

    let (parsed, unresolved_parsed) = load();
    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &fixture.observer_factory(),
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect("initial apply should create the IGW with tags");

    std::fs::write(fixture.config_path().join("main.crn"), config(false))
        .expect("remove tags from desired configuration");
    let (parsed, unresolved_parsed) = load();
    let error = run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &fixture.observer_factory(),
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("the fixture rejects IGW updates");
    assert!(error.to_string().contains("failed"), "{error:?}");

    let saved = fixture.read_state().await;
    let row = serde_json::to_value(
        saved
            .find_resource("mock", "ec2.InternetGateway", "igw")
            .expect("failed update must retain the IGW row"),
    )
    .expect("IGW row must serialize");
    assert!(
        row["explicit"]["children"].get("tags").is_some(),
        "failed update must retain the still-live pending tags removal: {row:#?}"
    );

    let (parsed, unresolved_parsed) = load();
    let provider_configs = resolved_provider_configs(&parsed);
    let saved = Some(saved);
    let canonical_resources = canonical_unresolved_for_test(ctx, &parsed, &unresolved_parsed);
    let validated = wiring.validated_parser_pair_for_test(canonical_resources);
    let retry = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &saved,
        true,
        crate::commands::shared::progress::ProgressSink::Human,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("retry plan should succeed");
    assert!(
        retry
            .resources()
            .plan
            .effects()
            .iter()
            .any(|effect| matches!(
                effect,
                carina_core::effect::Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == "tags")
            )),
        "the next plan must retry the failed tags removal: {:#?}",
        retry.resources().plan,
    );
}

#[derive(Clone, Copy)]
enum NestedPendingRemovalCase {
    StructAttribute,
    ListElement,
    DuplicateListElement,
}

impl NestedPendingRemovalCase {
    fn top_level_attribute(self) -> &'static str {
        match self {
            Self::StructAttribute => "settings",
            Self::ListElement | Self::DuplicateListElement => "rules",
        }
    }

    fn config(self, state_path: &Path, with_description: bool) -> String {
        let nested = match (self, with_description) {
            (Self::StructAttribute, true) => {
                r#"settings = { enabled = true, description = "web" }"#
            }
            (Self::StructAttribute, false) => r#"settings = { enabled = true }"#,
            (Self::ListElement, true) => {
                r#"rules = [{ port = 80, description = "web" }, { port = 443 }]"#
            }
            (Self::ListElement, false) => r#"rules = [{ port = 80 }, { port = 443 }]"#,
            (Self::DuplicateListElement, true) => {
                r#"rules = [{ port = 80, description = "web" }, { port = 80 }]"#
            }
            (Self::DuplicateListElement, false) => r#"rules = [{ port = 80 }, { port = 80 }]"#,
        };
        format!(
            r#"backend local {{ path = "{}" }}
provider mock {{}}

let igw = mock.ec2.InternetGateway {{
  name = "x"
  {nested}
}}
"#,
            state_path.display()
        )
    }
}

async fn assert_failed_nested_pending_removal_is_replanned(
    case: NestedPendingRemovalCase,
    complete_retry: bool,
) {
    let fixture = ApplyCancellationFixture::new();
    let state_path = fixture.state_path().to_path_buf();
    let fixture = fixture.with_raw_config(case.config(&state_path, true));
    let base_dir = get_base_dir(fixture.config_path());
    let shared = ApplyTimeReadShared::default();
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let ctx = wiring.context();
    let load = || {
        let loaded = load_configuration_with_config(
            fixture.config_path(),
            fixture.provider_context(),
            &SchemaRegistry::new(),
        )
        .expect("fixture must load");
        let parsed = loaded.parsed;
        let validation_errors = validate_with_test_factories(
            &parsed,
            base_dir,
            false,
            vec![Box::new(ApplyTimeReadFactory {
                shared: shared.clone(),
            })],
            HashMap::new(),
            &loaded.duplicate_declarations,
        );
        assert!(
            validation_errors.is_empty(),
            "fixture must validate, got: {validation_errors:?}"
        );
        (parsed, loaded.unresolved_parsed)
    };

    let (parsed, unresolved_parsed) = load();
    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &fixture.observer_factory(),
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect("initial apply should create the IGW with the nested field");

    std::fs::write(
        fixture.config_path().join("main.crn"),
        case.config(&state_path, false),
    )
    .expect("remove the nested field from desired configuration");
    let (parsed, unresolved_parsed) = load();
    let provider_configs = resolved_provider_configs(&parsed);
    let before_failure = Some(fixture.read_state().await);
    let canonical_resources = canonical_unresolved_for_test(ctx, &parsed, &unresolved_parsed);
    let validated = wiring.validated_parser_pair_for_test(canonical_resources);
    let removal_plan = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &before_failure,
        true,
        crate::commands::shared::progress::ProgressSink::Human,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("the removal plan should succeed");
    let attribute = case.top_level_attribute();
    assert!(
        removal_plan
            .resources()
            .plan
            .effects()
            .iter()
            .any(|effect| matches!(
                effect,
                carina_core::effect::Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == attribute)
            )),
        "an Applied row must initially plan the nested removal: {:#?}",
        removal_plan.resources().plan,
    );

    let error = run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &fixture.observer_factory(),
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect_err("the fixture rejects IGW updates");
    assert!(error.to_string().contains("failed"), "{error:?}");

    let saved = fixture.read_state().await;
    let row = serde_json::to_value(
        saved
            .find_resource("mock", "ec2.InternetGateway", "igw")
            .expect("failed update must retain the IGW row"),
    )
    .expect("IGW row must serialize");
    match case {
        NestedPendingRemovalCase::StructAttribute => assert_eq!(
            row["current"]["attributes"]["settings"]["description"],
            serde_json::json!("web"),
            "the failed provider update must leave the nested struct field live"
        ),
        NestedPendingRemovalCase::ListElement | NestedPendingRemovalCase::DuplicateListElement => {
            assert_eq!(
                row["current"]["attributes"]["rules"][0]["description"],
                serde_json::json!("web"),
                "the failed provider update must leave the list-element field live"
            )
        }
    }

    if complete_retry {
        assert_eq!(
            row["explicit"]["children"]["rules"]["kind"],
            serde_json::json!("whole-value"),
            "ambiguous carried list identity must fall back to whole-value authoring"
        );
    }

    let (parsed, unresolved_parsed) = load();
    let provider_configs = resolved_provider_configs(&parsed);
    let saved = Some(saved);
    let canonical_resources = canonical_unresolved_for_test(ctx, &parsed, &unresolved_parsed);
    let validated = wiring.validated_parser_pair_for_test(canonical_resources);
    let retry = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &saved,
        true,
        crate::commands::shared::progress::ProgressSink::Human,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("retry plan should succeed");
    assert!(
        retry
            .resources()
            .plan
            .effects()
            .iter()
            .any(|effect| matches!(
                effect,
                carina_core::effect::Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == attribute)
            )),
        "the next plan must retry the failed nested removal; explicit={:#?}, plan={:#?}",
        row["explicit"],
        retry.resources().plan,
    );

    if !complete_retry {
        return;
    }

    shared
        .allow_internet_gateway_updates
        .store(true, Ordering::SeqCst);
    let (parsed, unresolved_parsed) = load();
    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &fixture.observer_factory(),
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await
    .expect("the retry update should apply the desired whole list");

    let applied = fixture.read_state().await;
    let applied_row = serde_json::to_value(
        applied
            .find_resource("mock", "ec2.InternetGateway", "igw")
            .expect("successful retry must retain the IGW row"),
    )
    .expect("applied IGW row must serialize");
    assert_eq!(
        applied_row["current"]["attributes"]["rules"],
        serde_json::json!([{ "port": 80 }, { "port": 80 }]),
        "the successful retry must send the current desired list"
    );
    assert_eq!(
        applied_row["explicit"]["children"]["rules"]["kind"],
        serde_json::json!("list-elements"),
        "an Applied write must clear the whole-value fallback"
    );

    let (parsed, unresolved_parsed) = load();
    let provider_configs = resolved_provider_configs(&parsed);
    let canonical_resources = canonical_unresolved_for_test(ctx, &parsed, &unresolved_parsed);
    let validated = wiring.validated_parser_pair_for_test(canonical_resources);
    let final_plan = crate::wiring::create_plan_from_parsed_with_upstream_with_ctx(
        validated,
        &provider_configs,
        &Some(applied),
        true,
        crate::commands::shared::progress::ProgressSink::Human,
        &HashMap::new(),
        &carina_core::identifier::StateBlockClaims::empty(),
        &crate::wiring::ResolvedStateBlockTargets::default(),
    )
    .await
    .expect("plan after the successful retry should succeed");
    assert!(
        !final_plan.resources().plan.has_mutations(),
        "the Applied write must converge: {:#?}",
        final_plan.resources().plan
    );
}

#[tokio::test]
async fn failed_update_nested_struct_pending_removal_is_replanned() {
    assert_failed_nested_pending_removal_is_replanned(
        NestedPendingRemovalCase::StructAttribute,
        false,
    )
    .await;
}

#[tokio::test]
async fn failed_update_list_element_nested_pending_removal_is_replanned() {
    assert_failed_nested_pending_removal_is_replanned(NestedPendingRemovalCase::ListElement, false)
        .await;
}

#[tokio::test]
async fn ambiguous_list_pending_removal_retries_whole_value_and_converges() {
    assert_failed_nested_pending_removal_is_replanned(
        NestedPendingRemovalCase::DuplicateListElement,
        true,
    )
    .await;
}

#[tokio::test]
async fn run_apply_locked_orders_deferred_data_source_read_chain() {
    let fixture = ApplyCancellationFixture::new();
    let crn = format!(
        r#"backend local {{ path = "{}" }}
provider mock {{}}

let target_role = mock.iam.Role {{
  role_name = "carina-acc-test-3666-target"
}}

let roles = read mock.iam.Roles {{
  name_regex = "^${{target_role.role_name}}$"
}}

let roles_again = read mock.iam.Roles {{
  name_regex = "^${{roles.names[0]}}$"
}}

let consumer = mock.iam.Role {{
  role_name = "carina-acc-test-3666-consumer"
  description = roles_again.names[0]
}}
"#,
        fixture.state_path().display()
    );
    let fixture = fixture.with_raw_config(crn);

    let loaded = load_configuration_with_config(
        fixture.config_path(),
        fixture.provider_context(),
        &SchemaRegistry::new(),
    )
    .expect("fixture must load");
    let parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let base_dir = get_base_dir(fixture.config_path());
    let shared = ApplyTimeReadShared::default();
    let validation_errors = validate_with_test_factories(
        &parsed,
        base_dir,
        false,
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        HashMap::new(),
        &loaded.duplicate_declarations,
    );
    assert!(
        validation_errors.is_empty(),
        "fixture must validate, got: {validation_errors:?}"
    );
    let wiring = ValidatedWiring::from_factories_for_test(
        vec![Box::new(ApplyTimeReadFactory {
            shared: shared.clone(),
        })],
        &[],
    );
    let observer_factory = fixture.observer_factory();

    run_apply_locked(
        validated_apply_config_for_test(&wiring, &parsed, &unresolved_parsed),
        true,
        fixture.backend_handle(),
        StateLock::disabled_for_test(),
        base_dir,
        fixture.provider_context(),
        fixture.cancel_token(),
        &observer_factory,
        NonZeroUsize::new(4).unwrap(),
        false,
    )
    .await
    .expect("apply should order chained deferred reads before the consumer");

    assert_eq!(
        shared.operations.lock().unwrap().as_slice(),
        [
            "create:target_role",
            "read:roles",
            "read:roles_again",
            "create:consumer"
        ],
        "read chain must run target create -> read A -> read B -> consumer create"
    );
    assert_eq!(
        shared.read_calls.load(Ordering::SeqCst),
        2,
        "both deferred reads should execute exactly once at apply time"
    );
    assert_eq!(
        *shared.consumer_description.lock().unwrap(),
        Some(Value::Concrete(ConcreteValue::String(
            "carina-acc-test-3666-target".into()
        ))),
        "consumer create must receive the second read's output"
    );
}

fn s3_backend_config_with_encrypt(encrypt: bool) -> carina_core::parser::BackendConfig {
    let mut attributes = HashMap::new();
    attributes.insert(
        "bucket".to_string(),
        Value::Concrete(ConcreteValue::String("state-bucket".into())),
    );
    attributes.insert(
        "key".to_string(),
        Value::Concrete(ConcreteValue::String("project/state.json".into())),
    );
    attributes.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("us-east-1".into())),
    );
    attributes.insert(
        "encrypt".to_string(),
        Value::Concrete(ConcreteValue::Bool(encrypt)),
    );
    carina_core::parser::BackendConfig {
        backend_type: "s3".to_string(),
        attributes,
        attribute_spans: HashMap::new(),
    }
}

#[derive(Clone, Copy)]
enum BootstrapCreateBehavior {
    Success,
    PartialSuccess,
    Untracked,
    Created,
    CreatedNotFound,
    InFlight,
}

struct BootstrapRecordingProvider {
    create_calls: AtomicUsize,
    behavior: BootstrapCreateBehavior,
}

impl Provider for BootstrapRecordingProvider {
    fn name(&self) -> &str {
        "aws"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let id = id.clone();
        Box::pin(async move { Ok(ReadOutcome::not_found(id)) })
    }

    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(State::not_found(id)) })
    }

    fn create(
        &self,
        id: &ResourceId,
        _request: CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        let id = id.clone();
        match self.behavior {
            BootstrapCreateBehavior::Success => {
                let identifier = AppliedIdentifier::new("state-bucket")
                    .expect("fixture identifier is non-empty");
                Box::pin(async move {
                    Ok(attempt
                        .issued(
                            identifier,
                            carina_core::provider::Sent::AllRequestAttributes,
                        )
                        .succeeded(HashMap::from([(
                            "bucket".to_string(),
                            Value::Concrete(ConcreteValue::String("state-bucket".into())),
                        )])))
                })
            }
            BootstrapCreateBehavior::PartialSuccess => {
                let identifier = AppliedIdentifier::new("state-bucket")
                    .expect("fixture identifier is non-empty");
                let diagnostic = carina_core::provider::PartialReadDiagnostic::new(
                    "bootstrap read incomplete".to_string(),
                    vec!["versioning".to_string()],
                )
                .expect("fixture diagnostic has a missing attribute");
                Box::pin(async move {
                    Ok(attempt
                        .issued(
                            identifier,
                            carina_core::provider::Sent::AllRequestAttributes,
                        )
                        .partial(
                            HashMap::from([(
                                "bucket".to_string(),
                                Value::Concrete(ConcreteValue::String("state-bucket".into())),
                            )]),
                            diagnostic,
                        ))
                })
            }
            BootstrapCreateBehavior::Untracked => Box::pin(async move {
                Err(attempt.untracked(ProviderError::internal("bootstrap create failed")))
            }),
            BootstrapCreateBehavior::Created => {
                let identifier =
                    AppliedIdentifier::new("bucket-id").expect("fixture identifier is non-empty");
                let issued = attempt.issued(
                    identifier,
                    carina_core::provider::Sent::AllRequestAttributes,
                );
                let observed = AppliedState::new(
                    id,
                    "bucket-id",
                    HashMap::from([(
                        "observed".to_string(),
                        Value::Concrete(ConcreteValue::String("yes".into())),
                    )]),
                )
                .expect("fixture identifier is non-empty");
                Box::pin(async move {
                    Err(issued.failed(
                        carina_core::provider::Observation::Read(ReadOutcome::found(observed)),
                        ProviderError::internal("bootstrap created then failed"),
                    ))
                })
            }
            BootstrapCreateBehavior::CreatedNotFound => {
                let identifier = AppliedIdentifier::new("missing-bucket-id")
                    .expect("fixture identifier is non-empty");
                let issued = attempt.issued(
                    identifier,
                    carina_core::provider::Sent::only(Vec::<String>::new()),
                );
                Box::pin(async move {
                    Err(issued.failed(
                        carina_core::provider::Observation::Read(ReadOutcome::not_found(id)),
                        ProviderError::internal("bootstrap identifier could not be observed"),
                    ))
                })
            }
            BootstrapCreateBehavior::InFlight => {
                let pending = attempt.pending(
                    carina_core::provider::PendingToken::new("bootstrap-token"),
                    carina_core::provider::Sent::AllRequestAttributes,
                    1,
                );
                Box::pin(async move {
                    Err(pending.preserve(ProviderError::internal("bootstrap still in flight")))
                })
            }
        }
    }

    unexpected_resume_create!();

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("unexpected update")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("unexpected delete")) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

fn bootstrap_test_backend() -> (tempfile::TempDir, Box<dyn StateBackend>) {
    let temp = tempfile::tempdir().expect("bootstrap state tempdir");
    let backend = crate::commands::local_backend_for_test(temp.path().join("carina.state.json"));
    (temp, backend)
}

struct BootstrapWriteFailureBackend;

#[async_trait::async_trait]
impl StateBackend for BootstrapWriteFailureBackend {
    async fn read_state(&self) -> carina_state::BackendResult<Option<carina_state::LoadedState>> {
        Ok(None)
    }

    async fn read_state_bytes(
        &self,
    ) -> carina_state::BackendResult<Option<carina_state::RawState>> {
        Ok(None)
    }

    async fn write_state(&self, _state: &StateFile) -> carina_state::BackendResult<()> {
        Err(carina_state::BackendError::Configuration(
            "seed write failed".to_string(),
        ))
    }

    async fn acquire_lock(&self, _operation: &str) -> carina_state::BackendResult<LockInfo> {
        Err(carina_state::BackendError::Configuration(
            "lock is not used by bootstrap fixture".to_string(),
        ))
    }

    async fn release_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<()> {
        Err(carina_state::BackendError::Configuration(
            "lock is not used by bootstrap fixture".to_string(),
        ))
    }

    async fn renew_lock(&self, _lock: &LockInfo) -> carina_state::BackendResult<LockInfo> {
        Err(carina_state::BackendError::Configuration(
            "lock is not used by bootstrap fixture".to_string(),
        ))
    }

    async fn write_state_locked(
        &self,
        _state: &StateFile,
        _lock: &LockInfo,
    ) -> carina_state::BackendResult<()> {
        Err(carina_state::BackendError::Configuration(
            "locked write is not used by bootstrap fixture".to_string(),
        ))
    }

    async fn force_unlock(&self, _lock_id: &str) -> carina_state::BackendResult<()> {
        Err(carina_state::BackendError::Configuration(
            "lock is not used by bootstrap fixture".to_string(),
        ))
    }

    async fn bucket_exists(&self) -> carina_state::BackendResult<bool> {
        Ok(true)
    }

    async fn create_bucket(&self) -> carina_state::BackendResult<()> {
        Ok(())
    }
}

async fn bootstrap_declared_state_bucket(
    provider: &BootstrapRecordingProvider,
    backend: &dyn StateBackend,
) -> Result<(Resource, SchemaRegistry), AppError> {
    let resource = Resource::with_provider("aws", "s3.Bucket", "state_bucket", None)
        .with_attribute(
            "bucket",
            Value::Concrete(ConcreteValue::String("state-bucket".into())),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("s3.Bucket")
            .attribute(
                PatternAttributeSchema::optional("bucket", PatternAttributeType::string()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional("versioning", PatternAttributeType::string())
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let provider_configs = empty_resolved_provider_configs();
    let parsed = resolve_file_for_test(carina_core::parser::InferredFile::default());

    create_checked_bootstrap_resource_for_test(
        provider,
        resource.clone(),
        BootstrapCreateContext {
            backend,
            parsed: &parsed,
            provider_configs: &provider_configs,
            normalizer: &NoopNormalizer,
            factories: &[],
            schemas: &schemas,
        },
    )
    .await?;

    Ok((resource, schemas))
}

#[tokio::test]
async fn declared_state_bucket_bootstrap_success_seeds_protected_provider_state() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::Success,
    };
    let (_temp, backend) = bootstrap_test_backend();
    let (resource, _schemas) = bootstrap_declared_state_bucket(&provider, backend.as_ref())
        .await
        .expect("declared state bucket create must seed state");
    let id = resource.id.clone();
    let expected_identity = id
        .identity_str()
        .expect("declared resource identity must be resolved")
        .to_string();

    let state = backend
        .read_state()
        .await
        .expect("seed state read")
        .expect("successful bootstrap must write state")
        .into_state();
    assert_eq!(state.resources().len(), 1);
    let row = state
        .find_resource_for_id(&id)
        .expect("seed must use the declared resource's resolved identity");
    assert_eq!(row.identity.as_str(), expected_identity);
    assert!(row.protected);
    assert_eq!(
        row.identifier().map(AppliedIdentifier::as_str),
        Some("state-bucket")
    );
}

#[tokio::test]
async fn declared_state_bucket_bootstrap_partial_success_seeds_partial_read_state() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::PartialSuccess,
    };
    let (_temp, backend) = bootstrap_test_backend();
    let (resource, _schemas) = bootstrap_declared_state_bucket(&provider, backend.as_ref())
        .await
        .expect("partial-success bootstrap must seed state");
    let id = resource.id.clone();

    let state = backend
        .read_state()
        .await
        .expect("seed state read")
        .expect("partial-success bootstrap must write state")
        .into_state();
    let row = state
        .find_resource_for_id(&id)
        .expect("partial-success row must be seeded");
    assert!(row.protected);
    assert_eq!(
        row.identifier().map(AppliedIdentifier::as_str),
        Some("state-bucket")
    );
    let partial = row
        .partial_read()
        .expect("normal create writeback must preserve the partial-read marker");
    assert_eq!(
        partial.pending(),
        &carina_core::resource::PendingReRead::incomplete("bootstrap read incomplete")
    );
    assert_eq!(
        partial.missing_attributes(),
        &BTreeSet::from(["versioning".to_string()])
    );
}

#[test]
fn partial_state_bucket_warning_color_spans_do_not_cross_newlines() {
    struct ColorOverrideGuard;

    impl Drop for ColorOverrideGuard {
        fn drop(&mut self) {
            colored::control::unset_override();
        }
    }

    let diagnostic = carina_core::provider::PartialReadDiagnostic::new(
        "bootstrap read incomplete".to_string(),
        vec!["versioning".to_string()],
    )
    .expect("fixture diagnostic has a missing attribute");
    let rendered = {
        colored::control::set_override(true);
        let _guard = ColorOverrideGuard;
        format_partial_state_bucket_create_warning("state-bucket", &diagnostic)
    };

    assert!(
        rendered.contains("\u{1b}["),
        "fixture must exercise ANSI styling: {rendered:?}"
    );
    let chars: Vec<char> = rendered.chars().collect();
    let mut depth = 0_u32;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\u{1b}' && chars.get(i + 1) == Some(&'[') {
            let mut end = i + 2;
            while end < chars.len() && chars[end] != 'm' {
                end += 1;
            }
            let params: String = chars[i + 2..end].iter().collect();
            if params == "0" {
                depth = depth.saturating_sub(1);
            } else {
                depth += 1;
            }
            i = end + 1;
            continue;
        }
        if chars[i] == '\n' {
            assert_eq!(
                depth, 0,
                "an ANSI color span crossed a newline: {rendered:?}"
            );
        }
        i += 1;
    }
}

#[tokio::test]
async fn declared_state_bucket_bootstrap_seed_prevents_phantom_create_plan() {
    use carina_core::effect::Effect;

    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::Success,
    };
    let (_temp, backend) = bootstrap_test_backend();
    let (resource, schemas) = bootstrap_declared_state_bucket(&provider, backend.as_ref())
        .await
        .expect("declared state bucket create must seed state");
    let id = resource.id.clone();

    let state_file = backend
        .read_state()
        .await
        .expect("seed state read")
        .expect("successful bootstrap must write state")
        .into_state();
    let current_state = match state_file.build_state_for_resource(&id) {
        Current::Present(present) => present.into_parts().0,
        current => panic!("seeded bucket must be present, got {current:?}"),
    };
    let plan_input_states = crate::plan_input_current_from_states(
        HashMap::from([(id.clone(), current_state)]),
        &schemas,
        std::slice::from_ref(&resource),
    );
    let resolved_resource = ResolvedResource::new(resource);
    let saved_attrs = state_file.build_saved_attrs().lift(&schemas);
    let plan = carina_core::differ::create_plan_for_test(
        std::slice::from_ref(&resolved_resource),
        &[],
        &carina_core::provider::ProviderRouter::new(),
        carina_core::differ::PlanPriorStates::new(
            &plan_input_states,
            &carina_core::differ::Orphans::new(),
        ),
        &HashMap::new(),
        &schemas,
        &saved_attrs,
        &state_file.build_explicit(),
        &[],
    )
    .expect("subsequent plan must build from bootstrap seed");

    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Create(created) if created.id == id)),
        "seeded state bucket must not be planned for creation: {:?}",
        plan.effects()
    );
}

#[tokio::test]
async fn declared_state_bucket_successful_create_seed_write_failure_is_actionable() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::Success,
    };
    let error = bootstrap_declared_state_bucket(&provider, &BootstrapWriteFailureBackend)
        .await
        .expect_err("a successful create followed by a seed write failure must be actionable");

    let AppError::StateBucketSeed(seed) = &error else {
        panic!("seed write failure lost its bootstrap context: {error:?}");
    };
    assert_eq!(seed.identifier(), "state-bucket");
    assert_eq!(seed.resource_address(), "aws.s3.Bucket 'state_bucket'");

    let message = error.to_string();
    assert!(
        message
            .contains("State bucket 'state-bucket' was created but could not be recorded in state"),
        "{message}"
    );
    assert!(message.contains("it exists outside state"), "{message}");
    assert!(!message.contains("import {"), "{message}");

    let mut source = std::error::Error::source(&error);
    let mut backend_source = None;
    while let Some(current) = source {
        if let Some(backend_error) = current.downcast_ref::<carina_state::BackendError>() {
            backend_source = Some(backend_error);
            break;
        }
        source = current.source();
    }
    assert!(matches!(
        backend_source,
        Some(carina_state::BackendError::Configuration(message)) if message == "seed write failed"
    ));
}

#[tokio::test]
async fn invalid_state_bucket_bootstrap_resource_never_reaches_provider_create() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::Success,
    };
    let mut resource = Resource::with_provider("aws", "s3.Bucket", "state", None);
    resource.set_attr(
        "bucket",
        Value::Concrete(ConcreteValue::String("invalid-bucket".into())),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional(
                "bucket",
                PatternAttributeType::refined_string(None, Some("^state-".to_string()), None, None),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let provider_configs = empty_resolved_provider_configs();
    let (_temp, backend) = bootstrap_test_backend();
    let parsed = resolve_file_for_test(carina_core::parser::InferredFile::default());
    let error = create_checked_bootstrap_resource_for_test(
        &provider,
        resource,
        BootstrapCreateContext {
            backend: backend.as_ref(),
            parsed: &parsed,
            provider_configs: &provider_configs,
            normalizer: &NoopNormalizer,
            factories: &[],
            schemas: &schemas,
        },
    )
    .await
    .expect_err("invalid bootstrap bucket must fail its value constraint");

    let message = error.to_string();
    assert!(
        message.contains("aws.s3.Bucket.state"),
        "error must name the bootstrap resource: {message}"
    );
    assert!(
        message.contains("'bucket'"),
        "error must name the invalid attribute: {message}"
    );
    assert!(
        message.contains("does not match required pattern /^state-/"),
        "error must retain the value-constraint reason: {message}"
    );
    let AppError::StateBucketPreparation(preparation) = &error else {
        panic!("typed preparation error was erased: {error:?}");
    };
    let carina_core::executor::ProviderPreparationError::ValueConstraint { errors, .. } =
        preparation.preparation_error()
    else {
        panic!("typed schema error was erased: {error:?}");
    };
    assert!(!errors.errors().is_empty());
    assert_eq!(
        message
            .matches("does not match required pattern /^state-/")
            .count(),
        1,
        "{message}"
    );
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(provider.create_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn state_bucket_provider_error_keeps_typed_terminal_error() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::Untracked,
    };
    let mut resource = Resource::with_provider("aws", "s3.Bucket", "state", None);
    resource.set_attr(
        "bucket",
        Value::Concrete(ConcreteValue::String("state-bucket".into())),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket", PatternAttributeType::string()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let provider_configs = empty_resolved_provider_configs();
    let (_temp, backend) = bootstrap_test_backend();
    let error = create_checked_bootstrap_resource_for_test(
        &provider,
        resource,
        BootstrapCreateContext {
            backend: backend.as_ref(),
            parsed: &carina_core::parser::ExpandedInferredFile::default(),
            provider_configs: &provider_configs,
            normalizer: &NoopNormalizer,
            factories: &[],
            schemas: &schemas,
        },
    )
    .await
    .expect_err("provider create failure must escape bootstrap");

    assert_eq!(error.to_string(), "Failed to create state bucket");
    assert!(matches!(&error, AppError::StateBucketCreate(error)
        if error.create_failure().provider_error().detail().context() == "bootstrap create failed"));
    let AppError::StateBucketCreate(create_error) = &error else {
        panic!("typed create failure was erased: {error:?}");
    };
    let provider_error = std::error::Error::source(create_error.create_failure())
        .expect("create failure must preserve the provider error");
    assert_eq!(provider_error.to_string(), "bootstrap create failed");
}

#[tokio::test]
async fn violating_module_constraint_blocks_state_bucket_bootstrap_create() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::Success,
    };
    let mut resource = Resource::with_provider("aws", "s3.Bucket", "state", None);
    resource.set_attr(
        "bucket",
        Value::Concrete(ConcreteValue::String("state-bucket".into())),
    );
    let mut parsed = carina_core::parser::ExpandedInferredFile::default();
    parsed.compositions = vec![saved_module_composition(
        Value::Concrete(ConcreteValue::String("forbidden".into())),
        "forbidden",
    )];
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket", PatternAttributeType::string()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let provider_configs = empty_resolved_provider_configs();
    let (_temp, backend) = bootstrap_test_backend();
    let result = create_checked_bootstrap_resource_for_test(
        &provider,
        resource,
        BootstrapCreateContext {
            backend: backend.as_ref(),
            parsed: &parsed,
            provider_configs: &provider_configs,
            normalizer: &NoopNormalizer,
            factories: &[],
            schemas: &schemas,
        },
    )
    .await;

    assert_eq!(
        provider.create_calls.load(Ordering::SeqCst),
        0,
        "the bootstrap provider dispatch must be gated by module constraints"
    );
    let error = result.expect_err("the violating constraint must fail bootstrap preparation");
    assert!(matches!(&error, AppError::ModuleConstraint(_)), "{error:?}");
    assert!(error.to_string().contains("value must not be forbidden"));
}

#[tokio::test]
async fn bootstrap_created_failure_seeds_observed_state_before_error() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::Created,
    };
    let mut resource = Resource::with_provider("aws", "s3.Bucket", "state", None);
    resource.set_attr(
        "bucket",
        Value::Concrete(ConcreteValue::String("state-bucket".into())),
    );
    let id = resource.id.clone();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("s3.Bucket")
            .attribute(
                PatternAttributeSchema::optional("bucket", PatternAttributeType::string()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional("observed", PatternAttributeType::string())
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let provider_configs = empty_resolved_provider_configs();
    let (_temp, backend) = bootstrap_test_backend();
    let error = create_checked_bootstrap_resource_for_test(
        &provider,
        resource,
        BootstrapCreateContext {
            backend: backend.as_ref(),
            parsed: &carina_core::parser::ExpandedInferredFile::default(),
            provider_configs: &provider_configs,
            normalizer: &NoopNormalizer,
            factories: &[],
            schemas: &schemas,
        },
    )
    .await
    .expect_err("created failure must still be reported");

    let AppError::StateBucketCreate(error) = &error else {
        panic!("created bootstrap failure lost its classification: {error:?}")
    };
    let carina_core::provider::CreateFailureKind::Created {
        created,
        error: _provider_error,
    } = error.create_failure().kind()
    else {
        panic!("created bootstrap failure returned another verdict: {error:?}")
    };
    match created.observation() {
        carina_core::provider::CreateObservation::Observed(state) => {
            assert_eq!(state.identifier().as_str(), "bucket-id")
        }
        carina_core::provider::CreateObservation::Unobserved(reason) => match reason {
            carina_core::provider::UnobservedReason::NotFound => {
                panic!("found bootstrap observation became not found")
            }
            carina_core::provider::UnobservedReason::ReadFailed(read_error) => {
                panic!("found bootstrap observation became a read failure: {read_error}")
            }
        },
    }
    let state = backend
        .read_state()
        .await
        .expect("seed state read")
        .expect("seed state exists")
        .into_state();
    let row = state
        .find_resource_for_id(&id)
        .expect("created row must be seeded before the error returns");
    assert!(row.protected);
    let carina_state::CurrentInstance::Applied {
        identifier,
        attributes,
        ..
    } = row.current()
    else {
        panic!("created bootstrap failure must seed an applied row");
    };
    assert_eq!(identifier.as_str(), "bucket-id");
    assert_eq!(
        attributes.decoded_json_value("observed"),
        Some(serde_json::json!("yes"))
    );
}

#[tokio::test]
async fn bootstrap_unobserved_created_failure_seeds_identifier_and_partial_read_before_error() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::CreatedNotFound,
    };
    let resource = Resource::with_provider("aws", "s3.Bucket", "state", None).with_attribute(
        "bucket",
        Value::Concrete(ConcreteValue::String("state-bucket".into())),
    );
    let id = resource.id.clone();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket", PatternAttributeType::string()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let provider_configs = empty_resolved_provider_configs();
    let (_temp, backend) = bootstrap_test_backend();
    let error = create_checked_bootstrap_resource_for_test(
        &provider,
        resource,
        BootstrapCreateContext {
            backend: backend.as_ref(),
            parsed: &carina_core::parser::ExpandedInferredFile::default(),
            provider_configs: &provider_configs,
            normalizer: &NoopNormalizer,
            factories: &[],
            schemas: &schemas,
        },
    )
    .await
    .expect_err("unobserved created failure must still be reported");

    let AppError::StateBucketCreate(error) = &error else {
        panic!("unobserved bootstrap failure lost its classification: {error:?}")
    };
    let carina_core::provider::CreateFailureKind::Created {
        created,
        error: _provider_error,
    } = error.create_failure().kind()
    else {
        panic!("unobserved bootstrap failure returned another verdict: {error:?}")
    };
    match created.observation() {
        carina_core::provider::CreateObservation::Observed(state) => {
            panic!("not-found bootstrap observation became found: {state:?}")
        }
        carina_core::provider::CreateObservation::Unobserved(reason) => match reason {
            carina_core::provider::UnobservedReason::NotFound => {}
            carina_core::provider::UnobservedReason::ReadFailed(read_error) => {
                panic!("not-found bootstrap observation became a read failure: {read_error}")
            }
        },
    }

    let state = backend
        .read_state()
        .await
        .expect("seed state read")
        .expect("seed state exists")
        .into_state();
    let row = state
        .find_resource_for_id(&id)
        .expect("unobserved row must be seeded before the error returns");
    assert!(row.protected);
    let carina_state::CurrentInstance::Applied {
        identifier,
        attributes,
        partial_read,
        ..
    } = row.current()
    else {
        panic!("unobserved bootstrap failure must seed an applied row");
    };
    assert_eq!(identifier.as_str(), "missing-bucket-id");
    assert!(attributes.is_empty());
    assert_eq!(
        partial_read,
        &carina_core::resource::PartialReadMarker::new(
            carina_core::resource::PendingReRead::not_found(),
            BTreeSet::from(["bucket".to_string()]),
        )
    );
}

#[tokio::test]
async fn bootstrap_in_flight_failure_seeds_pending_state_before_error() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::InFlight,
    };
    let mut resource = Resource::with_provider("aws", "s3.Bucket", "state", None);
    resource.set_attr(
        "bucket",
        Value::Concrete(ConcreteValue::String("state-bucket".into())),
    );
    let id = resource.id.clone();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket", PatternAttributeType::string()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let provider_configs = empty_resolved_provider_configs();
    let (_temp, backend) = bootstrap_test_backend();
    let error = create_checked_bootstrap_resource_for_test(
        &provider,
        resource,
        BootstrapCreateContext {
            backend: backend.as_ref(),
            parsed: &carina_core::parser::ExpandedInferredFile::default(),
            provider_configs: &provider_configs,
            normalizer: &NoopNormalizer,
            factories: &[],
            schemas: &schemas,
        },
    )
    .await
    .expect_err("in-flight failure must still be reported");

    assert!(matches!(
        &error,
        AppError::StateBucketCreate(error)
            if matches!(
                error.create_failure().kind(),
                carina_core::provider::CreateFailureKind::InFlight {
                    pending: _pending,
                    error: _provider_error,
                }
            )
    ));
    let state = backend
        .read_state()
        .await
        .expect("seed state read")
        .expect("seed state exists")
        .into_state();
    let row = state
        .find_resource_for_id(&id)
        .expect("in-flight row must be seeded before the error returns");
    assert!(row.protected);
    let carina_state::CurrentInstance::CreateInFlight { pending } = row.current() else {
        panic!("in-flight bootstrap failure must seed a continuation row");
    };
    assert_eq!(
        pending.token().expose_for_provider_call(),
        "bootstrap-token"
    );
}

#[tokio::test]
async fn bootstrap_seed_write_failure_keeps_classified_create_and_token() {
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0),
        behavior: BootstrapCreateBehavior::InFlight,
    };
    let resource = Resource::with_provider("aws", "s3.Bucket", "state", None).with_attribute(
        "bucket",
        Value::Concrete(ConcreteValue::String("state-bucket".into())),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket", PatternAttributeType::string()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let provider_configs = empty_resolved_provider_configs();
    let error = create_checked_bootstrap_resource_for_test(
        &provider,
        resource,
        BootstrapCreateContext {
            backend: &BootstrapWriteFailureBackend,
            parsed: &carina_core::parser::ExpandedInferredFile::default(),
            provider_configs: &provider_configs,
            normalizer: &NoopNormalizer,
            factories: &[],
            schemas: &schemas,
        },
    )
    .await
    .expect_err("the classified create failure remains primary when seeding fails");

    let AppError::StateBucketCreate(error) = &error else {
        panic!("classified create failure was replaced: {error:?}");
    };
    let carina_core::provider::CreateFailureKind::InFlight { pending, error: _ } =
        error.create_failure().kind()
    else {
        panic!("in-flight classification was lost: {error:?}");
    };
    assert_eq!(
        pending.token().expose_for_provider_call(),
        "bootstrap-token"
    );
    assert!(matches!(
        error.seed_error(),
        Some(AppError::Backend(carina_state::BackendError::Configuration(message)))
            if message == "seed write failed"
    ));
    assert_eq!(error.to_string(), "Failed to create state bucket");
    let seed_error = std::error::Error::source(error)
        .expect("the recovery-row failure remains in the source chain");
    assert!(seed_error.downcast_ref::<AppError>().is_some());
    assert_eq!(
        seed_error.to_string(),
        "Backend configuration error: seed write failed"
    );
    assert_eq!(
        std::error::Error::source(error.create_failure())
            .expect("the classified create failure retains its provider source")
            .to_string(),
        "bootstrap still in flight"
    );
}

#[test]
fn saved_plan_backend_cross_check_rejects_non_addressing_attribute_change() {
    let planned = s3_backend_config_with_encrypt(false);
    let current = s3_backend_config_with_encrypt(true);

    let err = ensure_saved_plan_backend_matches_current(
        Some(&planned),
        Some(&current),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("reviewed-plan.json"),
            source_dir: Path::new("infra/foo"),
        },
    )
    .expect_err("any backend attribute change should invalidate a saved plan");
    let msg = err.to_string();

    assert!(msg.contains("Saved plan backend does not match the current `backend.crn`"));
    assert!(msg.contains("The plan file recorded one backend"));
    assert!(msg.contains("Re-run `carina plan --out reviewed-plan.json infra/foo`"));
}

#[test]
fn saved_plan_backend_cross_check_preserves_typed_config_error() {
    let mut planned = s3_backend_config_with_encrypt(false);
    planned.attributes.insert(
        "bucket".to_string(),
        Value::Concrete(ConcreteValue::Bool(true)),
    );
    let current = s3_backend_config_with_encrypt(false);

    let error = ensure_saved_plan_backend_matches_current(
        Some(&planned),
        Some(&current),
        ApplyTarget::SavedPlan {
            plan_file: Path::new("reviewed-plan.json"),
            source_dir: Path::new("infra/foo"),
        },
    )
    .expect_err("an invalid saved backend must fail inspection");

    assert!(
        matches!(
            &error,
            AppError::Backend(carina_state::BackendError::InvalidConfig(
                carina_state::BackendConfigError::Invalid(errors)
            )) if errors.errors().iter().any(|error| matches!(
                error,
                carina_state::BackendAttributeError::InvalidAttribute { attribute, .. }
                    if attribute == "bucket"
            ))
        ),
        "typed backend error was erased: {error:?}"
    );
}

#[test]
fn build_state_after_apply_finds_write_only_with_provider_prefix() {
    // The schema map is keyed by provider-prefixed names (e.g., "awscc.ec2.Vpc"),
    // but the buggy code used resource.id.resource_type (e.g., "ec2.Vpc") for lookup.
    // This test verifies that write-only attributes are found when the schema key
    // includes the provider prefix.
    let mut schemas = SchemaRegistry::new();
    let schema = PatternResourceSchema::new("ec2.Vpc")
        .attribute(
            PatternAttributeSchema::optional("cidr_block", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::optional("ipv4_netmask_length", PatternAttributeType::int())
                .write_only()
                .build(),
        );
    // Schema is registered with provider-prefixed key
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    resource.set_attr(
        "ipv4_netmask_length".to_string(),
        Value::Concrete(ConcreteValue::String("16".into())),
    );

    let sorted_resources = vec![resolved(resource)];

    // Simulate provider returning state without the write-only attribute
    let mut applied_attrs = HashMap::new();
    applied_attrs.insert(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    let applied_state = State::existing(sorted_resources[0].id.clone(), applied_attrs);
    let mut applied_states = HashMap::new();
    applied_states.insert(
        sorted_resources[0].id.clone(),
        complete_create(applied_state),
    );

    let current_states = HashMap::new();
    let plan = Plan::new();
    let successfully_deleted = HashSet::new();
    let failed_refreshes = HashSet::new();

    let result = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        },
        None,
    )
    .unwrap();

    // The write-only attribute should be merged from the desired resource into state
    let saved = result
        .find_resource("awscc", "ec2.Vpc", "my-vpc")
        .expect("resource should exist in state");
    assert_eq!(
        saved
            .persisted_attributes()
            .expect("saved resource has applied attributes")
            .get("ipv4_netmask_length"),
        carina_core::value::json_to_dsl_value(&serde_json::Value::String("16".to_string())),
        "write-only attribute should be persisted in state"
    );
}

#[test]
fn build_state_after_apply_preserves_block_unique_name_attribute() {
    // When a block_name attribute (e.g., "policies" with block_name "policy")
    // is carried over by the provider because CloudControl doesn't return it,
    // the state after apply should include the attribute under the canonical name.
    // This is the scenario in issue #1499 (iam_role/with_policy).

    let mut schemas = SchemaRegistry::new();
    let schema = PatternResourceSchema::new("iam.role")
        .attribute(
            PatternAttributeSchema::optional("role_name", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional(
                "policies",
                PatternAttributeType::unordered_list(PatternAttributeType::struct_(
                    "Policy".to_string(),
                    vec![
                        PatternStructField::required("policy_name", PatternAttributeType::string())
                            .build(),
                        PatternStructField::required(
                            "policy_document",
                            PatternAttributeType::string(),
                        )
                        .build(),
                    ],
                )),
            )
            .with_block_name("policy")
            .build(),
        );
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    // Resource with resolved block name (policy -> policies)
    let mut resource = Resource::with_provider("awscc", "iam.role", "test-role", None);
    resource.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("test-role".into())),
    );
    resource.set_attr(
        "policies".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::Concrete(ConcreteValue::String("test-policy".into())),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::Concrete(ConcreteValue::String("{}".into())),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )])),
    );

    let sorted_resources = vec![resolved(resource)];

    // Simulate provider returning state WITH carried-over policies attribute
    // (This is what AwsccProvider::create_resource does in the carry-over logic)
    let mut applied_attrs = HashMap::new();
    applied_attrs.insert(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("test-role".into())),
    );
    applied_attrs.insert(
        "policies".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::Concrete(ConcreteValue::String("test-policy".into())),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::Concrete(ConcreteValue::String("{}".into())),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )])),
    );
    let applied_state = State::existing(sorted_resources[0].id.clone(), applied_attrs)
        .with_identifier("some-identifier");
    let mut applied_states = HashMap::new();
    applied_states.insert(
        sorted_resources[0].id.clone(),
        complete_create(applied_state),
    );

    let current_states = HashMap::new();
    let plan = Plan::new();
    let successfully_deleted = HashSet::new();
    let failed_refreshes = HashSet::new();

    let state = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        },
        None,
    )
    .unwrap();

    // Verify state has the policies attribute
    let saved = state
        .find_resource("awscc", "iam.role", "test-role")
        .expect("resource should exist in state");
    assert!(
        saved
            .persisted_attributes()
            .expect("saved resource has applied attributes")
            .contains_key("policies"),
        "state should contain 'policies' attribute (carried over from desired)"
    );

    // Verify explicit tree includes "policies" (canonical name, not "policy")
    let carina_core::explicit::ExplicitFields::Struct {
        children: explicit_children,
    } = &saved.explicit
    else {
        panic!("saved.explicit must be Struct, got: {:?}", saved.explicit);
    };
    assert!(
        explicit_children.contains_key("policies"),
        "explicit children should contain 'policies': {:?}",
        explicit_children.keys().collect::<Vec<_>>()
    );

    // Now simulate second plan: build_saved_attrs should return the policies
    let saved_attrs = state.build_saved_attrs();
    let id = carina_core::resource::ResourceId::with_provider_identity(
        "awscc",
        "iam.role",
        "test-role",
        None,
    );
    let attrs = saved_attrs.get(&id).unwrap();
    assert!(
        attrs.contains_key("policies"),
        "saved_attrs should contain 'policies': {:?}",
        attrs.keys().collect::<Vec<_>>()
    );
}

#[test]
fn block_unique_name_attribute_no_diff_when_hydrated() {
    // After apply, the state file contains the block_name attribute (canonical name).
    // On re-plan, if hydrate_read_state restores it into the managed-current map,
    // the differ should see no changes.
    //
    // This tests the scenario from issue #1499 where plan-verify fails
    // because the block_name attribute shows as an addition.
    use carina_core::differ::diff;
    use carina_core::schema::StructField;

    let schema = ResourceSchema::new("awscc.iam.role")
        .attribute(
            AttributeSchema::optional("role_name", AttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "policies",
                AttributeType::unordered_list(AttributeType::struct_(
                    "Policy".to_string(),
                    vec![
                        StructField::required("policy_name", AttributeType::string()).build(),
                        StructField::required("policy_document", AttributeType::string()).build(),
                    ],
                )),
            )
            .with_block_name("policy")
            .build(),
        );

    // Desired resource (after resolve_block_names: "policy" -> "policies")
    let mut resource = Resource::with_provider("awscc", "iam.role", "test-role", None);
    resource.set_attr(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("test-role".into())),
    );
    resource.set_attr(
        "policies".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::Concrete(ConcreteValue::String("test-policy".into())),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::Concrete(ConcreteValue::String("{}".into())),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )])),
    );

    // Current state: simulate hydration restoring the policies attribute
    let mut state_attrs = HashMap::new();
    state_attrs.insert(
        "role_name".to_string(),
        Value::Concrete(ConcreteValue::String("test-role".into())),
    );
    state_attrs.insert(
        "policies".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        Value::Concrete(ConcreteValue::String("test-policy".into())),
                    ),
                    (
                        "policy_document".to_string(),
                        Value::Concrete(ConcreteValue::String("{}".into())),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )])),
    );
    let current = State::existing(resource.id.clone(), state_attrs).with_identifier("some-id");

    // Saved attrs: same as current (from previous apply)
    let saved: HashMap<String, Value> = current.attributes.clone();
    let saved = carina_core::provider::RawSavedAttrs::from_decoded_values_for_test(HashMap::from(
        [(resource.id.clone(), saved)],
    ))
    .lift(&SchemaRegistry::new());

    // Previous explicit tree: what the user wrote on first apply
    let prev_explicit = carina_core::explicit::ExplicitFields::Struct {
        children: std::collections::HashMap::from([
            (
                "policies".to_string(),
                carina_core::explicit::ExplicitFields::Leaf,
            ),
            (
                "role_name".to_string(),
                carina_core::explicit::ExplicitFields::Leaf,
            ),
        ]),
    };

    let schema = schema.validate().expect("IAM schema must validate");
    let d = diff(
        &ResolvedResource::new(resource),
        &current,
        &saved,
        Some(&prev_explicit),
        Some(schema.as_schema()),
        &carina_core::differ::exact_comparison_context_for_tests(),
    );

    assert!(
        matches!(d, carina_core::differ::Diff::NoChange(_)),
        "Expected no change, but got: {:?}",
        d
    );
}

#[test]
fn block_unique_name_attribute_state_roundtrip() {
    // Verify that block_name attributes (saved under canonical name in state)
    // roundtrip correctly through state save/load, meaning the saved_attrs
    // returned by build_saved_attrs have the correct canonical key.
    //
    // This covers the ec2_ipam case (operating_region -> operating_regions)
    // from issue #1499.

    let mut schemas = SchemaRegistry::new();
    let schema = PatternResourceSchema::new("ec2.ipam")
        .attribute(
            PatternAttributeSchema::optional(
                "operating_regions",
                PatternAttributeType::unordered_list(PatternAttributeType::struct_(
                    "IpamOperatingRegion".to_string(),
                    vec![
                        PatternStructField::required("region_name", PatternAttributeType::string())
                            .build(),
                    ],
                )),
            )
            .with_block_name("operating_region")
            .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("description", PatternAttributeType::string()).build(),
        );
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    // Resource with resolved block name
    let mut resource = Resource::with_provider("awscc", "ec2.ipam", "test-ipam", None);
    resource.set_attr(
        "operating_regions".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![(
                    "region_name".to_string(),
                    Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
                )]
                .into_iter()
                .collect(),
            ),
        )])),
    );
    resource.set_attr(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("test IPAM".into())),
    );

    let sorted_resources = vec![resolved(resource)];

    // Simulate provider state with carried-over operating_regions
    let mut applied_attrs = HashMap::new();
    applied_attrs.insert(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("test IPAM".into())),
    );
    applied_attrs.insert(
        "operating_regions".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(
                vec![(
                    "region_name".to_string(),
                    Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
                )]
                .into_iter()
                .collect(),
            ),
        )])),
    );
    let applied_state = State::existing(sorted_resources[0].id.clone(), applied_attrs)
        .with_identifier("ipam-12345");
    let mut applied_states = HashMap::new();
    applied_states.insert(
        sorted_resources[0].id.clone(),
        complete_create(applied_state),
    );

    let state = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied_states,
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    )
    .unwrap();

    // Verify state contains operating_regions
    let saved_rs = state
        .find_resource("awscc", "ec2.ipam", "test-ipam")
        .expect("resource should exist");
    assert!(
        saved_rs
            .persisted_attributes()
            .expect("saved resource has applied attributes")
            .contains_key("operating_regions"),
        "state should contain 'operating_regions'"
    );
    let carina_core::explicit::ExplicitFields::Struct {
        children: explicit_children,
    } = &saved_rs.explicit
    else {
        panic!(
            "saved_rs.explicit must be Struct, got: {:?}",
            saved_rs.explicit
        );
    };
    assert!(
        explicit_children.contains_key("operating_regions"),
        "explicit children should contain 'operating_regions'"
    );

    // Verify roundtrip through saved_attrs
    let saved_attrs = state.build_saved_attrs();
    let id = carina_core::resource::ResourceId::with_provider_identity(
        "awscc",
        "ec2.ipam",
        "test-ipam",
        None,
    );
    let attrs = saved_attrs.get(&id).unwrap();
    let operating_regions = attrs
        .get("operating_regions")
        .expect("should have operating_regions");

    // Verify the value structure is preserved
    if let Value::Concrete(ConcreteValue::List(items)) = operating_regions {
        assert_eq!(items.len(), 1);
        if let Value::Concrete(ConcreteValue::Map(map)) = &items[0] {
            assert_eq!(
                map.get("region_name"),
                Some(&Value::Concrete(ConcreteValue::String(
                    "ap-northeast-1".into()
                )))
            );
        } else {
            panic!("Expected Map in list, got {:?}", items[0]);
        }
    } else {
        panic!("Expected List, got {:?}", operating_regions);
    }
}

/// Move + Update — same shape as the Replace case but the Update
/// effect changes a non-create-only attribute. The post-Update
/// attribute value must survive Phase 2's Move cleanup.
#[test]
fn move_plus_update_keeps_post_update_attributes() {
    use carina_core::effect::Effect;
    use carina_state::{ResourceState, StateFile};

    let mut schemas = SchemaRegistry::new();
    let schema = PatternResourceSchema::new("ec2.Tag")
        .attribute(
            PatternAttributeSchema::optional("key", PatternAttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            PatternAttributeSchema::optional("value", PatternAttributeType::string()).build(),
        );
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let new_id = ResourceId::with_provider_identity("awscc", "ec2.Tag", "tag_new", None);
    let mut resource = Resource::with_provider("awscc", "ec2.Tag", "tag_new", None);
    resource.set_attr(
        "key".to_string(),
        Value::Concrete(ConcreteValue::String("env".into())),
    );
    resource.set_attr(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String("prod".into())),
    );
    let sorted_resources = vec![resolved(resource)];

    let mut applied_attrs = HashMap::new();
    applied_attrs.insert(
        "key".to_string(),
        Value::Concrete(ConcreteValue::String("env".into())),
    );
    applied_attrs.insert(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String("prod".into())),
    );
    let applied = State::existing(new_id.clone(), applied_attrs).with_identifier("tag-abc");
    let mut applied_states = HashMap::new();
    applied_states.insert(new_id.clone(), complete_update(applied, &["value"], &[]));

    // Old state row carries the pre-Update value.
    let old_row = ResourceState::try_applied("ec2.Tag", "tag_old", "awscc", "tag-abc")
        .expect("fixture identifier is non-empty")
        .with_attribute("key", serde_json::Value::String("env".to_string()))
        .with_attribute("value", serde_json::Value::String("staging".to_string()));
    let mut state_file = StateFile::default();
    state_file
        .upsert_resource(old_row)
        .expect("test state setup must be valid");

    let mut plan = Plan::new();
    let from_id = ResourceId::with_provider_identity("awscc", "ec2.Tag", "tag_old", None);
    plan.add(Effect::Update {
        identifier: crate::fixture_applied_identifier("tag-abc"),
        from: Box::new(State::existing(from_id.clone(), HashMap::new()).with_identifier("tag-abc")),
        to: sorted_resources[0].clone(),
        changed_attributes: vec!["value".to_string()],
    });
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(from_id),
        to: carina_core::resource::ResolvedResourceId::new(new_id.clone()),
        origin: None,
    });

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file,
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    )
    .expect("writeback should succeed");

    assert!(
        saved.find_resource("awscc", "ec2.Tag", "tag_old").is_none(),
        "old row must be removed by Move cleanup",
    );
    let new_row = saved
        .find_resource("awscc", "ec2.Tag", "tag_new")
        .expect("new row must exist");
    assert_eq!(
        new_row
            .persisted_attributes()
            .expect("new row has applied attributes")
            .get("value"),
        carina_core::value::json_to_dsl_value(&serde_json::Value::String("prod".to_string())),
        "Move must not overwrite Update's post-apply value",
    );
}

/// Pure-rename `moved {}` block — no Create/Update/Replace on the
/// `to` address. `materialize_moved_states` transferred the `from`
/// State into `managed_current[to]` before writeback runs, so Phase 1
/// must pick up the row from `managed_current` and the new address
/// must end up with the carried-over attributes.
#[test]
fn move_alone_carries_attributes_via_managed_current() {
    use carina_core::effect::Effect;
    use carina_state::{ResourceState, StateFile};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket_name", PatternAttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let new_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "bucket_new", None);
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "bucket_new", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    );
    let sorted_resources = vec![resolved(resource)];

    // managed_current already carries the migrated row at the new id.
    let mut current_attrs = HashMap::new();
    current_attrs.insert(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    );
    let current = State::existing(new_id.clone(), current_attrs).with_identifier("my-bucket");
    let mut managed_current = HashMap::new();
    managed_current.insert(new_id.clone(), Current::from_state(current));

    // State file still has the pre-move address — that's what the
    // Move's `from` cleanup targets.
    let from_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "bucket_old", None);
    let old_row = ResourceState::try_applied("s3.Bucket", "bucket_old", "awscc", "my-bucket")
        .expect("fixture identifier is non-empty")
        .with_attribute("bucket_name", serde_json::Value::String("my-bucket".into()));
    let mut state_file = StateFile::default();
    state_file
        .upsert_resource(old_row)
        .expect("test state setup must be valid");

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(from_id),
        to: carina_core::resource::ResolvedResourceId::new(new_id.clone()),
        origin: None,
    });

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file,
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &managed_current,
            applied_states: &HashMap::new(),
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    )
    .expect("writeback should succeed");

    assert!(
        saved
            .find_resource("awscc", "s3.Bucket", "bucket_old")
            .is_none(),
        "old row must be removed",
    );
    let new_row = saved
        .find_resource("awscc", "s3.Bucket", "bucket_new")
        .expect("new row must exist");
    assert_eq!(
        new_row.identifier().map(|value| value.as_str()),
        Some("my-bucket"),
        "identifier carried over via managed_current",
    );
    assert_eq!(
        new_row
            .persisted_attributes()
            .expect("new row has applied attributes")
            .get("bucket_name"),
        carina_core::value::json_to_dsl_value(&serde_json::Value::String("my-bucket".to_string(),)),
    );
}

/// A `Move` whose `from` is not present in state and whose `to` has
/// no Phase-1 source must be a no-op. This is the "moved block left
/// behind after the move already happened" case that carina#3167's
/// reporter saw — the block is harmless until the user deletes it.
#[test]
fn move_with_absent_from_is_no_op() {
    use carina_core::effect::Effect;
    use carina_state::StateFile;

    let schemas = SchemaRegistry::new();
    let from_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "stale_from", None);
    let to_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "stale_to", None);

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(from_id.clone()),
        to: carina_core::resource::ResolvedResourceId::new(to_id.clone()),
        origin: None,
    });

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::default(),
            sorted_resources: &[],
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &HashMap::new(),
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    )
    .expect("writeback should succeed");

    assert!(
        saved.resources().is_empty(),
        "no-op move leaves state empty"
    );
}

/// `failed_refreshes` must skip both Upsert and Cleanup: the
/// pre-existing row stays untouched because we don't know whether
/// the live resource still exists.
#[test]
fn failed_refresh_preserves_existing_row() {
    use carina_state::{ResourceState, StateFile};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket_name", PatternAttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "stuck", None);
    let resource = Resource::with_provider("awscc", "s3.Bucket", "stuck", None);
    let sorted_resources = vec![resolved(resource)];

    let mut failed_refreshes = HashSet::new();
    failed_refreshes.insert(id.clone());

    let existing = ResourceState::try_applied("s3.Bucket", "stuck", "awscc", "preserved-id")
        .expect("fixture identifier is non-empty")
        .with_attribute(
            "bucket_name",
            serde_json::Value::String("preserved".to_string()),
        );
    let mut state_file = StateFile::default();
    state_file
        .upsert_resource(existing)
        .expect("test state setup must be valid");

    let saved = build_state_after_apply(
        ApplyStateSave {
            state_file,
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &HashMap::new(),
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        },
        None,
    )
    .expect("writeback should succeed");

    let row = saved
        .find_resource("awscc", "s3.Bucket", "stuck")
        .expect("row must be preserved");
    assert_eq!(
        row.identifier().map(|value| value.as_str()),
        Some("preserved-id")
    );
}

/// A `Move` whose `from` collides with a desired resource at the
/// same address is a `WritebackConflict::UpsertCleanupOverlap` — the
/// apply pipeline upstream of writeback is supposed to prevent this
/// (a `moved` block's `from` should never be in `sorted_resources`),
/// so surface it as a validation error rather than silently dropping
/// the desired row.
#[test]
fn move_from_overlapping_desired_resource_errors() {
    use carina_core::effect::Effect;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket_name", PatternAttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "collision", None);
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "collision", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    let sorted_resources = vec![resolved(resource.clone())];

    let mut applied = HashMap::new();
    applied.insert(
        id.clone(),
        complete_create(State::existing(id.clone(), HashMap::new()).with_identifier("x")),
    );

    let to_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "elsewhere", None);
    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(id.clone()),
        to: carina_core::resource::ResolvedResourceId::new(to_id),
        origin: None,
    });

    let result = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    );

    let err = result.expect_err("overlap must surface as a writeback error");
    let msg = err.to_string();
    assert!(
        msg.contains("upsert") && msg.contains("cleanup"),
        "error must name the overlap class, got: {msg}",
    );
}

/// `Effect::Remove` whose `id` is still in `sorted_resources` (user
/// wrote both `removed { from = X }` and X still in DSL) must
/// surface the same `UpsertCleanupOverlap` as the Move overlap case.
/// Locks in that the structural invariant covers every Phase-2
/// cleanup-emitting effect, not just Move.
#[test]
fn remove_overlapping_desired_resource_errors() {
    use carina_core::effect::Effect;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket_name", PatternAttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "collision", None);
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "collision", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    let sorted_resources = vec![resolved(resource)];

    let mut applied = HashMap::new();
    applied.insert(
        id.clone(),
        complete_create(State::existing(id.clone(), HashMap::new()).with_identifier("x")),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Remove {
        id: carina_core::resource::ResolvedResourceId::new(id.clone()),
    });

    let result = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    );

    let err = result.expect_err("overlap must surface as a writeback error");
    assert!(
        err.to_string().contains("upsert") && err.to_string().contains("cleanup"),
        "error must name the overlap class, got: {err}",
    );
}

/// A self-move (`Effect::Move { from: X, to: X }`) where the
/// resource is in `sorted_resources` must error. Phase 1 upserts the
/// row, Phase 2 then tries to clean it up at the same id — surfacing
/// the upstream planner bug rather than silently deleting the row.
#[test]
fn self_move_overlapping_desired_resource_errors() {
    use carina_core::effect::Effect;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (PatternResourceSchema::new("s3.Bucket").attribute(
            PatternAttributeSchema::optional("bucket_name", PatternAttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "self", None);
    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "self", None);
    resource.set_attr(
        "bucket_name".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    let sorted_resources = vec![resolved(resource)];

    let mut applied = HashMap::new();
    applied.insert(
        id.clone(),
        complete_create(State::existing(id.clone(), HashMap::new()).with_identifier("x")),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(id.clone()),
        to: carina_core::resource::ResolvedResourceId::new(id.clone()),
        origin: None,
    });

    let result = build_state_after_apply(
        ApplyStateSave {
            state_file: StateFile::new(),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &HashMap::new(),
            applied_states: &applied,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        },
        None,
    );

    let err = result.expect_err("self-move must surface as a writeback error");
    assert!(
        err.to_string().contains("upsert") && err.to_string().contains("cleanup"),
        "error must name the overlap class, got: {err}",
    );
}

#[test]
fn format_duration_sub_second() {
    let d = Duration::from_millis(500);
    assert_eq!(format_duration(d), "0.5s");
}

#[test]
fn format_duration_seconds() {
    let d = Duration::from_secs_f64(3.25);
    assert_eq!(format_duration(d), "3.2s");
}

#[test]
fn format_duration_minutes() {
    let d = Duration::from_secs_f64(65.3);
    assert_eq!(format_duration(d), "1m 5.3s");
}

#[test]
fn format_duration_zero() {
    let d = Duration::from_secs(0);
    assert_eq!(format_duration(d), "0.0s");
}

#[test]
fn resolve_exports_resolves_cross_file_resource_refs() {
    use carina_core::parser::ExpandedInferredExportParam as ExportParameter;
    use carina_core::resource::Value;

    // Build a state file with a resource that has a binding and attributes
    let state = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 1,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": [
                {
                    "resource_type": "organizations.account",
                    "name": "registry-prod",
                    "identifier": "459524413166",
                    "provider": "awscc",
                    "binding": "registry_prod",
                    "attributes": {
                        "account_id": "459524413166",
                        "account_name": "registry-prod"
                    }
                }
            ]
        });
        state_file_from_json(json)
    };

    // Export param references registry_prod.account_id using the
    // parser-produced ResourceRef shape.
    let export_params = vec![ExportParameter {
        name: "account_id".to_string(),
        static_type: fixture_unchecked_export_static_type(),
        value: Some(Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        )),
    }];

    // Mirror production callers: the resource is in sorted_resources
    // with a binding; provider-returned attributes flow in via
    // `managed_current` derived from `state.resources`.
    let mut registry_prod =
        Resource::with_provider("awscc", "organizations.account", "registry-prod", None);
    registry_prod.binding = Some("registry_prod".to_string());
    let sorted_resources = vec![registry_prod];

    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &std::collections::HashMap::new(),
            &DataSourceValues::empty(),
            &state,
        );
    let exports = resolve_and_materialize_exports_for_test(ResolveExportValuesPostApplyInput {
        export_params: &checked_exports(&export_params),
        sorted_resources: &sorted_resources,
        pre_resolve_compositions: &[],
        post_apply_states: &post_apply_states,
        schemas: &carina_core::schema::SchemaRegistry::new(),
        wait_aliases: &[],
        provider_context: &carina_core::parser::LoadedProviderContext::empty_for_test(),
    })
    .unwrap()
    .into_parts()
    .0;

    assert_eq!(
        exports
            .get("account_id")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::Value::String("459524413166".to_string())),
        "resolve_export_values_post_apply should resolve ResourceRef values to actual values. Got: {:?}",
        exports
    );
}

#[test]
fn resolve_exports_resolves_module_call_attribute_via_composition() {
    // #2479: writeback used to build bindings from `state.resources`
    // only. A composition resource (synthesised by module-call expansion to
    // expose `attributes { role_arn = role.arn }`) carries no provider
    // identity and never lands in `state.resources`, so an export
    // referencing `<module_call>.<attr>` failed with
    // `unresolved reference <call>.<attr>`.
    use carina_core::parser::ExpandedInferredExportParam as ExportParameter;
    use carina_core::resource::{AccessPath, DeferredValue, LiveComposition, Value};

    let state = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 1,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": [
                {
                    "resource_type": "iam.Role",
                    "name": "github_actions_carina.role",
                    "identifier": "github-actions-carina",
                    "provider": "awscc",
                    "binding": "github_actions_carina.role",
                    "attributes": {
                        "arn": "arn:aws:iam::123456789012:role/github-actions-carina"
                    }
                }
            ]
        });
        state_file_from_json(json)
    };

    let mut role_resource =
        Resource::with_provider("awscc", "iam.Role", "github_actions_carina.role", None);
    role_resource.binding = Some("github_actions_carina.role".to_string());

    // Virtual resource as `expand_module_call` produces it: binding is
    // the module-call alias, and each attribute is a ResourceRef into
    // an expanded sub-resource. carina#3181: compositions are their own type.
    let mut virt_attrs: indexmap::IndexMap<
        String,
        carina_core::resource::LiveCompositionAttribute,
    > = indexmap::IndexMap::new();
    virt_attrs.insert(
        "role_arn".to_string(),
        carina_core::resource::LiveCompositionAttribute::from_value(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("github_actions_carina.role", "arn"),
            }),
            None,
        ),
    );
    let composition = LiveComposition {
        id: carina_core::resource::ResolvedResourceId::new(
            carina_core::resource::ResourceId::with_identity("_virtual", "github_actions_carina"),
        ),
        signature: carina_core::resource::LiveCompositionSignature {
            arguments: indexmap::IndexMap::new(),
            attributes: virt_attrs,
            pending_constraints: Vec::new(),
        },
        binding: Some("github_actions_carina".to_string()),
        dependency_bindings: std::collections::BTreeSet::new(),
        module_name: "github_module".to_string(),
        instance: "github_actions_carina".to_string(),
        call_path: crate::test_composition_call_path(),
        quoted_string_attrs: std::collections::HashSet::new(),
    };
    let sorted_resources = vec![role_resource];
    let pre_resolve_compositions = vec![composition];

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        static_type: fixture_unchecked_export_static_type(),
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("github_actions_carina", "role_arn"),
        })),
    }];

    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &std::collections::HashMap::new(),
            &DataSourceValues::empty(),
            &state,
        );
    let exports = resolve_and_materialize_exports_for_test(ResolveExportValuesPostApplyInput {
        export_params: &checked_exports(&export_params),
        sorted_resources: &sorted_resources,
        pre_resolve_compositions: &pre_resolve_compositions,
        post_apply_states: &post_apply_states,
        schemas: &carina_core::schema::SchemaRegistry::new(),
        wait_aliases: &[],
        provider_context: &carina_core::parser::LoadedProviderContext::empty_for_test(),
    })
    .unwrap()
    .into_parts()
    .0;

    assert_eq!(
        exports
            .get("role_arn")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::Value::String(
            "arn:aws:iam::123456789012:role/github-actions-carina".to_string()
        )),
        "module-call attribute export must resolve via composition binding + provider state, got: {:?}",
        exports
    );
}

#[test]
fn resolve_exports_resolves_chained_module_call_attribute_via_two_compositions() {
    // #2479 follow-up: a module-call binding whose attribute itself
    // points at *another* module-call binding's attribute (e.g.
    // `${outer_module.public_role_arn}` where the outer module's
    // `attributes { public_role_arn = inner_module.role_arn }` exposes
    // an inner module-call binding's attribute). Two `Virtual` hops
    // through `ResourceRef` recursion before bottoming out at the real
    // role's `arn` from state. Pins the resolver's transitive walk so a
    // regression that broke after a single hop would surface.
    use carina_core::parser::ExpandedInferredExportParam as ExportParameter;
    use carina_core::resource::{AccessPath, DeferredValue, LiveComposition, Value};

    let state = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 1,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": [
                {
                    "resource_type": "iam.Role",
                    "name": "outer.inner.role",
                    "identifier": "github-actions-carina",
                    "provider": "awscc",
                    "binding": "outer.inner.role",
                    "attributes": {
                        "arn": "arn:aws:iam::123456789012:role/chained"
                    }
                }
            ]
        });
        state_file_from_json(json)
    };

    let mut role_resource = Resource::with_provider("awscc", "iam.Role", "outer.inner.role", None);
    role_resource.binding = Some("outer.inner.role".to_string());

    // carina#3181: compositions are a distinct typestate.
    let make_virtual =
        |id_name: &'static str, binding: &str, attr: &str, ref_b: &str, ref_a: &str| {
            let mut attributes: indexmap::IndexMap<
                String,
                carina_core::resource::LiveCompositionAttribute,
            > = indexmap::IndexMap::new();
            attributes.insert(
                attr.to_string(),
                carina_core::resource::LiveCompositionAttribute::from_value(
                    Value::Deferred(DeferredValue::ResourceRef {
                        path: AccessPath::new(ref_b, ref_a),
                    }),
                    None,
                ),
            );
            LiveComposition {
                id: carina_core::resource::ResolvedResourceId::new(
                    carina_core::resource::ResourceId::with_identity("_virtual", id_name),
                ),
                signature: carina_core::resource::LiveCompositionSignature {
                    arguments: indexmap::IndexMap::new(),
                    attributes,
                    pending_constraints: Vec::new(),
                },
                binding: Some(binding.to_string()),
                dependency_bindings: std::collections::BTreeSet::new(),
                module_name: "mod".to_string(),
                instance: binding.to_string(),
                call_path: crate::test_composition_call_path(),
                quoted_string_attrs: std::collections::HashSet::new(),
            }
        };
    let inner_virtual = make_virtual(
        "outer.inner",
        "outer.inner",
        "role_arn",
        "outer.inner.role",
        "arn",
    );
    let outer_virtual = make_virtual(
        "outer",
        "outer",
        "public_role_arn",
        "outer.inner",
        "role_arn",
    );

    let sorted_resources = vec![role_resource];

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        static_type: fixture_unchecked_export_static_type(),
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("outer", "public_role_arn"),
        })),
    }];

    let pre_resolve_compositions = vec![inner_virtual, outer_virtual];
    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &std::collections::HashMap::new(),
            &DataSourceValues::empty(),
            &state,
        );
    let exports = resolve_and_materialize_exports_for_test(ResolveExportValuesPostApplyInput {
        export_params: &checked_exports(&export_params),
        sorted_resources: &sorted_resources,
        pre_resolve_compositions: &pre_resolve_compositions,
        post_apply_states: &post_apply_states,
        schemas: &carina_core::schema::SchemaRegistry::new(),
        wait_aliases: &[],
        provider_context: &carina_core::parser::LoadedProviderContext::empty_for_test(),
    })
    .unwrap()
    .into_parts()
    .0;

    assert_eq!(
        exports
            .get("role_arn")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::Value::String(
            "arn:aws:iam::123456789012:role/chained".to_string()
        )),
        "two-hop module-call chain must resolve through both compositions, got: {:?}",
        exports
    );
}

#[test]
fn resolve_exports_picks_post_apply_role_arn_after_replace_3169() {
    // #3169 root-cause regression test (carina-rs/infra PR #64 drift).
    //
    // **The bug** (pre-#3177): the apply path's head-of-pipeline
    // call to `resolve_refs_with_state_and_remote(&mut
    // resources_for_plan, &pre_apply_managed_states, …)` collapses
    // every `ResourceRef` — including the ones inside a
    // `LiveComposition`'s `attributes` — into the pre-apply
    // concrete value (here: OLD_ARN). The composition's `role_arn`
    // attribute thus becomes a frozen string snapshot of the
    // pre-apply state. Later in `finalize_apply`, the resource
    // graph is sent into `resolve_export_values_post_apply`, which feeds the
    // composition's stale `role_arn` into `state.exports` — even
    // though the writeback wrote the new ARN into
    // `state.resources[role].arn`. So `state.exports.role_arn` and
    // `state.resources[role].arn` disagree, with exports holding
    // the old ARN.
    //
    // **The fix** (#3177): `apply/mod.rs` snapshots every composition
    // *before* the head-of-pipeline `ResourceRef` collapse — that
    // snapshot still carries the authored `ref role.arn`. It
    // threads that pre-resolve snapshot into `finalize_apply` →
    // `resolve_export_values_post_apply`, which uses it (instead of the mutated
    // `sorted_resources`) to build a fresh post-apply
    // `ResolvedBindings` view via the typestate-split API
    // (`from_managed_with_state` + `resolve_live_composition_refs_post_apply`
    // + `layer_live_compositions_post_apply`). Exports then resolve against the
    // post-apply view.
    //
    // **The test** mirrors that production pipeline: it (1)
    // constructs the same managed + composition graph, (2) takes a
    // pre-resolve snapshot of the composition, (3) runs the head-of-
    // pipeline resolver against a *pre-apply* managed-current map
    // (OLD_ARN), then (4) feeds the pre-resolve snapshot and the
    // resolved working slice into `resolve_export_values_post_apply` along with a
    // *post-apply* `StateFile` (NEW_ARN). The assertion checks the
    // export ends up at NEW_ARN.
    //
    // Without the #3177 fix, `resolve_export_values_post_apply` would consult the
    // mutated composition's stale concrete attribute and return
    // OLD_ARN. With the fix in place, the pre-resolve snapshot
    // kicks in and re-resolves against the post-apply state.
    use carina_core::parser::ExpandedInferredExportParam as ExportParameter;
    use carina_core::resolver::resolve_managed_refs_with_state_and_remote;
    use carina_core::resource::{
        AccessPath, ConcreteValue, DeferredValue, LiveComposition, ResourceId,
        State as ResourceState, Value,
    };
    use std::collections::HashMap;

    let pre_apply_arn = "arn:aws:iam::123456789012:role/role-OLD";
    let post_apply_arn = "arn:aws:iam::123456789012:role/role-NEW";

    // Step (a): post-apply `StateFile` — what `state.resources`
    // looks like after the writeback wrote the new ARN. This is
    // what `resolve_export_values_post_apply` sees as its `state` argument.
    let post_apply_state_file = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 2,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": [
                {
                    "resource_type": "iam.Role",
                    "name": "carina_role",
                    "identifier": "carina-role-NEW",
                    "provider": "awscc",
                    "binding": "role",
                    "attributes": { "arn": post_apply_arn }
                }
            ]
        });
        state_file_from_json(json)
    };

    // Step (b): pre-apply managed state — what the head-of-
    // pipeline resolver sees. Holds the OLD ARN, so the pre-apply
    // pass freezes composition `role_arn` to OLD_ARN.
    let pre_apply_managed_states: HashMap<ResourceId, ResourceState> = {
        let id = ResourceId::with_provider_identity("awscc", "iam.Role", "carina_role", None);
        let mut attrs: HashMap<String, Value> = HashMap::new();
        attrs.insert(
            "arn".to_string(),
            Value::Concrete(ConcreteValue::String(pre_apply_arn.to_string().into())),
        );
        let mut m = HashMap::new();
        m.insert(id.clone(), ResourceState::existing(id, attrs));
        m
    };

    // Build the authored resource graph: a managed `role` (DSL
    // does not inline `arn` — provider returns it) and a composition
    // module-call binding that references `role.arn`. carina#3181:
    // compositions are a distinct typestate, untouched by the managed
    // head-of-pipeline resolver — they keep their authored
    // `ResourceRef`s, which is exactly the pre-resolve snapshot the
    // #3177 fix needs.
    let mut role_managed = Resource::with_provider("awscc", "iam.Role", "carina_role", None);
    role_managed.binding = Some("role".to_string());

    let mut virt_attrs: indexmap::IndexMap<
        String,
        carina_core::resource::LiveCompositionAttribute,
    > = indexmap::IndexMap::new();
    virt_attrs.insert(
        "role_arn".to_string(),
        carina_core::resource::LiveCompositionAttribute::from_value(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("role", "arn"),
            }),
            None,
        ),
    );
    let composition = LiveComposition {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "_virtual",
            "carina_module",
        )),
        signature: carina_core::resource::LiveCompositionSignature {
            arguments: indexmap::IndexMap::new(),
            attributes: virt_attrs,
            pending_constraints: Vec::new(),
        },
        binding: Some("carina_module".to_string()),
        dependency_bindings: std::collections::BTreeSet::new(),
        module_name: "carina_module".to_string(),
        instance: "carina_module".to_string(),
        call_path: crate::test_composition_call_path(),
        quoted_string_attrs: std::collections::HashSet::new(),
    };

    let mut sorted_resources = vec![role_managed];

    // Step (c): pre-resolve snapshot of the composition — carries the
    // authored `ref role.arn`.
    let pre_resolve_compositions: Vec<LiveComposition> = vec![composition];

    // Step (d): head-of-pipeline resolver — runs over the managed
    // slice only. compositions are not part of it, so the pre-resolve
    // snapshot above is preserved verbatim.
    let bindings = carina_core::binding_index::ResolvedBindings::pre_apply_live(
        carina_core::binding_index::LivePreApplyInputs {
            managed: &sorted_resources.clone(),
            compositions: &[],
            managed_current: &crate::plan_input_current_from_states(
                pre_apply_managed_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &sorted_resources,
            ),
            data_source_values: &carina_core::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        },
    );
    resolve_managed_refs_with_state_and_remote(&mut sorted_resources, &bindings).unwrap();

    let export_params = vec![ExportParameter {
        name: "role_arn".to_string(),
        static_type: fixture_unchecked_export_static_type(),
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("carina_module", "role_arn"),
        })),
    }];

    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &std::collections::HashMap::new(),
            &DataSourceValues::empty(),
            &post_apply_state_file,
        );
    let exports = resolve_and_materialize_exports_for_test(ResolveExportValuesPostApplyInput {
        export_params: &checked_exports(&export_params),
        sorted_resources: &sorted_resources,
        pre_resolve_compositions: &pre_resolve_compositions,
        post_apply_states: &post_apply_states,
        schemas: &carina_core::schema::SchemaRegistry::new(),
        wait_aliases: &[],
        provider_context: &carina_core::parser::LoadedProviderContext::empty_for_test(),
    })
    .unwrap()
    .into_parts()
    .0;

    assert_eq!(
        exports
            .get("role_arn")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&serde_json::Value::String(post_apply_arn.to_string())),
        "exports.role_arn must be the POST-apply ARN ({post_apply_arn}) — \
         the value carried by state.resources, NOT a pre-apply snapshot. \
         Got: {exports:?}",
    );
}

#[test]
fn resolve_exports_resolves_data_source_attribute_after_apply_3266() {
    // carina#3266 root-cause regression test.
    //
    // The bug: `resolve_export_values_post_apply` builds `post_apply_states` from
    // `state.resources` only. Managed resources are persisted there,
    // but data sources are not — they are queried each run and their
    // results live in `data_source_states` during execution before being
    // discarded at writeback. So when an export references a
    // data-source attribute (e.g. `exports { x = ds.arns }`), the
    // post-apply binding view layered onto `pre_apply` finds no
    // attributes for the data source (#3265's `layer_data_source_bindings`
    // merge fires only when `data_source_states[ds.id]` carries the read
    // result), the ResourceRef resolves to nothing, and the resolver
    // silently keeps the prior literal in `state.exports`.
    //
    // The fix: carry the split `managed_current` and `data_source_states` maps
    // through `finalize_apply` into `resolve_export_values_post_apply` so the
    // transient read results stay visible across the exports-resolution
    // boundary. `layer_data_source_bindings` then merges the read attrs and
    // `ds.arns` resolves to the read value.
    //
    // Without the fix, `resolve_export_values_post_apply` returns an empty map (no
    // entry for the export) instead of the NEW arn — exports stays
    // at the pre-apply literal in production.
    use carina_core::parser::ExpandedInferredExportParam as ExportParameter;
    use carina_core::resource::{
        AccessPath, ConcreteValue, DataSource, DeferredValue, ResourceId, State as ResourceState,
        Value,
    };
    use std::collections::HashMap;

    let new_arn = "arn:aws:iam::412038850359:role/aws-reserved/sso.amazonaws.com/ap-northeast-1/AWSReservedSSO_AdministratorAccess_ed2ecac126d82b94";

    // Post-apply state file: no managed resource entries needed; the
    // export's only input is a data source, which is never persisted
    // to `state.resources`.
    let post_apply_state_file = {
        let json = serde_json::json!({
            "version": 5,
            "serial": 2,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": []
        });
        state_file_from_json(json)
    };

    // The authored data source: `let admin_access_roles = read aws.iam.Roles { ... }`.
    let ds_id = ResourceId::with_provider_identity("aws", "iam.Roles", "admin_access_roles", None);
    let mut ds = DataSource::with_provider("aws", "iam.Roles", "admin_access_roles", None);
    ds.binding = Some("admin_access_roles".to_string());
    ds.attributes.insert(
        "path_prefix".to_string(),
        Value::Concrete(ConcreteValue::String(
            "/aws-reserved/sso.amazonaws.com/".into(),
        )),
    );
    let data_sources = vec![ds];

    // The provider-read result that `read_data_source_with_retry`
    // writes into `data_source_states[ds.id]` during apply execution.
    let data_source_states: HashMap<ResourceId, ResourceState> = {
        let mut attrs: HashMap<String, Value> = HashMap::new();
        attrs.insert(
            "arns".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String(new_arn.to_string().into()),
            )])),
        );
        let mut m = HashMap::new();
        m.insert(ds_id.clone(), ResourceState::existing(ds_id, attrs));
        m
    };

    // The export: `exports { admin_access_role_arns = admin_access_roles.arns }`.
    let export_params = vec![ExportParameter {
        name: "admin_access_role_arns".to_string(),
        static_type: fixture_unchecked_export_static_type(),
        value: Some(Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("admin_access_roles", "arns"),
        })),
    }];

    let data_source_values = DataSourceValues::from_data_sources(&data_sources, |data_source| {
        DataSourceValue::read(data_source_states[&data_source.id].clone())
    });
    let post_apply_states =
        crate::commands::shared::state_writeback::PostApplyStates::from_current_and_state(
            &HashMap::new(),
            &data_source_values,
            &post_apply_state_file,
        );
    let exports = resolve_and_materialize_exports_for_test(ResolveExportValuesPostApplyInput {
        export_params: &checked_exports(&export_params),
        sorted_resources: &[],
        pre_resolve_compositions: &[],
        post_apply_states: &post_apply_states,
        schemas: &carina_core::schema::SchemaRegistry::new(),
        wait_aliases: &[],
        provider_context: &carina_core::parser::LoadedProviderContext::empty_for_test(),
    })
    .unwrap()
    .into_parts()
    .0;

    let expected_json = serde_json::json!([new_arn]);
    assert_eq!(
        exports
            .get("admin_access_role_arns")
            .map(carina_core::value::PersistedExportValue::as_json),
        Some(&expected_json),
        "data-source-derived export must resolve via data_source_states read result, got: {exports:?}",
    );
}

#[test]
fn emit_newline_on_interrupt_writes_newline_when_interrupted() {
    let mut buf: Vec<u8> = Vec::new();
    let result: Result<String, AppError> = Err(AppError::Interrupted);
    emit_newline_on_interrupt(&mut buf, &result);
    assert_eq!(buf, b"\n");
}

#[test]
fn emit_newline_on_interrupt_writes_nothing_on_ok() {
    let mut buf: Vec<u8> = Vec::new();
    let result: Result<String, AppError> = Ok("yes".to_string());
    emit_newline_on_interrupt(&mut buf, &result);
    assert!(buf.is_empty());
}

#[test]
fn emit_newline_on_interrupt_writes_nothing_on_other_error() {
    let mut buf: Vec<u8> = Vec::new();
    let result: Result<String, AppError> = Err(AppError::Config("boom".to_string()));
    emit_newline_on_interrupt(&mut buf, &result);
    assert!(buf.is_empty());
}

struct NeverReady;

impl tokio::io::AsyncRead for NeverReady {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Pending
    }
}

#[tokio::test]
async fn confirm_apply_returns_confirmed_on_yes() {
    let input = &b"yes\n"[..];
    let cancel = carina_core::shutdown::ShutdownToken::running();
    let outcome = confirm_apply(input, cancel, false).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Confirmed);
}

#[tokio::test]
async fn confirm_apply_returns_cancelled_on_no() {
    let input = &b"no\n"[..];
    let cancel = carina_core::shutdown::ShutdownToken::running();
    let outcome = confirm_apply(input, cancel, false).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Cancelled);
}

#[tokio::test]
async fn confirm_apply_returns_cancelled_on_empty_input() {
    let input = &b"\n"[..];
    let cancel = carina_core::shutdown::ShutdownToken::running();
    let outcome = confirm_apply(input, cancel, false).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Cancelled);
}

#[tokio::test]
async fn confirm_apply_auto_approve_skips_read() {
    // Reader would hang forever; auto_approve must short-circuit without reading.
    let input = tokio::io::BufReader::new(tokio::io::empty());
    let cancel = carina_core::shutdown::ShutdownToken::running();
    let outcome = confirm_apply(input, cancel, true).await.unwrap();
    assert_eq!(outcome, ApplyConfirmation::Confirmed);
}

#[tokio::test]
async fn confirm_apply_returns_interrupted_when_cancel_fires_after_subscription() {
    let reader = tokio::io::BufReader::new(NeverReady);
    let (trigger, cancel) = carina_core::shutdown::testing::shutdown_channel();
    let waiting = tokio::spawn(confirm_apply(reader, cancel.clone(), false));
    tokio::task::yield_now().await;
    trigger.request_graceful_shutdown();
    let err = waiting.await.unwrap().unwrap_err();
    assert!(matches!(err, AppError::Interrupted));
}

#[tokio::test]
async fn confirm_apply_returns_interrupted_when_cancel_token_fires() {
    let (trigger, token) = carina_core::shutdown::testing::shutdown_channel();
    trigger.request_graceful_shutdown();
    let reader = tokio::io::BufReader::new(NeverReady);
    let err = confirm_apply(reader, token, false).await.unwrap_err();
    assert!(matches!(err, AppError::Interrupted));
}

// carina#3132 PR-2: the apply path expands deferred-for loops
// *post-refresh* via the *same* `crate::wiring::expand_same_config_deferred_for`
// the plan path uses (PR-1). The function itself is exhaustively
// unit-tested in `wiring::tests::expand_same_config_deferred_for_tests`;
// these tests pin the **apply-side contract**: `run_apply_locked`
// depends on that exact shared function (plan/apply parity —
// MEMORY "unit-test path ≠ apply path"), and the carina#3132 real
// registry shape (chained `opt.resource_record.name`, resolvable since
// carina#3136) materializes + resolves identically on the apply side.
mod apply_deferred_for_parity {
    use carina_core::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };

    use carina_core::binding_index::WaitAliasSpec;
    use carina_core::data_source_value::DataSourceValues;
    use carina_core::parser::{ProviderContext, parse_file};
    use carina_core::resource::{ConcreteValue, ResourceId, State, Value};
    use carina_core::schema::SchemaRegistry;
    use std::collections::HashMap;

    fn comparison_schemas() -> SchemaRegistry {
        let certificate = PatternResourceSchema::new("acm.Certificate")
            .attribute(
                PatternAttributeSchema::optional("domain_name", PatternAttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::provider_populated(
                    "domain_validation_options",
                    PatternAttributeType::list(PatternAttributeType::struct_(
                        "DomainValidation",
                        vec![
                            PatternStructField::provider_populated(
                                "resource_record",
                                PatternAttributeType::struct_(
                                    "ResourceRecord",
                                    vec![
                                        PatternStructField::provider_populated(
                                            "name",
                                            PatternAttributeType::string(),
                                        )
                                        .build(),
                                        PatternStructField::provider_populated(
                                            "value",
                                            PatternAttributeType::string(),
                                        )
                                        .build(),
                                    ],
                                ),
                            )
                            .build(),
                        ],
                    )),
                )
                .build(),
            )
            .validate()
            .expect("comparison schema must be valid");
        let mut schemas = SchemaRegistry::new();
        schemas.insert("aws", certificate);
        schemas
    }

    fn dvo_state(
        parsed: &carina_core::parser::ModuleResolvedParsedFile,
    ) -> HashMap<ResourceId, State> {
        let cert = parsed
            .resources
            .iter()
            .find(|r| r.binding.as_deref() == Some("cert"))
            .expect("parsed cert resource");
        let mut rr = indexmap::IndexMap::new();
        rr.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.r.example.com".into())),
        );
        rr.insert(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("_a1.acm-validations.aws.".into())),
        );
        let mut entry = indexmap::IndexMap::new();
        entry.insert(
            "resource_record".to_string(),
            Value::Concrete(ConcreteValue::Map(rr)),
        );
        let mut attrs = HashMap::new();
        attrs.insert(
            "domain_name".to_string(),
            Value::Concrete(ConcreteValue::String("r.example.com".into())),
        );
        attrs.insert(
            "domain_validation_options".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(entry),
            )])),
        );
        let mut states = HashMap::new();
        states.insert(
            cert.id.clone(),
            State::existing(cert.id.clone(), attrs).with_identifier("cert-id"),
        );
        states
    }

    /// The apply path's exact call: same-config `let cert` read
    /// iterable with a chained loop-var body, fed a post-refresh
    /// managed-current map. Asserts the apply side materializes AND
    /// resolves the chained ref — parity with the plan path's
    /// `chained_loop_var_field_access_resolves_post_expansion`.
    #[test]
    fn apply_post_refresh_expansion_resolves_registry_shape() {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet {
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    resource_records = [opt.resource_record.value]
                }
            }
        "#;
        let parsed = parse_file(
            src,
            std::path::Path::new("apply_deferred_for.crn"),
            &ProviderContext::default(),
        )
        .expect("parse");
        let parsed = carina_core::module_resolver::resolve_modules_providerless(
            parsed,
            std::path::Path::new("."),
        )
        .expect("cross the deferred-body resolution seam");
        let states = dvo_state(&parsed);
        let parsed = parsed
            .into_expanded()
            .expect("deferred-for fixture must contain only loop slots");
        let sorted = carina_core::deps::sort_resources_by_dependencies(&parsed.resources).unwrap();
        let schemas = comparison_schemas();

        // This is the exact function `run_apply_locked` calls
        // post-refresh (carina#3132 PR-2). Reaching it through the
        // apply module's `crate::wiring::` path pins the dependency.
        let out = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states),
            &DataSourceValues::empty(),
            &schemas,
            &carina_core::provider::RawSavedAttrs::default().lift(&schemas),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        let record_sets: Vec<_> = out
            .sorted_resources
            .iter()
            .filter(|r| r.id.resource_type.contains("RecordSet"))
            .collect();
        assert_eq!(
            record_sets.len(),
            1,
            "apply materializes one RecordSet per domain_validation_options entry"
        );
        assert_eq!(
            record_sets[0].get_attr("name"),
            Some(&Value::Concrete(ConcreteValue::String(
                "_a1.r.example.com".into()
            ))),
            "apply resolves the chained loop-var ref identically to plan \
             (carina#3132 PR-3 registry shape)"
        );
        assert!(
            out.residual_deferred_for.is_empty(),
            "resolved loop leaves no residual on the apply path"
        );
        assert_eq!(
            out.new_child_ids.len(),
            1,
            "the materialized RecordSet is reported for the apply-side \
             targeted child-refresh"
        );
    }

    /// No refreshed cert state ⇒ the loop is carried as an apply-time
    /// re-expansion target (no mis-expansion), matching the plan path's
    /// unresolvable-iterable behavior.
    #[test]
    fn apply_unresolvable_iterable_stays_deferred() {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            for (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet { name = opt.resource_record.name }
            }
        "#;
        let parsed = parse_file(
            src,
            std::path::Path::new("apply_deferred_for.crn"),
            &ProviderContext::default(),
        )
        .expect("parse");
        let parsed = parsed
            .into_expanded()
            .expect("deferred-for fixture must contain only loop slots");
        let sorted = carina_core::deps::sort_resources_by_dependencies(&parsed.resources).unwrap();
        let empty: HashMap<ResourceId, State> = HashMap::new();

        let out = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(empty),
            &DataSourceValues::empty(),
            &carina_core::schema::SchemaRegistry::new(),
            &carina_core::provider::RawSavedAttrs::default()
                .lift(&carina_core::schema::SchemaRegistry::new()),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        assert!(
            out.sorted_resources
                .iter()
                .all(|r| !r.id.resource_type.contains("RecordSet")),
            "no RecordSet materializes without a resolvable iterable on apply"
        );
        assert_eq!(out.deferred_create_targets.len(), 1);
        assert!(out.residual_deferred_for.is_empty());
        assert!(out.new_child_ids.is_empty());
    }

    /// carina#3141 apply-side contract: `run_apply_locked` refreshes
    /// exactly `refreshable_child_ids`, the same typed field the plan
    /// path consumes. Reaching `expand_same_config_deferred_for` through
    /// the apply module's `crate::wiring::` path pins that the apply
    /// path's moved-exclusion is the *identical* computation as the plan
    /// path's — a divergence would be a compile error against
    /// `DeferredForExpansion`, not a silent parity bug
    /// (MEMORY "unit-test path ≠ apply path"). Parity twin of
    /// `wiring::tests::...::moved_target_child_is_excluded_from_refreshable_set`.
    fn assert_apply_moved_target_child_excluded_from_refreshable_set(loop_target: &str) {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "r.example.com"
                validation_method = "DNS"
            }

            LOOP_TARGETfor (_, opt) in cert.domain_validation_options {
                aws.route53.RecordSet {
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    resource_records = [opt.resource_record.value]
                }
            }
        "#
        .replacen("LOOP_TARGET", loop_target, 1);
        let parsed = parse_file(
            &src,
            std::path::Path::new("apply_deferred_for.crn"),
            &ProviderContext::default(),
        )
        .expect("parse");
        let parsed = carina_core::module_resolver::resolve_modules_providerless(
            parsed,
            std::path::Path::new("."),
        )
        .expect("cross the deferred-body resolution seam");
        let states = dvo_state(&parsed);
        let parsed = parsed
            .into_expanded()
            .expect("deferred-for fixture must contain only loop slots");
        let sorted = carina_core::deps::sort_resources_by_dependencies(&parsed.resources).unwrap();
        let mut schemas = carina_core::schema::SchemaRegistry::new();
        schemas.insert(
            "aws",
            PatternResourceSchema::new("route53.RecordSet")
                .attribute(
                    PatternAttributeSchema::optional("name", PatternAttributeType::string())
                        .create_only()
                        .build(),
                )
                .validate()
                .expect("record-set schema fixture must be valid"),
        );

        // No moved targets: the expanded child is refreshable.
        let baseline = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states.clone()),
            &DataSourceValues::empty(),
            &schemas,
            &carina_core::provider::RawSavedAttrs::default().lift(&schemas),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        )
        .expect("expand");
        assert_eq!(baseline.new_child_ids.len(), 1);
        let child_id = baseline
            .new_child_ids
            .iter()
            .next()
            .expect("materialized child")
            .clone();
        assert!(
            baseline.refreshable_child_ids.contains(&child_id),
            "apply: a non-moved expanded child must still be refreshed"
        );
        let child = baseline
            .sorted_resources
            .iter()
            .find(|resource| resource.id == child_id)
            .expect("expanded child remains in the sorted resources");
        if loop_target.is_empty() {
            assert!(child.binding.is_none());
            assert!(child.loop_provenance.display_origin().is_some());
        } else {
            assert_eq!(child.binding.as_deref(), Some("records[0]"));
        }

        // Declare it a `moved` `to`: it must drop out of the refreshable
        // set so `run_apply_locked` does not clobber the migrated state.
        let mut moved_targets = std::collections::HashSet::new();
        moved_targets.insert(child_id.clone());
        let out = crate::wiring::expand_same_config_deferred_for(
            &parsed,
            &sorted,
            &crate::managed_current_from_states(states),
            &DataSourceValues::empty(),
            &schemas,
            &carina_core::provider::RawSavedAttrs::default().lift(&schemas),
            &HashMap::new(),
            &HashMap::new(),
            &[] as &[WaitAliasSpec],
            &moved_targets,
            &std::collections::HashSet::new(),
        )
        .expect("expand");

        assert_eq!(
            out.new_child_ids, baseline.new_child_ids,
            "apply: moved-exclusion must not change what materialized"
        );
        assert!(
            !out.refreshable_child_ids.contains(&child_id),
            "apply: a `moved` target child must be excluded from \
             refreshable_child_ids (carina#3141 parity with plan path)"
        );
    }

    #[test]
    fn apply_moved_target_child_excluded_from_refreshable_set() {
        assert_apply_moved_target_child_excluded_from_refreshable_set("let records = ");
    }

    #[test]
    fn apply_unbound_moved_target_child_excluded_from_refreshable_set() {
        assert_apply_moved_target_child_excluded_from_refreshable_set("");
    }
}

#[cfg(test)]
mod saved_plan_version_tests {

    //! Saved plan formats have no backward-compatibility path: re-running
    //! `carina plan` is the supported migration. Version 36 adds typed
    //! deferred-for templates and stage-specific placeholder types. Version 35
    //! stores tri-state prevent-destroy directives and the approved state-only
    //! directive change set. Version 34 adds Int-only integer arithmetic.
    //! Version 33 adds typed keyed-list release effects. Version 32 adds mandatory
    //! provider-instance reference scopes and the create-outcome-unknown marker
    //! for provider protocol v13. Version 31 persists authored ignore paths;
    //! version 30 was never emitted. Version 29 persists anonymous loop
    //! provenance, typed deferred targets, and Move origins (carina#4109).
    //! Version 26 persists exact provider artifact identities (carina#4045);
    //! version 25 persists total data-source outcomes (carina#3993); version 24
    //! removed the wait target schema kind (carina#3916), and version 23 added
    //! the mandatory provider schema fingerprint snapshot (carina#3983).
    use carina_core::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema,
    };

    use std::path::Path;

    use tempfile::TempDir;

    use super::{
        ApplyTarget, PlanFileHeader, saved_plan_replan_project_dir, saved_provider_configs,
    };

    #[tokio::test]
    async fn format_28_saved_plan_is_rejected_before_deferred_payload_decode() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("format-28.plan.json");
        let old_plan = serde_json::json!({
            "version": 28,
            "timestamp": "2026-10-29T00:00:00Z",
            "source_path": "infra",
            "plan": {
                "effects": [{
                    "DeferredCreate": {
                        "template": {
                            "binding_name": "legacy-format-28-payload"
                        }
                    }
                }]
            }
        });
        std::fs::write(&plan_path, serde_json::to_vec(&old_plan).unwrap())
            .expect("write format-28 plan");

        let error = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(1).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await
        .expect_err("format 28 must be rejected before its old deferred payload is decoded");

        let crate::error::AppError::UnsupportedPlanVersion {
            found, expected, ..
        } = error
        else {
            panic!("expected typed unsupported plan version error")
        };
        assert_eq!(found, 28);
        assert_eq!(expected, crate::commands::plan::PlanFile::CURRENT_VERSION);
    }

    #[tokio::test]
    async fn version_25_plan_is_rejected_after_provider_artifacts_become_mandatory() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("reviewed-plan.json");
        let old_plan = serde_json::json!({
            "version": 25,
            "timestamp": "2026-10-28T00:00:00Z",
            "source_path": "infra",
        });
        std::fs::write(&plan_path, serde_json::to_vec(&old_plan).unwrap()).expect("write old plan");

        let error = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(1).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await
        .expect_err("format 25 must be rejected before its body is decoded");

        let crate::error::AppError::UnsupportedPlanVersion {
            found, expected, ..
        } = error
        else {
            panic!("expected typed unsupported plan version error")
        };
        assert_eq!(
            (found, expected),
            (25, crate::commands::plan::PlanFile::CURRENT_VERSION),
        );
    }

    #[tokio::test]
    async fn previous_emitted_and_older_plan_versions_are_rejected_before_payload_decoding() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        assert_eq!(
            crate::commands::plan::PlanFile::CURRENT_VERSION,
            36,
            "carina#4094 uses saved-plan format 36 after carina#4150 format 35",
        );
        let previous = 35;

        for older_version in [previous, 34, 33, 32] {
            let plan_path = dir.path().join(format!("format-{older_version}-plan.json"));
            let old_plan = serde_json::json!({
                "version": older_version,
                "timestamp": "2026-10-29T00:00:00Z",
                "source_path": "infra",
                "compositions": "intentionally invalid payload",
            });
            std::fs::write(&plan_path, serde_json::to_vec(&old_plan).unwrap())
                .expect("write older plan");

            let error = crate::commands::apply::run_apply_from_plan(
                &user_dirs,
                &plan_path,
                true,
                false,
                std::num::NonZeroUsize::new(1).unwrap(),
                false,
                &carina_core::parser::ProviderContext::default(),
                carina_core::shutdown::ShutdownToken::running(),
            )
            .await
            .expect_err("an older emitted format must be rejected before decoding its payload");

            let crate::error::AppError::UnsupportedPlanVersion {
                found, expected, ..
            } = error
            else {
                panic!("expected typed unsupported plan version error")
            };
            assert_eq!(found, older_version);
            assert_eq!(expected, crate::commands::plan::PlanFile::CURRENT_VERSION);
        }
    }

    #[tokio::test]
    async fn version_33_plan_is_rejected_with_regeneration_guidance() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("format-33-plan.json");
        let old_plan = serde_json::json!({
            "version": 33,
            "timestamp": "2026-10-30T00:00:00Z",
            "source_path": "infra",
            "compositions": "intentionally invalid payload",
        });
        std::fs::write(&plan_path, serde_json::to_vec(&old_plan).unwrap())
            .expect("write format 33 plan");

        let error = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(1).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await
        .expect_err("format 33 must be regenerated before its payload is decoded");

        let crate::error::AppError::UnsupportedPlanVersion {
            found,
            expected,
            replan_command,
            ..
        } = &error
        else {
            panic!("expected typed unsupported plan version error")
        };
        assert_eq!(*found, 33);
        assert_eq!(*expected, crate::commands::plan::PlanFile::CURRENT_VERSION);
        assert!(replan_command.is_some());
        assert!(error.to_string().contains("Re-run `carina plan"));
    }

    #[tokio::test]
    async fn version_22_plan_without_schema_fingerprints_is_rejected_with_replan_command() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("reviewed-plan.json");
        let old_plan = serde_json::json!({
            "version": 22,
            "carina_version": "0.4.0",
            "timestamp": "2026-10-27T00:00:00Z",
            "source_path": "infra",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": [] },
            "sorted_resources": [],
            "unresolved_resources": [],
            "compositions": [],
            "data_sources": [],
            "data_source_origins": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(&plan_path, serde_json::to_vec(&old_plan).unwrap()).expect("write old plan");

        let error = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(1).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await
        .expect_err("a plan predating schema fingerprints must be rejected");

        let crate::error::AppError::UnsupportedPlanVersion {
            found,
            expected,
            replan_command,
            ..
        } = &error
        else {
            panic!("expected typed version error, got {error:?}");
        };
        assert_eq!(
            (*found, *expected),
            (22, crate::commands::plan::PlanFile::CURRENT_VERSION),
        );
        let expected_replan = format!("carina plan --out {} infra", plan_path.display());
        assert_eq!(replan_command.as_deref(), Some(expected_replan.as_str()));
        assert!(
            error
                .to_string()
                .contains(&format!("Re-run `{expected_replan}`"))
        );
    }

    #[tokio::test]
    async fn future_saved_plan_without_source_path_is_rejected_by_version() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("future-plan");
        std::fs::write(&plan_path, r#"{"version":99,"timestamp":"x"}"#).expect("write future plan");

        let result = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        let error = result.expect_err("future saved plan must be rejected by version");
        let crate::error::AppError::UnsupportedPlanVersion {
            path: error_path,
            found,
            expected,
            replan_command,
        } = &error
        else {
            panic!("expected typed unsupported-version error, got: {error}");
        };
        assert_eq!(error_path, &plan_path);
        assert_eq!(*found, 99);
        assert_eq!(*expected, crate::commands::plan::PlanFile::CURRENT_VERSION);
        assert_eq!(replan_command, &None);

        let message = error.to_string();
        assert!(
            message.contains("Unsupported plan file version: 99"),
            "version must be checked without requiring source_path: {message}",
        );
        assert!(
            message
                .contains("Re-create the plan with plan --out from the project that produced it"),
            "missing source_path must use source-agnostic guidance: {message}",
        );
        assert!(
            !message.contains("is not a Carina plan file") && !message.contains("`carina plan"),
            "future plans without source_path must not be misclassified or use a guessed command: {message}",
        );
    }

    #[tokio::test]
    async fn current_saved_plan_without_source_path_is_corrupt_without_command() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("current-plan");
        let malformed = serde_json::json!({
            "version": crate::commands::plan::PlanFile::CURRENT_VERSION,
            "timestamp": "x",
            "foo": 1,
        });
        std::fs::write(
            &plan_path,
            serde_json::to_vec(&malformed).expect("serialize malformed plan"),
        )
        .expect("write malformed plan");

        let result = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        let message = result
            .expect_err("malformed current plan must be reported as corrupt")
            .to_string();
        assert!(
            message.contains("The plan file may be corrupted")
                && message.contains(
                    "Re-create the plan with plan --out from the project that produced it"
                ),
            "missing source_path must use corruption guidance without a command: {message}",
        );
        assert!(
            !message.contains("is not a Carina plan file") && !message.contains("`carina plan"),
            "a recognized current-version plan must not be misclassified or use a guessed command: {message}",
        );
    }

    /// A `version: 3` saved plan must be rejected by
    /// `run_apply_from_plan` with a message that names the expected
    /// version and points the user at re-running `plan`.
    ///
    /// The test writes a minimal v3-shaped JSON to disk and asserts
    /// the rejection. The plan body is deliberately tiny — the
    /// version gate runs first, before any field is consumed, so a
    /// stub `effects: []` / resource arrays are enough.
    #[tokio::test]
    async fn version_3_saved_plan_is_rejected() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        // A v3 plan: everything required by the pre-#3248 PlanFile
        // shape, no `compositions` field.
        let v3 = serde_json::json!({
            "version": 3,
            "carina_version": "0.4.0",
            "timestamp": "2026-05-24T00:00:00Z",
            "source_path": "infra/nested/main.crn",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": [] },
            "sorted_resources": [],
            "unresolved_resources": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(&plan_path, serde_json::to_string(&v3).unwrap()).expect("write plan");

        let result = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        let err = result.expect_err("v3 saved plan must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("Unsupported plan file version: 3"),
            "error must name the rejected version, got: {msg}",
        );
        let expected = format!(
            "expected {}",
            crate::commands::plan::PlanFile::CURRENT_VERSION
        );
        assert!(
            msg.contains(&expected),
            "error must name the expected version, got: {msg}",
        );
        let expected_replan = format!(
            "Re-run `carina plan --out {} infra/nested`",
            plan_path.display()
        );
        assert!(
            msg.contains(&expected_replan),
            "version-mismatch hint must regenerate the same saved plan, got: {msg}",
        );
    }

    #[test]
    fn old_recorded_crn_source_uses_parent_directory_for_replan_hint() {
        let source_path = Path::new("infra/nested/main.crn");

        assert_eq!(
            ApplyTarget::SavedPlan {
                plan_file: Path::new("reviewed-plan.json"),
                source_dir: saved_plan_replan_project_dir(source_path),
            }
            .replan_command()
            .to_string(),
            "carina plan --out reviewed-plan.json infra/nested"
        );
    }

    #[test]
    fn current_saved_plan_replan_hint_keeps_recorded_source_path() {
        assert_eq!(
            ApplyTarget::SavedPlan {
                plan_file: Path::new("reviewed-plan.json"),
                source_dir: Path::new("infra/nested/main.crn"),
            }
            .replan_command()
            .to_string(),
            "carina plan --out reviewed-plan.json infra/nested/main.crn"
        );
    }

    #[tokio::test]
    async fn version_10_saved_plan_is_rejected_after_constraint_metadata_bump() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        let v10 = serde_json::json!({
            "version": 10,
            "carina_version": "0.4.0",
            "timestamp": "2026-07-02T00:00:00Z",
            "source_path": "infra/nested",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": [] },
            "sorted_resources": [],
            "unresolved_resources": [],
            "compositions": [],
            "data_sources": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(&plan_path, serde_json::to_string(&v10).unwrap()).expect("write plan");

        let result = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        let err = result.expect_err("v10 saved plan must be rejected after format bumps");
        let crate::error::AppError::UnsupportedPlanVersion {
            path: error_path,
            found,
            expected,
            replan_command,
        } = &err
        else {
            panic!("expected typed unsupported-version error, got: {err}");
        };
        assert_eq!(error_path, &plan_path);
        assert_eq!(*found, 10);
        assert_eq!(*expected, crate::commands::plan::PlanFile::CURRENT_VERSION);
        let expected_replan = format!("carina plan --out {} infra/nested", plan_path.display());
        assert_eq!(replan_command.as_deref(), Some(expected_replan.as_str()));

        let msg = err.to_string();
        assert!(
            msg.contains("Unsupported plan file version: 10"),
            "error must name the rejected v10 version, got: {msg}",
        );
        assert!(
            msg.contains(&format!(
                "expected {}",
                crate::commands::plan::PlanFile::CURRENT_VERSION
            )),
            "error must name the current expected version, got: {msg}",
        );
        assert!(
            msg.contains(&format!("Re-run `{expected_replan}`")),
            "error must include the path-aware re-plan command, got: {msg}",
        );
    }

    #[tokio::test]
    async fn version_13_saved_plan_is_rejected_after_orphan_ownership_change() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        let v13 = serde_json::json!({
            "version": 13,
            "carina_version": "0.4.0",
            "timestamp": "2026-10-17T00:00:00Z",
            "source_path": "infra",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": [] },
            "sorted_resources": [],
            "unresolved_resources": [],
            "compositions": [],
            "data_sources": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(&plan_path, serde_json::to_string(&v13).unwrap()).expect("write plan");

        let error = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await
        .expect_err("v13 saved plan must be rejected after the orphan ownership change");

        let crate::error::AppError::UnsupportedPlanVersion {
            path: error_path,
            found,
            expected,
            replan_command,
        } = &error
        else {
            panic!("expected typed unsupported-version error, got: {error}");
        };
        assert_eq!(error_path, &plan_path);
        assert_eq!(*found, 13);
        assert_eq!(*expected, crate::commands::plan::PlanFile::CURRENT_VERSION);
        assert_eq!(
            replan_command.as_deref(),
            Some(format!("carina plan --out {} infra", plan_path.display()).as_str())
        );
    }

    #[tokio::test]
    async fn version_15_saved_plan_is_rejected_after_wait_rhs_typing_change() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        let v15 = serde_json::json!({
            "version": 15,
            "carina_version": "0.4.0",
            "timestamp": "2026-10-18T00:00:00Z",
            "source_path": "infra",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": [] },
            "sorted_resources": [],
            "unresolved_resources": [],
            "compositions": [],
            "data_sources": [],
            "data_source_origins": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(&plan_path, serde_json::to_string(&v15).unwrap()).expect("write plan");

        let error = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await
        .expect_err("v15 saved plans must be regenerated with typed wait RHS values");

        let crate::error::AppError::UnsupportedPlanVersion {
            found,
            expected,
            replan_command,
            ..
        } = &error
        else {
            panic!("expected typed unsupported-version error, got: {error}");
        };
        assert_eq!(*found, 15);
        assert_eq!(*expected, crate::commands::plan::PlanFile::CURRENT_VERSION);
        assert_eq!(
            replan_command.as_deref(),
            Some(format!("carina plan --out {} infra", plan_path.display()).as_str())
        );
    }

    #[tokio::test]
    async fn version_18_saved_plan_is_rejected_after_abandoned_generation_shape_change() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        let v18 = serde_json::json!({
            "version": 18,
            "carina_version": "0.4.0",
            "timestamp": "2026-10-23T00:00:00Z",
            "source_path": "infra",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": [] },
            "sorted_resources": [],
            "unresolved_resources": [],
            "compositions": [],
            "data_sources": [],
            "data_source_origins": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(&plan_path, serde_json::to_string(&v18).unwrap()).expect("write plan");

        let error = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await
        .expect_err("v18 saved plans contain the old abandoned-generation shape");

        let crate::error::AppError::UnsupportedPlanVersion {
            found,
            expected,
            replan_command,
            ..
        } = &error
        else {
            panic!("expected typed unsupported-version error, got: {error}");
        };
        assert_eq!(*found, 18);
        assert_eq!(*expected, crate::commands::plan::PlanFile::CURRENT_VERSION);
        assert_eq!(
            replan_command.as_deref(),
            Some(format!("carina plan --out {} infra", plan_path.display()).as_str())
        );
    }

    #[tokio::test]
    async fn version_19_saved_plan_is_rejected_after_current_state_tagging_change() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        let v19 = serde_json::json!({
            "version": 19,
            "carina_version": "0.4.0",
            "timestamp": "2026-10-23T00:00:00Z",
            "source_path": "infra",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": [] },
            "sorted_resources": [],
            "unresolved_resources": [],
            "compositions": [],
            "data_sources": [],
            "data_source_origins": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(&plan_path, serde_json::to_string(&v19).unwrap()).expect("write plan");

        let error = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await
        .expect_err("v19 saved plans contain the untagged current-state shape");

        let crate::error::AppError::UnsupportedPlanVersion {
            path: error_path,
            found,
            expected,
            replan_command,
        } = &error
        else {
            panic!("expected typed unsupported-version error, got: {error}");
        };
        assert_eq!(error_path, &plan_path);
        assert_eq!(*found, 19);
        assert_eq!(*expected, crate::commands::plan::PlanFile::CURRENT_VERSION);
        assert_eq!(
            replan_command.as_deref(),
            Some(format!("carina plan --out {} infra", plan_path.display()).as_str())
        );
    }

    #[tokio::test]
    async fn saved_plan_decode_rejects_legacy_import_shape_by_version() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("reviewed-plan");
        let import_id = carina_core::resource::ResolvedResourceId::new(
            carina_core::resource::ResourceId::with_identity("mock.test.resource", "legacy"),
        );
        let legacy_v4 = serde_json::json!({
            "version": 4,
            "carina_version": "0.4.0",
            "timestamp": "2026-05-24T00:00:00Z",
            "source_path": "infra/nested",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": {
                "effects": [{
                    "Import": {
                        "id": import_id,
                        "identifier": "legacy-provider-id"
                    }
                }]
            },
            "sorted_resources": [],
            "unresolved_resources": [],
            "compositions": [],
            "data_sources": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(
            &plan_path,
            serde_json::to_string(&legacy_v4).expect("serialize legacy plan"),
        )
        .expect("write legacy plan");

        let result = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        let message = result
            .expect_err("legacy saved plan must be rejected by the version gate")
            .to_string();
        let current_version = crate::commands::plan::PlanFile::CURRENT_VERSION;
        assert!(
            message.contains(&format!(
                "Unsupported plan file version: 4 (expected {current_version})"
            )),
            "legacy wire shapes must reach the version gate: {message}",
        );
        assert!(
            message.contains(&format!(
                "Re-run `carina plan --out {} infra/nested`",
                plan_path.display()
            )),
            "version error must include the recorded source path: {message}",
        );
        assert!(
            !message.contains("is not a Carina plan file"),
            "a recognized older plan must not be called a non-plan file: {message}",
        );
    }

    #[tokio::test]
    async fn saved_plan_decode_reports_malformed_current_version_as_corrupt() {
        let user_dirs = carina_core::TestUserDirs::new();
        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("reviewed-plan");
        let malformed = serde_json::json!({
            "version": crate::commands::plan::PlanFile::CURRENT_VERSION,
            "carina_version": "0.4.0",
            "timestamp": "2026-07-02T00:00:00Z",
            "source_path": "infra/nested",
            "state_lineage": null,
            "state_serial": null,
            "provider_configs": [],
            "backend_config": null,
            "plan": { "effects": "not-an-array" },
            "sorted_resources": [],
            "unresolved_resources": [],
            "compositions": [],
            "data_sources": [],
            "current_states": [],
            "upstream_snapshot": {},
            "upstream_sources": [],
            "wait_bindings": [],
        });
        std::fs::write(
            &plan_path,
            serde_json::to_string(&malformed).expect("serialize malformed plan"),
        )
        .expect("write malformed plan");

        let result = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            &plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        let error = result.expect_err("malformed current-version plan must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("The plan file may be corrupted. Re-run"),
            "current-version structural errors must be reported as corruption: {message}",
        );
        assert!(
            message.contains("line") && message.contains("column"),
            "body decode errors must retain their source location: {message}",
        );
        assert!(
            message.contains(&format!(
                "`carina plan --out {} infra/nested`",
                plan_path.display()
            )),
            "corruption error must include the replan command: {message}",
        );
        assert!(
            !message.contains("is not a Carina plan file"),
            "a recognized current-version plan must not be called a non-plan file: {message}",
        );
    }

    fn replacement_plan_json() -> (TempDir, std::path::PathBuf, serde_json::Value) {
        use carina_core::provider::ProviderRouter;
        use carina_core::schema::SchemaRegistry;

        let dir = TempDir::new().expect("tempdir");
        let plan_path = dir.path().join("plan.json");
        let resource = carina_core::resource::Resource::new("ec2.Vpc", "main").with_attribute(
            "cidr_block",
            carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::String(
                "10.1.0.0/16".into(),
            )),
        );
        let id = resource.id.clone();
        let current_state = carina_core::resource::State::existing(
            id.clone(),
            std::collections::HashMap::from([(
                "cidr_block".to_string(),
                carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String("10.0.0.0/16".into()),
                ),
            )]),
        )
        .with_identifier("vpc-old");
        let current_states = std::collections::HashMap::from([(id.clone(), current_state.clone())]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "",
            (PatternResourceSchema::new("ec2.Vpc").attribute(
                PatternAttributeSchema::optional("cidr_block", PatternAttributeType::string())
                    .create_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let plan_input_states = crate::plan_input_current_from_states(
            current_states.clone(),
            &SchemaRegistry::new(),
            &[],
        );
        let resolved_resource = carina_core::resource::ResolvedResource::new(resource.clone());
        let plan = carina_core::differ::create_plan_for_test(
            std::slice::from_ref(&resolved_resource),
            &[],
            &ProviderRouter::new(),
            carina_core::differ::PlanPriorStates::new(
                &plan_input_states,
                &carina_core::differ::Orphans::new(),
            ),
            &std::collections::HashMap::new(),
            &schemas,
            &crate::empty_lifted_saved_attrs(),
            &std::collections::HashMap::new(),
            &[],
        )
        .expect("test provider hints should succeed");
        assert_eq!(plan.replace_display_info().count(), 1);
        let plan_file = crate::commands::plan::PlanFileContents {
            version: crate::commands::plan::PlanFile::CURRENT_VERSION,
            carina_version: "test".to_string(),
            timestamp: "2026-07-02T00:00:00Z".to_string(),
            source_path: "missing.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: saved_provider_configs(Vec::new()),
            backend_config: None,
            plan,
            sorted_resources: vec![resource.clone()],
            unresolved_resources: vec![resource],
            compositions: Vec::new(),
            data_sources: Vec::new(),
            data_source_origins: Vec::new(),
            data_source_values: Vec::new(),
            current_states: vec![crate::commands::plan::CurrentStateEntry::managed(
                id,
                current_state,
            )],
            upstream_snapshot: std::collections::HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        }
        .record_for_test(
            &crate::wiring::ValidatedConfig::from_factories_with_additional_schemas_for_test(
                (),
                Vec::new(),
                &[],
                schemas
                    .iter()
                    .map(|(provider, _resource_type, _kind, schema)| {
                        (provider.to_string(), schema.clone())
                    }),
            ),
            Vec::new(),
        )
        .expect("replacement plan provider evidence must be valid");
        let json = serde_json::to_value(&plan_file).expect("plan file should serialize");
        (dir, plan_path, json)
    }

    #[test]
    fn saved_plan_header_decodes_from_current_serialized_plan_file() {
        let (_dir, _plan_path, json) = replacement_plan_json();
        let serialized = serde_json::to_vec(&json).expect("real plan must serialize");

        let header: PlanFileHeader =
            serde_json::from_slice(&serialized).expect("real plan must decode as a header");

        assert_eq!(
            header.version,
            crate::commands::plan::PlanFile::CURRENT_VERSION
        );
        assert_eq!(
            header
                .source_path
                .as_ref()
                .and_then(serde_json::Value::as_str),
            json.get("source_path").and_then(serde_json::Value::as_str)
        );
    }

    async fn rejected_saved_plan_message(
        plan_path: &std::path::PathBuf,
        json: &serde_json::Value,
        expectation: &str,
    ) -> String {
        let user_dirs = carina_core::TestUserDirs::new();
        std::fs::write(plan_path, serde_json::to_string(json).unwrap()).expect("write plan");

        let result = crate::commands::apply::run_apply_from_plan(
            &user_dirs,
            plan_path,
            true,
            false,
            std::num::NonZeroUsize::new(8).unwrap(),
            false,
            &carina_core::parser::ProviderContext::default(),
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await;

        result.expect_err(expectation).to_string()
    }

    fn replace_first_resource_identity(value: &mut serde_json::Value, identity: &str) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                if map.contains_key("resource_type") && map.contains_key("identity") {
                    map.insert(
                        "identity".to_string(),
                        serde_json::Value::String(identity.to_string()),
                    );
                    return true;
                }
                map.values_mut()
                    .any(|child| replace_first_resource_identity(child, identity))
            }
            serde_json::Value::Array(items) => items
                .iter_mut()
                .any(|child| replace_first_resource_identity(child, identity)),
            _ => false,
        }
    }

    fn set_replace_display_delete_generation_to_deposed(json: &mut serde_json::Value) {
        let delete_idx = json["plan"]["replace_display"][0]["delete_idx"]
            .as_u64()
            .expect("delete_idx should be numeric") as usize;
        let delete_effect = &mut json["plan"]["effects"][delete_idx];
        let deposed = serde_json::to_value(carina_core::effect::EffectGeneration::Deposed(
            carina_core::resource::DeposedKey::new_unique(),
        ))
        .expect("deposed generation should serialize");

        if let serde_json::Value::Object(effect) = delete_effect {
            if let Some(serde_json::Value::Object(delete)) = effect.get_mut("Delete") {
                delete.insert("generation".to_string(), deposed);
                return;
            }
            if effect
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind == "Delete")
            {
                effect.insert("generation".to_string(), deposed);
                return;
            }
        }

        panic!("fixture delete effect has an unexpected serde shape: {delete_effect:#?}");
    }

    #[tokio::test]
    async fn saved_plan_with_empty_mutation_identifier_requests_replan() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        let delete_idx = json["plan"]["replace_display"][0]["delete_idx"]
            .as_u64()
            .expect("delete_idx should be numeric") as usize;
        json["plan"]["effects"][delete_idx]["Delete"]["identifier"] =
            serde_json::Value::String(String::new());

        let message = rejected_saved_plan_message(
            &plan_path,
            &json,
            "an empty saved-plan mutation identifier must be rejected",
        )
        .await;

        assert!(
            message.contains("an applied resource identifier must not be empty"),
            "the typed identifier error must survive saved-plan decoding: {message}",
        );
        assert!(
            message.contains("The plan file may be corrupted. Re-run"),
            "the decode error must include re-plan guidance: {message}",
        );
        assert!(
            message.contains(&format!("carina plan --out {}", plan_path.display())),
            "the re-plan guidance must name the saved-plan output: {message}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_out_of_range_replace_display_create_idx_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        json["plan"]["replace_display"][0]["create_idx"] = serde_json::json!(999);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "corrupt replace_display metadata must be rejected",
        )
        .await;
        assert!(
            msg.contains("Invalid saved plan replacement metadata"),
            "error must name replacement metadata, got: {msg}",
        );
        assert!(
            msg.contains("create_idx 999 is out of bounds"),
            "error must name the bad create_idx, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_out_of_range_replace_display_delete_idx_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        json["plan"]["replace_display"][0]["delete_idx"] = serde_json::json!(999);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "corrupt replace_display metadata must be rejected",
        )
        .await;
        assert!(
            msg.contains("Invalid saved plan replacement metadata"),
            "error must name replacement metadata, got: {msg}",
        );
        assert!(
            msg.contains("delete_idx 999 is out of bounds"),
            "error must name the bad delete_idx, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_replace_display_create_idx_kind_mismatch_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        let delete_idx = json["plan"]["replace_display"][0]["delete_idx"]
            .as_u64()
            .expect("delete_idx should be numeric");
        json["plan"]["replace_display"][0]["create_idx"] = serde_json::json!(delete_idx);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "create_idx kind mismatch must be rejected",
        )
        .await;
        assert!(
            msg.contains("expected Create"),
            "error must name the create_idx kind mismatch, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_replace_display_delete_idx_kind_mismatch_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        let create_idx = json["plan"]["replace_display"][0]["create_idx"]
            .as_u64()
            .expect("create_idx should be numeric");
        json["plan"]["replace_display"][0]["delete_idx"] = serde_json::json!(create_idx);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "delete_idx kind mismatch must be rejected",
        )
        .await;
        assert!(
            msg.contains("expected Delete"),
            "error must name the delete_idx kind mismatch, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_replace_display_deposed_delete_half_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        set_replace_display_delete_generation_to_deposed(&mut json);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "deposed replacement delete half must be rejected",
        )
        .await;
        assert!(
            msg.contains("expected Current Delete"),
            "error must require a current-generation delete half, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_cross_wired_replace_display_indices_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        let create_idx = json["plan"]["replace_display"][0]["create_idx"]
            .as_u64()
            .expect("create_idx should be numeric") as usize;
        let effects = json["plan"]["effects"]
            .as_array_mut()
            .expect("effects should be an array");
        let mut other_create = effects[create_idx].clone();
        assert!(
            replace_first_resource_identity(&mut other_create, "other"),
            "fixture create effect should contain a resource identity"
        );
        effects.push(other_create);
        let other_create_idx = effects.len() - 1;
        json["plan"]["replace_display"][0]["create_idx"] = serde_json::json!(other_create_idx);

        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "cross-wired replacement metadata must be rejected",
        )
        .await;
        assert!(
            msg.contains("cross-wires"),
            "error must name cross-wired replacement metadata, got: {msg}",
        );
        assert!(
            msg.contains("expected both halves to address the same resource"),
            "error must describe same-resource invariant, got: {msg}",
        );
    }

    #[tokio::test]
    async fn saved_plan_with_duplicate_replace_display_index_is_rejected() {
        let (_dir, plan_path, mut json) = replacement_plan_json();
        let first = json["plan"]["replace_display"][0].clone();
        json["plan"]["replace_display"]
            .as_array_mut()
            .expect("replace_display should be an array")
            .push(first);
        let msg = rejected_saved_plan_message(
            &plan_path,
            &json,
            "duplicate replace_display metadata must be rejected",
        )
        .await;
        assert!(
            msg.contains("Invalid saved plan replacement metadata"),
            "error must name replacement metadata, got: {msg}",
        );
        assert!(
            msg.contains("duplicates or overlaps"),
            "error must name duplicate replacement metadata, got: {msg}",
        );
    }
}
#[test]
fn apply_exit_code_prioritizes_failure_over_partial() {
    assert_eq!(apply_exit_code_for_counts(0, 0), ApplyExitCode::Success);
    assert_eq!(
        apply_exit_code_for_counts(0, 1),
        ApplyExitCode::PartialSuccess
    );
    assert_eq!(apply_exit_code_for_counts(1, 0), ApplyExitCode::Failure);
    assert_eq!(apply_exit_code_for_counts(1, 1), ApplyExitCode::Failure);
}

#[test]
fn skipped_export_does_not_downgrade_partial_success() {
    let result: ExecutionResult = ExecutionResult {
        success_count: 0,
        failure_count: 0,
        partial_count: 1,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        interrupted_count: 0,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        successfully_deleted: HashSet::new(),
        permanent_name_overrides: HashMap::new(),
        managed_current: HashMap::new(),
        data_source_states: HashMap::new(),
        bindings: Default::default(),
        failed_refreshes: HashSet::new(),
    };

    let err = finish_apply(
        &result,
        SkippedExports::from_names_for_test(&["role_arn"]),
        Duration::ZERO,
    )
    .unwrap_err();

    assert!(
        matches!(&err, AppError::PartialSuccess(_)),
        "a skipped export must not downgrade partial-success exit semantics: {err:?}",
    );
    let message = err.to_string();
    assert!(message.contains("1 partial"), "error: {message}");
    assert!(message.contains("1 export not written"), "error: {message}",);
}

#[test]
fn interrupted_apply_summary_counts_unknown_outcome() {
    let result: ExecutionResult = ExecutionResult {
        success_count: 1,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        interrupted_count: 1,
        applied_states: HashMap::new(),
        runtime_synthesized_resources: Vec::new(),
        successfully_deleted: HashSet::new(),
        permanent_name_overrides: HashMap::new(),
        managed_current: HashMap::new(),
        data_source_states: HashMap::new(),
        bindings: Default::default(),
        failed_refreshes: HashSet::new(),
    };

    let err = finish_apply(&result, SkippedExports::default(), Duration::ZERO).unwrap_err();

    assert!(matches!(&err, AppError::Config(_)));
    assert_eq!(
        err.to_string(),
        "Apply failed. 1 succeeded, 1 outcome unknown."
    );
}
