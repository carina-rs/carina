//! Tests for WasmProviderFactory precompile cache functionality.

use std::path::{Path, PathBuf};

use carina_core::provider::ProviderFactory;
use carina_plugin_host::{WasmProviderFactory, WasmProviderLoadError};

/// A valid component that cannot be brought up as a Carina provider because
/// neither host world can satisfy its imported interface.
const NON_INSTANTIABLE_COMPONENT_WAT: &str = r#"
(component
  (type $i (instance))
  (import "bogus:cache-regression/iface@1.0.0" (instance (type $i)))
)
"#;

struct CacheArtifactSnapshot {
    contents: Vec<u8>,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    device_and_inode: (u64, u64),
}

fn cached_component_path(cache_dir: &Path) -> PathBuf {
    std::fs::read_dir(cache_dir)
        .expect("read cache dir")
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "cwasm"))
        .expect("cold load should leave one precompiled artifact")
        .path()
}

fn snapshot_cache_artifact(cwasm_path: &Path) -> CacheArtifactSnapshot {
    let metadata = std::fs::metadata(cwasm_path).expect("stat warm cache artifact");

    #[cfg(unix)]
    let device_and_inode = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };

    CacheArtifactSnapshot {
        contents: std::fs::read(cwasm_path).expect("read warm cache artifact"),
        modified: metadata.modified().expect("cache modified time"),
        #[cfg(unix)]
        device_and_inode,
    }
}

