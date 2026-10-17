//! Integration tests: load MockProvider .wasm via WasmProviderFactory and perform CRUD.

use std::collections::HashMap;
use std::path::PathBuf;

use carina_core::effect::PlanOp;
use carina_core::provider::{
    CreateRequest, DeleteRequest, Provider, ProviderFactory, ProviderReadyConfig, RawSavedAttrs,
    ReadRequest, SavedAttrs, UpdateRequest,
};
use carina_core::resource::{
    ConcreteValue, DataSource, DeferredValue, Resource, ResourceId, State, UnknownReason, Value,
};
use carina_plugin_host::WasmProviderFactory;

fn lifted_saved_attrs(
    entries: impl IntoIterator<Item = (ResourceId, HashMap<String, Value>)>,
) -> carina_core::provider::LiftedSavedAttrs {
    RawSavedAttrs::from_decoded_values_for_test(entries.into_iter().collect())
        .lift(&carina_core::schema::SchemaRegistry::new())
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

async fn update_request_for_test(
    resource: Resource,
    from: State,
    changed_attributes: &[String],
) -> UpdateRequest {
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
/// `WasmProviderFactory::new()`'s default `~/.carina/cache`, concurrent
/// precompile runs race on the same `.cwasm` path and one test can observe
/// a partially-written file (`"failed to load code for …"`). Each test gets
/// its own cache dir via this helper to eliminate that race.
async fn load_factory(wasm: &std::path::Path) -> (WasmProviderFactory, tempfile::TempDir) {
    let cache = tempfile::tempdir().expect("Failed to create cache tempdir");
    let factory = WasmProviderFactory::from_file_cached(wasm, cache.path())
        .await
        .expect("Failed to load WASM provider");
    (factory, cache)
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

    // schemas() should return an empty vec for the mock provider
    let schemas = factory.schemas();
    assert!(schemas.is_empty(), "Mock provider should have no schemas");
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
        .read(&id, None, ReadRequest)
        .await
        .expect("read should not error");
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

    let created = provider
        .create(&id, create_request_for_test(resource.clone()).await)
        .await
        .expect("create should succeed")
        .into_state_for_writeback();
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
        .read(&id, Some("mock-id"), ReadRequest)
        .await
        .expect("read should not error");
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
        .read(&id, Some("mock-id"), ReadRequest)
        .await
        .expect_err("guest output containing nested NaN must be a provider error");

    assert!(error.message().contains("WASM boundary encode error"));
    assert!(error.message().contains("non-finite float NaN"));
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
    ]);

    let created = provider
        .create(&id, create_request_for_test(resource.clone()).await)
        .await
        .expect("create should succeed")
        .into_state_for_writeback();
    assert_eq!(
        created.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("red".into())))
    );

    // Prepare the desired resource through the checked core seam. Both ops
    // become Replace because the attributes exist in `from`.
    resource.set_attr(
        "color",
        Value::Concrete(ConcreteValue::String("blue".into())),
    );
    resource.set_attr("size", Value::Concrete(ConcreteValue::Int(20)));
    let request = update_request_for_test(
        resource,
        created.clone(),
        &["color".to_string(), "size".to_string()],
    )
    .await;

    let updated = provider
        .update(&id, "mock-id", request)
        .await
        .expect("update should succeed")
        .into_state_for_writeback();
    assert_eq!(
        updated.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("blue".into())))
    );
    assert_eq!(
        updated.attributes.get("size"),
        Some(&Value::Concrete(ConcreteValue::Int(20)))
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
    assert_eq!(ops.len(), 2);
    assert_eq!(
        ops[0],
        Value::Concrete(ConcreteValue::String("replace:color".into()))
    );
    assert_eq!(
        ops[1],
        Value::Concrete(ConcreteValue::String("replace:size".into()))
    );

    // Read to verify update persisted in memory
    let read_state = provider
        .read(&id, Some("mock-id"), ReadRequest)
        .await
        .expect("read should not error");
    assert_eq!(
        read_state.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("blue".into())))
    );

    // Delete
    provider
        .delete(&id, "mock-id", DeleteRequest::default())
        .await
        .expect("delete should succeed");

    // Read after delete - should return empty state (no identifier, no attributes)
    let deleted_state = provider
        .read(&id, None, ReadRequest)
        .await
        .expect("read should not error");
    assert!(deleted_state.identifier.is_none());
    assert!(deleted_state.attributes.is_empty());
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
    normalizer.normalize_state(&mut states).await.unwrap();
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

    normalizer
        .normalize_state(&mut pending_states)
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

    normalizer
        .normalize_state(&mut rekeyed_states)
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
        normalizer
            .normalize_state(&mut dropped_states)
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
    normalizer
        .hydrate_read_state(&mut hydrate_states, saved_attrs.as_provider_saved_attrs())
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
        normalizer
            .hydrate_read_state(&mut dropped_hydrate_states, &SavedAttrs::new())
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

    normalizer.normalize_state(&mut states).await.unwrap();

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

    normalizer
        .hydrate_read_state(&mut hydrate_states, saved_attrs.as_provider_saved_attrs())
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
            .message()
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

        let result = normalizer.normalize_state(&mut states).await;

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

        let result = normalizer
            .hydrate_read_state(&mut states, saved_attrs.as_provider_saved_attrs())
            .await;

        assert!(result.is_err(), "{identity} must be rejected");
        assert_eq!(states, before, "key mismatches must be atomic");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_hydrate_read_state_receives_persisted_secret_as_legacy_marker_string() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &ready_config(&factory, &indexmap::IndexMap::new()))
        .await
        .expect("normalizer should initialize");
    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let persisted_secret: Value = serde_json::from_value(serde_json::json!({
        "RedactedSecret": { "hash": hash }
    }))
    .expect("deserialize persisted secret");
    let id = ResourceId::with_provider_identity("mock", "test.resource", "secret-state", None);
    let mut states = HashMap::from([(id.clone(), State::existing(id.clone(), HashMap::new()))]);
    let saved_attrs = lifted_saved_attrs([(
        id.clone(),
        HashMap::from([("__mock_hydrate_read_state__".to_string(), persisted_secret)]),
    )]);

    normalizer
        .hydrate_read_state(&mut states, saved_attrs.as_provider_saved_attrs())
        .await
        .expect("persisted secret must cross the legacy guest wire");

    assert!(matches!(
        states[&id].attributes.get("__mock_hydrated_read_state__"),
        Some(Value::Deferred(DeferredValue::RedactedSecret { hash: loaded }))
            if loaded.as_str() == hash
    ));
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

    let error = normalizer
        .normalize_state(&mut states)
        .await
        .expect_err("a guest normalizer trap must become a normalizer error");

    assert!(error.to_string().contains("normalize_state"));
    assert_eq!(states, before, "guest traps must be atomic");

    let create_id = ResourceId::with_provider_identity("mock", "test.resource", "after-trap", None);
    let resource = Resource::with_provider("mock", "test.resource", "after-trap", None);
    let error = provider
        .create(&create_id, create_request_for_test(resource).await)
        .await
        .expect_err("the shared provider instance must remain poisoned after a normalizer trap");
    let rendered = error.to_string();

    assert!(
        rendered.contains("provider instance unusable after trap in normalize_state"),
        "the follow-up CRUD error must name the normalizer trap: {rendered}"
    );
    assert!(
        !rendered.contains("WASM trap in create")
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
        .create(&trap_id, create_request_for_test(trap_resource).await)
        .await
        .expect_err("the mock create hook must trap");
    assert!(
        initial_error.to_string().contains("WASM trap in create"),
        "the initial error must name the trapping operation: {initial_error}"
    );

    let followup_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-create-trap", None);
    let followup_error = provider
        .read(&followup_id, None, ReadRequest)
        .await
        .expect_err("the shared instance must remain poisoned after a CRUD trap");
    let rendered = followup_error.to_string();

    assert!(
        rendered.contains("provider instance unusable after trap in create")
            && rendered.contains("wasm `unreachable` instruction executed"),
        "the follow-up error must name the original operation and typed trap cause: {rendered}"
    );
    assert!(
        !rendered.contains("WASM trap in read")
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
        .create(&error_id, create_request_for_test(error_resource).await)
        .await
        .expect_err("the mock create hook must return a provider error");
    assert!(error.to_string().contains("intentional mock create error"));

    let followup_id =
        ResourceId::with_provider_identity("mock", "test.resource", "after-create-error", None);
    let state = provider
        .read(&followup_id, None, ReadRequest)
        .await
        .expect("a provider-returned error must not poison the shared instance");
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
    assert_eq!(error.message(), "intentional mock normalize_desired error");
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

    let error = normalizer
        .normalize_state(&mut states)
        .await
        .expect_err("a guest normalize_state error must cross the structured error channel");

    assert_eq!(error.variant_name(), "internal");
    assert_eq!(error.message(), "intentional mock normalize_state error");
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
    let error = normalizer
        .hydrate_read_state(&mut hydrate_states, &SavedAttrs::new())
        .await
        .expect_err("a guest hydrate_read_state error must cross the structured error channel");
    assert_eq!(error.variant_name(), "internal");
    assert_eq!(error.message(), "intentional mock hydrate_read_state error");
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
        .read(&read_id, None, ReadRequest)
        .await
        .expect("provider-returned normalizer errors must not poison a later CRUD call");
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
    assert!(std::error::Error::source(&error).is_some());
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

    let error = normalizer
        .normalize_state(&mut states)
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

    let error = normalizer
        .hydrate_read_state(&mut states, saved_attrs.as_provider_saved_attrs())
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

    normalizer
        .hydrate_read_state(&mut states, saved_attrs.as_provider_saved_attrs())
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
