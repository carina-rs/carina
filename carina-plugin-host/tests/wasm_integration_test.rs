//! Integration tests: load MockProvider .wasm via WasmProviderFactory and perform CRUD.

use std::collections::HashMap;
use std::path::PathBuf;

use carina_core::provider::{Provider, ProviderFactory};
use carina_core::resource::{Expr, Resource, ResourceId, Value};
use carina_plugin_host::WasmProviderFactory;

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

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_factory() {
    let path = skip_if_no_wasm!();
    let factory = WasmProviderFactory::new(path)
        .await
        .expect("Failed to load WASM provider");

    assert_eq!(factory.name(), "mock");
    assert_eq!(factory.display_name(), "Mock Provider (Process)");

    // schemas() should return an empty vec for the mock provider
    let schemas = factory.schemas();
    assert!(schemas.is_empty(), "Mock provider should have no schemas");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_create_and_read() {
    let path = skip_if_no_wasm!();
    let factory = WasmProviderFactory::new(path)
        .await
        .expect("Failed to load WASM provider");
    let provider = factory.create_provider(&HashMap::new()).await;

    assert_eq!(provider.name(), "mock");

    // Read before create - should return a state with no identifier and empty attributes
    let id = ResourceId::with_provider("mock", "test.resource", "my-resource");
    let state = provider
        .read(&id, None)
        .await
        .expect("read should not error");
    assert!(state.identifier.is_none());
    assert!(state.attributes.is_empty());

    // Create a resource
    let mut resource = Resource::with_provider("mock", "test.resource", "my-resource");
    resource.attributes = HashMap::from([
        ("name".into(), Expr(Value::String("my-resource".into()))),
        ("region".into(), Expr(Value::String("us-east-1".into()))),
        ("count".into(), Expr(Value::Int(42))),
    ]);

    let created = provider
        .create(&resource)
        .await
        .expect("create should succeed");
    assert_eq!(created.identifier, Some("mock-id".into()));
    assert_eq!(
        created.attributes.get("name"),
        Some(&Value::String("my-resource".into()))
    );
    assert_eq!(
        created.attributes.get("region"),
        Some(&Value::String("us-east-1".into()))
    );
    assert_eq!(created.attributes.get("count"), Some(&Value::Int(42)));

    // Read back - should return the created state
    let read_state = provider
        .read(&id, Some("mock-id"))
        .await
        .expect("read should not error");
    assert_eq!(read_state.identifier, Some("mock-id".into()));
    assert_eq!(
        read_state.attributes.get("name"),
        Some(&Value::String("my-resource".into()))
    );
    assert_eq!(
        read_state.attributes.get("region"),
        Some(&Value::String("us-east-1".into()))
    );
    assert_eq!(read_state.attributes.get("count"), Some(&Value::Int(42)));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_update_and_delete() {
    let path = skip_if_no_wasm!();
    let factory = WasmProviderFactory::new(path)
        .await
        .expect("Failed to load WASM provider");
    let provider = factory.create_provider(&HashMap::new()).await;

    let id = ResourceId::with_provider("mock", "test.resource", "updatable");

    // Create first
    let mut resource = Resource::with_provider("mock", "test.resource", "updatable");
    resource.attributes = HashMap::from([
        ("color".into(), Expr(Value::String("red".into()))),
        ("size".into(), Expr(Value::Int(10))),
    ]);

    let created = provider
        .create(&resource)
        .await
        .expect("create should succeed");
    assert_eq!(
        created.attributes.get("color"),
        Some(&Value::String("red".into()))
    );

    // Update with new attributes
    let mut updated_resource = Resource::with_provider("mock", "test.resource", "updatable");
    updated_resource.attributes = HashMap::from([
        ("color".into(), Expr(Value::String("blue".into()))),
        ("size".into(), Expr(Value::Int(20))),
    ]);

    let updated = provider
        .update(&id, "mock-id", &created, &updated_resource)
        .await
        .expect("update should succeed");
    assert_eq!(
        updated.attributes.get("color"),
        Some(&Value::String("blue".into()))
    );
    assert_eq!(updated.attributes.get("size"), Some(&Value::Int(20)));

    // Read to verify update persisted in memory
    let read_state = provider
        .read(&id, Some("mock-id"))
        .await
        .expect("read should not error");
    assert_eq!(
        read_state.attributes.get("color"),
        Some(&Value::String("blue".into()))
    );

    // Delete
    let lifecycle = Default::default();
    provider
        .delete(&id, "mock-id", &lifecycle)
        .await
        .expect("delete should succeed");

    // Read after delete - should return empty state (no identifier, no attributes)
    let deleted_state = provider
        .read(&id, None)
        .await
        .expect("read should not error");
    assert!(deleted_state.identifier.is_none());
    assert!(deleted_state.attributes.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wasm_mock_provider_normalizer() {
    let path = skip_if_no_wasm!();
    let factory = WasmProviderFactory::new(path)
        .await
        .expect("Failed to load WASM provider");
    let normalizer = factory.create_normalizer(&HashMap::new()).await;

    // normalize_desired: mock provider returns resources unchanged
    let mut resources = vec![{
        let mut r = Resource::with_provider("mock", "test.resource", "norm-test");
        r.attributes = HashMap::from([("key".into(), Expr(Value::String("value".into())))]);
        r
    }];
    let original_attrs = resources[0].resolved_attributes();
    normalizer.normalize_desired(&mut resources);
    assert_eq!(resources[0].resolved_attributes(), original_attrs);

    // normalize_state: mock provider returns states unchanged
    let id = ResourceId::with_provider("mock", "test.resource", "norm-test");
    let attrs = HashMap::from([("key".into(), Value::String("value".into()))]);
    let state = carina_core::resource::State::existing(id.clone(), attrs.clone());
    let mut states = HashMap::from([(id.clone(), state)]);
    normalizer.normalize_state(&mut states);
    let result_state = states.values().next().unwrap();
    assert_eq!(
        result_state.attributes.get("key"),
        Some(&Value::String("value".into()))
    );
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
    let factory = WasmProviderFactory::new(path)
        .await
        .expect("Failed to load WASM provider");
    let provider = factory.create_provider(&HashMap::new()).await;

    let mut resource = Resource::with_provider("mock", "test.data_source", "example");
    resource.attributes = HashMap::from([
        (
            "identity_store_id".into(),
            Expr(Value::String("d-1234567890".into())),
        ),
        (
            "user_name".into(),
            Expr(Value::String("alice@example.com".into())),
        ),
    ]);

    let state = provider
        .read_data_source(&resource)
        .await
        .expect("read_data_source should dispatch to the plugin override");

    assert!(state.exists, "state should be marked as existing");
    assert_eq!(
        state.attributes.get("__mock_read_data_source__"),
        Some(&Value::Bool(true)),
        "sentinel attribute must be present — proves the plugin override ran"
    );
    assert_eq!(
        state.attributes.get("identity_store_id"),
        Some(&Value::String("d-1234567890".into())),
        "input attributes must cross the WASM boundary unchanged"
    );
    assert_eq!(
        state.attributes.get("user_name"),
        Some(&Value::String("alice@example.com".into())),
    );
}
