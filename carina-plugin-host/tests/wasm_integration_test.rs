//! Integration tests: load MockProvider .wasm via WasmProviderFactory and perform CRUD.

use std::collections::HashMap;
use std::path::PathBuf;

use carina_core::effect::PlanOp;
use carina_core::provider::{
    CreateRequest, DeleteRequest, PatchOp, PatchOpKind, Provider, ProviderFactory, ReadRequest,
    UpdatePatch, UpdateRequest,
};
use carina_core::resource::{
    ConcreteValue, DataSource, ResolvedResource, Resource, ResourceId, Value,
};
use carina_plugin_host::WasmProviderFactory;

async fn normalized_for_test(resource: Resource) -> ResolvedResource {
    let normalized = carina_core::executor::normalized::apply_desired_normalization(
        resource,
        &[],
        &carina_core::provider::NoopNormalizer,
        &[],
        &carina_core::schema::SchemaRegistry::new(),
    )
    .await;
    carina_core::executor::resolve_normalized_for_provider(normalized)
        .expect("test resource should be fully resolved")
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
        .create_provider(None, &indexmap::IndexMap::new())
        .await
        .expect("provider should init");

    assert_eq!(provider.name(), "mock");

    // Read before create - should return a state with no identifier and empty attributes
    let id = ResourceId::with_provider("mock", "test.resource", "my-resource", None);
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
        .create(
            &id,
            CreateRequest {
                resource: normalized_for_test(resource.clone()).await,
            },
        )
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
async fn test_wasm_mock_provider_update_and_delete() {
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let provider = factory
        .create_provider(None, &indexmap::IndexMap::new())
        .await
        .expect("provider should init");

    let id = ResourceId::with_provider("mock", "test.resource", "updatable", None);

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
        .create(
            &id,
            CreateRequest {
                resource: normalized_for_test(resource.clone()).await,
            },
        )
        .await
        .expect("create should succeed")
        .into_state_for_writeback();
    assert_eq!(
        created.attributes.get("color"),
        Some(&Value::Concrete(ConcreteValue::String("red".into())))
    );

    // Build an UpdatePatch describing the user's intended changes
    // (color: red→blue, size: 10→20). Both ops are Replace because
    // they exist in `from`.
    let patch = UpdatePatch {
        ops: vec![
            PatchOp {
                kind: PatchOpKind::Replace,
                key: "color".to_string(),
                value: Some(Value::Concrete(ConcreteValue::String("blue".into()))),
            },
            PatchOp {
                kind: PatchOpKind::Replace,
                key: "size".to_string(),
                value: Some(Value::Concrete(ConcreteValue::Int(20))),
            },
        ],
    };

    let updated = provider
        .update(
            &id,
            "mock-id",
            UpdateRequest {
                from: created.clone(),
                patch,
            },
        )
        .await
        .expect("update should succeed");
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
        .create_normalizer(None, &indexmap::IndexMap::new())
        .await;

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
    normalizer.normalize_desired(&mut resources).await;
    assert_eq!(resources[0].resolved_attributes(), original_attrs);

    // normalize_state: mock provider returns states unchanged
    let id = ResourceId::with_provider("mock", "test.resource", "norm-test", None);
    let attrs = HashMap::from([(
        "key".into(),
        Value::Concrete(ConcreteValue::String("value".into())),
    )]);
    let state = carina_core::resource::State::existing(id.clone(), attrs.clone());
    let mut states = HashMap::from([(id.clone(), state)]);
    normalizer.normalize_state(&mut states).await;
    let result_state = states.values().next().unwrap();
    assert_eq!(
        result_state.attributes.get("key"),
        Some(&Value::Concrete(ConcreteValue::String("value".into())))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_merge_default_tags_dispatches_through_wit() {
    // Regression test for carina-rs/carina-provider-awscc#192 and
    // carina-rs/carina-provider-aws#242. Before the WIT contract gained
    // `merge-default-tags`, the host's `WasmProviderNormalizer` had no
    // way to dispatch the call to the guest, so provider-level
    // `default_tags` silently never reached resources. The mock guest
    // echoes the host-supplied `default_tags` into a sentinel attribute;
    // its presence proves the WIT bridge round-tripped.
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &indexmap::IndexMap::new())
        .await;

    let registry = carina_core::schema::SchemaRegistry::new();
    let mut resources = vec![Resource::with_provider(
        "mock",
        "test.resource",
        "tag-test",
        None,
    )];
    let default_tags = indexmap::IndexMap::from([
        (
            "Env".to_string(),
            Value::Concrete(ConcreteValue::String("dev".to_string())),
        ),
        (
            "Owner".to_string(),
            Value::Concrete(ConcreteValue::String("platform".to_string())),
        ),
    ]);

    normalizer
        .merge_default_tags(&mut resources, &default_tags, &registry)
        .await;

    let echoed = resources[0]
        .get_attr("__mock_merged_default_tags__")
        .expect("guest's merge_default_tags must run via the WIT bridge");
    let Value::Concrete(ConcreteValue::List(items)) = echoed else {
        panic!("expected list, got {echoed:?}");
    };
    assert_eq!(items.len(), 2, "both default_tags should arrive at guest");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_merge_default_tags_empty_short_circuits() {
    // Empty `default_tags` must skip the WIT round-trip entirely; if it
    // didn't, the mock guest would still write its sentinel attribute.
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &indexmap::IndexMap::new())
        .await;

    let registry = carina_core::schema::SchemaRegistry::new();
    let mut resources = vec![Resource::with_provider(
        "mock",
        "test.resource",
        "no-tags",
        None,
    )];
    let default_tags: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();

    normalizer
        .merge_default_tags(&mut resources, &default_tags, &registry)
        .await;

    assert!(
        resources[0]
            .get_attr("__mock_merged_default_tags__")
            .is_none(),
        "empty default_tags must short-circuit before reaching the guest"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_merge_default_tags_preserves_order() {
    // Multi-resource ordering: the host zips the guest's response by
    // index, so the guest must return resources in input order.
    let path = skip_if_no_wasm!();
    let (factory, _cache) = load_factory(&path).await;
    let normalizer = factory
        .create_normalizer(None, &indexmap::IndexMap::new())
        .await;

    let registry = carina_core::schema::SchemaRegistry::new();
    let mut resources = vec![
        Resource::with_provider("mock", "test.resource", "alpha", None),
        Resource::with_provider("mock", "test.resource", "beta", None),
        Resource::with_provider("mock", "test.resource", "gamma", None),
    ];
    let default_tags = indexmap::IndexMap::from([(
        "Env".to_string(),
        Value::Concrete(ConcreteValue::String("dev".to_string())),
    )]);

    normalizer
        .merge_default_tags(&mut resources, &default_tags, &registry)
        .await;

    assert_eq!(resources[0].id.name.as_str(), "alpha");
    assert_eq!(resources[1].id.name.as_str(), "beta");
    assert_eq!(resources[2].id.name.as_str(), "gamma");
    for r in &resources {
        assert!(
            r.get_attr("__mock_merged_default_tags__").is_some(),
            "every resource should receive the merged sentinel"
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
        .create_provider(None, &indexmap::IndexMap::new())
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
        .create_provider(None, &indexmap::IndexMap::new())
        .await
        .expect("provider should init");
    let id = ResourceId::with_provider("mock", "test.resource", "example", None);

    assert_eq!(
        provider.required_permissions(&id, PlanOp::Create),
        Vec::<String>::new()
    );
}
