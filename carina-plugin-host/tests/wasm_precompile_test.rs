//! Tests for WasmProviderFactory precompile cache functionality.

use std::path::PathBuf;

use carina_core::provider::ProviderFactory;
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
async fn test_precompile_cache_creation() {
    let wasm = skip_if_no_wasm!();
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwasm_path = cache_dir.path().join("carina_provider_mock.cwasm");

    // Precompile and write .cwasm
    WasmProviderFactory::precompile(&wasm, &cwasm_path).expect("precompile should succeed");

    // .cwasm file should now exist
    assert!(cwasm_path.exists(), ".cwasm file should have been created");

    // Load from precompiled and verify the factory works
    let factory = WasmProviderFactory::from_precompiled(&cwasm_path)
        .await
        .expect("from_precompiled should succeed");

    assert_eq!(factory.name(), "mock");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_from_file_cached_creates_cache() {
    let wasm = skip_if_no_wasm!();
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // First call: no cache exists, should compile and cache
    let factory = WasmProviderFactory::from_file_cached(&wasm, cache_dir.path())
        .await
        .expect("from_file_cached should succeed");

    assert_eq!(factory.name(), "mock");

    // .cwasm should have been created in cache dir
    // cache_key() includes a content hash, so look for any .cwasm file
    let cwasm_files: Vec<_> = std::fs::read_dir(cache_dir.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "cwasm"))
        .collect();
    assert_eq!(cwasm_files.len(), 1, ".cwasm cache file should be created");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_from_file_cached_uses_cache() {
    let wasm = skip_if_no_wasm!();
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // First call: compiles and caches
    let factory1 = WasmProviderFactory::from_file_cached(&wasm, cache_dir.path())
        .await
        .expect("first from_file_cached should succeed");
    assert_eq!(factory1.name(), "mock");

    // Second call: uses cache
    let factory2 = WasmProviderFactory::from_file_cached(&wasm, cache_dir.path())
        .await
        .expect("second from_file_cached (cache hit) should succeed");
    assert_eq!(factory2.name(), "mock");

    // Both factories should report correct schemas (empty for mock provider)
    assert!(factory1.schemas().is_empty());
    assert!(factory2.schemas().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_from_file_cached_recovers_from_stale_cache() {
    let wasm = skip_if_no_wasm!();
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // First, create a valid cache so we know the filename
    let factory = WasmProviderFactory::from_file_cached(&wasm, cache_dir.path())
        .await
        .expect("initial from_file_cached should succeed");
    assert_eq!(factory.name(), "mock");

    // Find the .cwasm file before dropping the factory (path only, no access).
    let cwasm_path = std::fs::read_dir(cache_dir.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|ext| ext == "cwasm"))
        .expect("should find .cwasm file")
        .path();

    // Drop the factory — and therefore its mmap of this file — before
    // overwriting. On Linux, mutating a file while a file-backed mmap of it
    // is live results in SIGBUS on any later access to the now-invalid
    // pages; macOS is more lenient, which is why this test slipped through
    // locally.
    drop(factory);

    std::fs::write(&cwasm_path, b"not a valid cwasm file")
        .expect("Failed to write stale cache file");

    // from_file_cached should detect invalid cache, recompile, and succeed
    let factory = WasmProviderFactory::from_file_cached(&wasm, cache_dir.path())
        .await
        .expect("from_file_cached should recover from stale cache");

    assert_eq!(factory.name(), "mock");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_precompiled_factory_creates_provider() {
    let wasm = skip_if_no_wasm!();
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwasm_path = cache_dir.path().join("carina_provider_mock.cwasm");

    WasmProviderFactory::precompile(&wasm, &cwasm_path).expect("precompile should succeed");

    let factory = WasmProviderFactory::from_precompiled(&cwasm_path)
        .await
        .expect("from_precompiled should succeed");

    // Verify the factory can actually create a working provider
    let provider = factory
        .create_provider(&indexmap::IndexMap::new())
        .await
        .expect("provider should init");
    assert_eq!(provider.name(), "mock");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_new_uses_default_cache() {
    let wasm = skip_if_no_wasm!();

    // Use a temporary directory as the cache location to avoid polluting the real cache
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");

    let factory = WasmProviderFactory::new_with_cache_dir(wasm.clone(), cache_dir.path())
        .await
        .expect("new_with_cache_dir should succeed");

    assert_eq!(factory.name(), "mock");

    // A .cwasm file should have been created in the cache dir
    let entries: Vec<_> = std::fs::read_dir(cache_dir.path())
        .expect("read_dir should succeed")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "cwasm"))
        .collect();

    assert_eq!(
        entries.len(),
        1,
        "Exactly one .cwasm file should be created"
    );

    // Second call should reuse the cache (file should not change)
    let cwasm_path = entries[0].path();
    let mtime1 = std::fs::metadata(&cwasm_path)
        .expect("metadata")
        .modified()
        .expect("modified");

    let factory2 = WasmProviderFactory::new_with_cache_dir(wasm, cache_dir.path())
        .await
        .expect("second new_with_cache_dir should succeed");

    assert_eq!(factory2.name(), "mock");

    let mtime2 = std::fs::metadata(&cwasm_path)
        .expect("metadata")
        .modified()
        .expect("modified");

    assert_eq!(
        mtime1, mtime2,
        "Cache file should not be rewritten on second call"
    );
}

/// Regression guard for #1978. `Component::deserialize_file` mmap's the
/// `.cwasm`. If `precompile` ever starts writing the target path in place
/// (instead of write-tempfile + atomic rename), a second precompile run on
/// the same path while an earlier factory's mmap is still live will trap
/// as SIGBUS on Linux — the same failure mode that broke #1976's test.
///
/// This test loads a factory, keeps it alive, overwrites the cache via the
/// public `precompile` API, loads a second factory from the same path, and
/// then lets both drop. If the invariant holds the whole sequence is a
/// no-op from the user's perspective; if someone breaks it, the Linux CI
/// runner crashes here.
#[tokio::test(flavor = "multi_thread")]
async fn precompile_preserves_prior_factory_mmap() {
    let wasm = skip_if_no_wasm!();
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwasm_path = cache_dir.path().join("carina_provider_mock.cwasm");

    WasmProviderFactory::precompile(&wasm, &cwasm_path).expect("initial precompile should succeed");
    let factory1 = WasmProviderFactory::from_precompiled(&cwasm_path)
        .await
        .expect("first from_precompiled should succeed");

    // Overwrite the cache file via the public API while `factory1` still
    // holds its mmap. A `precompile` implementation that writes in place
    // would pull backing pages out from under `factory1`'s Component.
    WasmProviderFactory::precompile(&wasm, &cwasm_path).expect("second precompile should succeed");

    let factory2 = WasmProviderFactory::from_precompiled(&cwasm_path)
        .await
        .expect("second from_precompiled should succeed");

    assert_eq!(factory1.name(), "mock");
    assert_eq!(factory2.name(), "mock");
}