fn assert_cache_artifact_preserved(cwasm_path: &Path, before: &CacheArtifactSnapshot) {
    assert!(
        cwasm_path.exists(),
        "component bring-up failure must not delete a valid cache artifact"
    );
    assert_eq!(
        std::fs::read(cwasm_path).expect("read preserved cache artifact"),
        before.contents,
        "component bring-up failure must not replace a valid cache artifact"
    );

    let metadata_after = std::fs::metadata(cwasm_path).expect("stat preserved cache artifact");
    assert_eq!(
        metadata_after.modified().expect("cache modified time"),
        before.modified,
        "component bring-up failure must not rewrite a valid cache artifact"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        assert_eq!(
            (metadata_after.dev(), metadata_after.ino()),
            before.device_and_inode,
            "component bring-up failure must leave the original cache file in place"
        );
    }
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

    let corrupt_contents = b"not a valid cwasm file";
    std::fs::write(&cwasm_path, corrupt_contents).expect("Failed to write stale cache file");

    // from_file_cached should detect invalid cache, recompile, and succeed
    let factory = WasmProviderFactory::from_file_cached(&wasm, cache_dir.path())
        .await
        .expect("from_file_cached should recover from stale cache");

    assert_eq!(factory.name(), "mock");
    assert_ne!(
        std::fs::read(&cwasm_path).expect("read recovered cache artifact"),
        corrupt_contents,
        "corrupt cache artifact should be replaced after recovery"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn warm_cache_instantiation_failure_preserves_precompiled_artifact() {
    let source_dir = tempfile::tempdir().expect("Failed to create source temp dir");
    let wasm_path = source_dir.path().join("non-instantiable.wasm");
    std::fs::write(&wasm_path, NON_INSTANTIABLE_COMPONENT_WAT)
        .expect("Failed to write test component");
    let cache_dir = tempfile::tempdir().expect("Failed to create cache temp dir");

    // A cold load still precompiles the component before its deterministic
    // bring-up failure, leaving a valid cache artifact for the next attempt.
    let cold_error =
        match WasmProviderFactory::new_with_cache_dir(wasm_path.clone(), cache_dir.path()).await {
            Ok(_) => panic!("component should fail to instantiate"),
            Err(error) => error,
        };
    assert!(matches!(
        cold_error,
        WasmProviderLoadError::Instantiation(_)
    ));

    let cwasm_path = cached_component_path(cache_dir.path());
    let artifact_before = snapshot_cache_artifact(&cwasm_path);

    let warm_error =
        match WasmProviderFactory::new_with_cache_dir(wasm_path.clone(), cache_dir.path()).await {
            Ok(_) => panic!("component should still fail to instantiate"),
            Err(error) => error,
        };
    assert!(matches!(
        warm_error,
        WasmProviderLoadError::Instantiation(_)
    ));

    assert_cache_artifact_preserved(&cwasm_path, &artifact_before);
}

#[tokio::test(flavor = "multi_thread")]
async fn warm_cache_protocol_version_failure_preserves_precompiled_artifact() {
    let valid_wasm_path = skip_if_no_wasm!();
    let source_dir = tempfile::tempdir().expect("Failed to create source temp dir");
    let wasm_path = source_dir.path().join("unsupported-protocol.wasm");
    let mut component_bytes =
        std::fs::read(valid_wasm_path).expect("read valid mock provider component");

    // Keep the component binary structurally valid while making the mock
    // provider serialize an old-style info envelope. The host defaults a
    // missing protocol_version field to zero, then rejects it after the
    // component has instantiated and info() has returned successfully.
    let field_name = b"protocol_version";
    let invalid_field_name = b"protocol_xersion";
    assert_eq!(field_name.len(), invalid_field_name.len());
    let field_offsets: Vec<_> = component_bytes
        .windows(field_name.len())
        .enumerate()
        .filter_map(|(offset, bytes)| (bytes == field_name).then_some(offset))
        .collect();
    assert!(
        !field_offsets.is_empty(),
        "mock component should contain the serialized protocol_version field name"
    );
    for offset in field_offsets {
        component_bytes[offset..offset + field_name.len()].copy_from_slice(invalid_field_name);
    }
    std::fs::write(&wasm_path, component_bytes).expect("write protocol-mismatch component");

    let cache_dir = tempfile::tempdir().expect("Failed to create cache temp dir");
    let cold_error =
        match WasmProviderFactory::new_with_cache_dir(wasm_path.clone(), cache_dir.path()).await {
            Ok(_) => panic!("component with an unsupported protocol should fail bring-up"),
            Err(error) => error,
        };
    assert!(
        matches!(
            &cold_error,
            WasmProviderLoadError::Other(message) if message.contains("protocol version 0")
        ),
        "component should instantiate and fail during its protocol-version check: {cold_error}"
    );

    let cwasm_path = cached_component_path(cache_dir.path());
    let artifact_before = snapshot_cache_artifact(&cwasm_path);

    let warm_error =
        match WasmProviderFactory::new_with_cache_dir(wasm_path, cache_dir.path()).await {
            Ok(_) => panic!("component with an unsupported protocol should still fail bring-up"),
            Err(error) => error,
        };
    assert!(
        matches!(
            &warm_error,
            WasmProviderLoadError::Other(message) if message.contains("protocol version 0")
        ),
        "warm component should fail during its protocol-version check: {warm_error}"
    );

    assert_cache_artifact_preserved(&cwasm_path, &artifact_before);
}

#[tokio::test(flavor = "multi_thread")]
async fn from_precompiled_classifies_failure_by_step() {
    let source_dir = tempfile::tempdir().expect("Failed to create source temp dir");
    let wasm_path = source_dir.path().join("non-instantiable.wasm");
    std::fs::write(&wasm_path, NON_INSTANTIABLE_COMPONENT_WAT)
        .expect("Failed to write test component");
    let cwasm_path = source_dir.path().join("non-instantiable.cwasm");
    WasmProviderFactory::precompile(&wasm_path, &cwasm_path)
        .expect("test component should precompile");

    let bring_up_error = match WasmProviderFactory::from_precompiled(&cwasm_path).await {
        Ok(_) => panic!("component should fail to instantiate"),
        Err(error) => error,
    };
    assert!(matches!(
        bring_up_error,
        WasmProviderLoadError::Instantiation(_)
    ));

    std::fs::write(&cwasm_path, b"not a precompiled component")
        .expect("Failed to corrupt precompiled component");
    let deserialization_error = match WasmProviderFactory::from_precompiled(&cwasm_path).await {
        Ok(_) => panic!("corrupt component should fail deserialization"),
        Err(error) => error,
    };
    assert!(matches!(
        deserialization_error,
        WasmProviderLoadError::PrecompiledDeserialization(_)
    ));
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
        .create_provider(None, &indexmap::IndexMap::new())
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
