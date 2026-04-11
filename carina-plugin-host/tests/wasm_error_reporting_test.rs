//! Regression test for carina-rs/carina#1681: when a provider wasm
//! cannot instantiate under either the HTTP-enabled or the basic world,
//! the returned error must include *both* underlying causes — not just
//! the fallback's misleading "wasi:http/types not found" linker message.

use carina_plugin_host::WasmProviderFactory;

/// Minimal component that imports a fake interface which is registered
/// in neither the HTTP-enabled nor the basic wasi linker, so both
/// instantiation attempts must fail. The fake namespace is chosen to
/// not collide with any real wasi/carina import.
const FAKE_COMPONENT_WAT: &str = r#"
(component
  (type $i (instance))
  (import "bogus:fake-1681/iface@99.0.0" (instance (type $i)))
)
"#;

#[tokio::test(flavor = "multi_thread")]
async fn both_worlds_failing_error_includes_both_causes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let wasm_path = dir.path().join("fake.wasm");
    std::fs::write(&wasm_path, FAKE_COMPONENT_WAT).expect("write wat file");

    let cache_dir = tempfile::tempdir().expect("cache dir");
    let err = match WasmProviderFactory::new_with_cache_dir(wasm_path, cache_dir.path()).await {
        Ok(_) => panic!("fake component should have failed to instantiate"),
        Err(e) => e,
    };

    assert!(
        err.contains("HTTP-enabled world failed"),
        "missing HTTP-path cause in error: {err}"
    );
    assert!(
        err.contains("basic fallback also failed"),
        "missing basic-path cause in error: {err}"
    );
}
