//! Regression tests for carina-rs/carina#1681 and #3688: a genuinely
//! informative basic-fallback failure must remain visible as subordinate
//! context, while the basic linker's predetermined wasi:http-missing failure
//! must not compete with the real HTTP-path cause.

use carina_plugin_host::{WasmProviderFactory, WasmProviderLoadError};

/// Minimal component that imports both wasi:http and a fake interface. The
/// HTTP-enabled binding path rejects it as an invalid provider component; the
/// basic linker is independently incapable of satisfying the wasi:http import.
const HTTP_AND_BOGUS_COMPONENT_WAT: &str = r#"
(component
  (type $i (instance))
  (import "wasi:http/types@0.2.9" (instance (type $i)))
  (import "bogus:fake-3688/iface@99.0.0" (instance (type $i)))
)
"#;

/// The original carina#1681 scenario: neither linker provides this genuinely
/// missing, non-wasi:http interface. Because the basic attempt is not
/// structurally predetermined, its failure must remain subordinate context.
const NON_WASI_BOGUS_COMPONENT_WAT: &str = r#"
(component
  (type $i (instance))
  (import "bogus:fake-1681/iface@99.0.0" (instance (type $i)))
)
"#;

async fn render_instantiation_error(wat: &str) -> String {
    let dir = tempfile::tempdir().expect("temp dir");
    let wasm_path = dir.path().join("fake.wasm");
    std::fs::write(&wasm_path, wat).expect("write wat file");

    let cache_dir = tempfile::tempdir().expect("cache dir");
    let err = match WasmProviderFactory::new_with_cache_dir(wasm_path, cache_dir.path()).await {
        Ok(_) => panic!("fake component should have failed to instantiate"),
        Err(e) => e,
    };
    match err {
        WasmProviderLoadError::Instantiation(error) => error.to_string(),
        other => panic!("expected typed instantiation error, got: {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn instantiation_failure_is_typed_and_demotes_basic_fallback() {
    let rendered = render_instantiation_error(HTTP_AND_BOGUS_COMPONENT_WAT).await;

    assert!(
        rendered.starts_with("Failed to instantiate WASM component (HTTP)"),
        "HTTP failure did not lead error: {rendered}"
    );
    assert!(
        rendered.contains("no exported instance named `carina:provider/provider@0.1.0`"),
        "full HTTP cause chain missing real linker detail: {rendered}"
    );
    assert!(rendered.contains("Component wasi:http imports: wasi:http/types@0.2.9"));
    assert!(rendered.contains("Host wasi:http version: 0.2.6"));
    assert!(rendered.contains("These wasi:http versions are compatible"));
    assert!(rendered.contains("Basic fallback diagnostic omitted"));
    assert!(!rendered.contains("basic fallback also failed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn non_wasi_import_preserves_informative_basic_fallback_as_subordinate_context() {
    let rendered = render_instantiation_error(NON_WASI_BOGUS_COMPONENT_WAT).await;
    let missing_provider_export = "no exported instance named `carina:provider/provider@0.1.0`";
    let subordinate_marker = "Subordinate basic fallback attempt also failed (not the root cause):";

    assert!(
        rendered.starts_with("Failed to instantiate WASM component (HTTP)"),
        "HTTP failure did not lead error: {rendered}"
    );
    assert!(rendered.contains("Component wasi:http imports: none detected"));
    let subordinate_at = rendered
        .find(subordinate_marker)
        .unwrap_or_else(|| panic!("basic failure was not subordinate context: {rendered}"));
    assert!(
        rendered[subordinate_at + subordinate_marker.len()..].contains(missing_provider_export),
        "the subordinate basic failure chain was dropped: {rendered}"
    );
    assert!(
        rendered.contains("bogus:fake-1681/iface@99.0.0"),
        "genuinely missing non-wasi:http import was dropped: {rendered}"
    );
    assert!(!rendered.contains("Basic fallback diagnostic omitted"));
}
