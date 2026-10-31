//! Tests for WasmProviderFactory precompile cache functionality.

use std::path::{Path, PathBuf};

use carina_core::UserDirs;
use carina_core::provider::ProviderFactory;
use carina_plugin_host::{WasmProviderFactory, WasmProviderLoadError};
use carina_provider_resolver::{ProviderLoadIdentity, ProviderTrustClass, VerifiedLoad};
use wit_component::{ComponentEncoder, StringEncoding};
use wit_parser::{ManglingAndAbi, Resolve};

fn test_identity(name: &str) -> ProviderLoadIdentity {
    ProviderLoadIdentity::for_test(name, ProviderTrustClass::Untrusted)
}

fn test_load(bytes: &[u8], name: &str) -> VerifiedLoad {
    VerifiedLoad::for_test(bytes, test_identity(name))
}

/// A valid component that cannot be brought up as a Carina provider because
/// neither host world can satisfy its imported interface.
const NON_INSTANTIABLE_COMPONENT_WAT: &str = r#"
(component
  (type $i (instance))
  (import "bogus:cache-regression/iface@1.0.0" (instance (type $i)))
)
"#;

fn non_instantiable_component() -> Vec<u8> {
    wat::parse_str(NON_INSTANTIABLE_COMPONENT_WAT).expect("fixture WAT must compile")
}

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

fn build_protocol_v2_component() -> Vec<u8> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let current_wit = workspace_root.join("carina-plugin-wit/wit");
    let temp = tempfile::tempdir().expect("create old-protocol WIT dir");
    let wit_dir = temp.path().join("wit");
    std::fs::create_dir(&wit_dir).expect("create WIT package dir");

    std::fs::copy(current_wit.join("types.wit"), wit_dir.join("types.wit"))
        .expect("copy provider types");
    let mut provider_wit =
        std::fs::read_to_string(current_wit.join("provider.wit")).expect("read provider WIT");
    let resource_result = ") -> result<list<resource-def>, provider-error>;";
    let state_result = ") -> result<list<tuple<string, persisted-state>>, provider-error>;";
    assert_eq!(
        provider_wit.matches(resource_result).count(),
        1,
        "v4 WIT must have a result channel for desired-resource normalization"
    );
    assert_eq!(
        provider_wit.matches(state_result).count(),
        2,
        "v4 WIT must have result channels for both state normalizers"
    );
    provider_wit = provider_wit
        .replace(resource_result, ") -> list<resource-def>;")
        .replace(state_result, ") -> list<tuple<string, persisted-state>>;");
    std::fs::write(wit_dir.join("provider.wit"), provider_wit).expect("write v2 provider WIT");
    std::fs::write(
        wit_dir.join("world.wit"),
        "package carina:provider@0.1.0;\n\nworld carina-provider {\n    export provider;\n}\n",
    )
    .expect("write v2 provider world");

    let mut resolve = Resolve::new();
    let (package, _) = resolve.push_dir(&wit_dir).expect("parse v2 WIT package");
    let world = resolve
        .select_world(&[package], Some("carina-provider"))
        .expect("select v2 provider world");
    let module = wit_component::dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    let mut module_wat = wasmprinter::print_bytes(module).expect("print dummy provider module");

    let info_json = r#"{"name":"old-v2","display_name":"Old v2 Provider","capabilities":[],"version":"2.0.0","protocol_version":2}"#;
    let escaped_info = info_json.replace('\\', "\\\\").replace('"', "\\\"");
    let old_info_body = "  (func (;0;) (type 0) (result i32)\n    unreachable\n  )";
    let new_info_body = format!(
        "  (func (;0;) (type 0) (result i32)\n    i32.const 0\n    i32.const 8\n    i32.store\n    i32.const 4\n    i32.const {}\n    i32.store\n    i32.const 0\n  )",
        info_json.len()
    );
    assert!(
        module_wat.contains("(export \"cm32p2|carina:provider/provider@0.1|info\" (func 0))"),
        "the dummy module must export info as its first function"
    );
    assert!(
        module_wat.contains(old_info_body),
        "the dummy info implementation shape changed"
    );
    module_wat = module_wat.replacen(old_info_body, &new_info_body, 1);
    module_wat = module_wat.replacen(
        "  (memory (;0;) 0)",
        &format!("  (memory (;0;) 1)\n  (data (i32.const 8) \"{escaped_info}\")"),
        1,
    );

    let mut module = wat::parse_str(&module_wat).expect("assemble v2 dummy provider module");
    wit_component::embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .expect("embed v2 component metadata");
    ComponentEncoder::default()
        .module(&module)
        .expect("configure v2 component encoder")
        .validate(true)
        .encode()
        .expect("encode v2 provider component")
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
async fn default_precompile_cache_uses_the_supplied_user_dirs() {
    let root = tempfile::tempdir().unwrap();
    let user_dirs = UserDirs::under(root.path());
    let component = wat::parse_str(NON_INSTANTIABLE_COMPONENT_WAT).unwrap();

    let error = match WasmProviderFactory::from_bytes(
        test_load(&component, "cache-test"),
        &user_dirs,
    )
    .await
    {
        Ok(_) => panic!("the deliberately unsatisfied import must reject provider startup"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("bogus:cache-regression/iface"));
    let cached = cached_component_path(user_dirs.compiled_wasm_cache_dir());
    assert!(cached.starts_with(root.path()));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_precompile_cache_creation() {
    let wasm = std::fs::read(skip_if_no_wasm!()).expect("read mock provider fixture");
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwasm_path = cache_dir.path().join("carina_provider_mock.cwasm");

    // Precompile and write .cwasm
    WasmProviderFactory::precompile(&wasm, &cwasm_path).expect("precompile should succeed");

    // .cwasm file should now exist
    assert!(cwasm_path.exists(), ".cwasm file should have been created");

    // Load from precompiled and verify the factory works
    let factory = WasmProviderFactory::from_precompiled(&cwasm_path, test_identity("mock"))
        .await
        .expect("from_precompiled should succeed");

    assert_eq!(factory.name(), "mock");
}

#[tokio::test(flavor = "multi_thread")]
async fn from_bytes_loads_mock_provider_fixture() {
    let wasm = std::fs::read(skip_if_no_wasm!()).expect("read mock provider fixture");
    let cache_dir = tempfile::tempdir().expect("create cache temp dir");

    let factory =
        WasmProviderFactory::from_bytes_with_cache_dir(test_load(&wasm, "mock"), cache_dir.path())
            .await
            .expect("provider bytes should load");

    assert_eq!(factory.name(), "mock");
}

#[tokio::test(flavor = "multi_thread")]
async fn precompile_cache_key_depends_only_on_content() {
    let fixture_path = skip_if_no_wasm!();
    let fixture_bytes = std::fs::read(fixture_path).expect("read mock provider fixture");
    let source_dir = tempfile::tempdir().expect("create source temp dir");
    let first_path = source_dir.path().join("first.wasm");
    let second_path = source_dir.path().join("second.wasm");
    std::fs::write(&first_path, &fixture_bytes).expect("write first provider path");
    std::fs::write(&second_path, &fixture_bytes).expect("write second provider path");
    let cache_dir = tempfile::tempdir().expect("create cache temp dir");

    let first_bytes = std::fs::read(first_path).expect("read first provider path");
    let second_bytes = std::fs::read(second_path).expect("read second provider path");
    WasmProviderFactory::from_bytes_with_cache_dir(
        test_load(&first_bytes, "mock"),
        cache_dir.path(),
    )
    .await
    .expect("first provider should load");
    WasmProviderFactory::from_bytes_with_cache_dir(
        test_load(&second_bytes, "mock"),
        cache_dir.path(),
    )
    .await
    .expect("second provider should load");

    let cwasm_count = std::fs::read_dir(cache_dir.path())
        .expect("read cache dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "cwasm"))
        .count();
    assert_eq!(
        cwasm_count, 1,
        "identical provider bytes must share one precompile cache entry"
    );

    let distinct_component = non_instantiable_component();
    let distinct_error = match WasmProviderFactory::from_bytes_with_cache_dir(
        test_load(&distinct_component, "mock"),
        cache_dir.path(),
    )
    .await
    {
        Ok(_) => panic!("different component should fail during provider bring-up"),
        Err(error) => error,
    };
    assert!(matches!(
        distinct_error,
        WasmProviderLoadError::Instantiation(_)
    ));
    let cwasm_count = std::fs::read_dir(cache_dir.path())
        .expect("read cache dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "cwasm"))
        .count();
    assert_eq!(
        cwasm_count, 2,
        "different provider bytes must use a different precompile cache entry"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_from_bytes_uses_cache() {
    let wasm = std::fs::read(skip_if_no_wasm!()).expect("read mock provider fixture");
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // First call: compiles and caches
    let factory1 =
        WasmProviderFactory::from_bytes_with_cache_dir(test_load(&wasm, "mock"), cache_dir.path())
            .await
            .expect("first byte load should succeed");
    assert_eq!(factory1.name(), "mock");

    // Second call: uses cache
    let factory2 =
        WasmProviderFactory::from_bytes_with_cache_dir(test_load(&wasm, "mock"), cache_dir.path())
            .await
            .expect("second byte load (cache hit) should succeed");
    assert_eq!(factory2.name(), "mock");

    // Both factories should report the mock provider's schemas.
    for factory in [&factory1, &factory2] {
        assert!(
            factory
                .schemas()
                .iter()
                .any(|schema| schema.parts().resource_type == "test.short_budget"),
            "cached and uncached factories must retain mock schema metadata"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_from_bytes_recovers_from_stale_cache() {
    let wasm = std::fs::read(skip_if_no_wasm!()).expect("read mock provider fixture");
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");

    // First, create a valid cache so we know the filename
    let factory =
        WasmProviderFactory::from_bytes_with_cache_dir(test_load(&wasm, "mock"), cache_dir.path())
            .await
            .expect("initial byte load should succeed");
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

    // The byte loader should detect invalid cache, recompile, and succeed.
    let factory =
        WasmProviderFactory::from_bytes_with_cache_dir(test_load(&wasm, "mock"), cache_dir.path())
            .await
            .expect("byte loader should recover from stale cache");

    assert_eq!(factory.name(), "mock");
    assert_ne!(
        std::fs::read(&cwasm_path).expect("read recovered cache artifact"),
        corrupt_contents,
        "corrupt cache artifact should be replaced after recovery"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn warm_cache_instantiation_failure_preserves_precompiled_artifact() {
    let wasm_bytes = non_instantiable_component();
    let cache_dir = tempfile::tempdir().expect("Failed to create cache temp dir");

    // A cold load still precompiles the component before its deterministic
    // bring-up failure, leaving a valid cache artifact for the next attempt.
    let cold_error = match WasmProviderFactory::from_bytes_with_cache_dir(
        test_load(&wasm_bytes, "mock"),
        cache_dir.path(),
    )
    .await
    {
        Ok(_) => panic!("component should fail to instantiate"),
        Err(error) => error,
    };
    assert!(matches!(
        cold_error,
        WasmProviderLoadError::Instantiation(_)
    ));

    let cwasm_path = cached_component_path(cache_dir.path());
    let artifact_before = snapshot_cache_artifact(&cwasm_path);

    let warm_error = match WasmProviderFactory::from_bytes_with_cache_dir(
        test_load(&wasm_bytes, "mock"),
        cache_dir.path(),
    )
    .await
    {
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
    let cache_dir = tempfile::tempdir().expect("Failed to create cache temp dir");
    let cold_error = match WasmProviderFactory::from_bytes_with_cache_dir(
        test_load(&component_bytes, "mock"),
        cache_dir.path(),
    )
    .await
    {
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

    let warm_error = match WasmProviderFactory::from_bytes_with_cache_dir(
        test_load(&component_bytes, "mock"),
        cache_dir.path(),
    )
    .await
    {
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
async fn protocol_v2_is_rejected_before_current_world_binding() {
    let cache_dir = tempfile::tempdir().expect("create cache temp dir");
    let component_bytes = build_protocol_v2_component();

    let error = match WasmProviderFactory::from_bytes_with_cache_dir(
        test_load(&component_bytes, "old-v2"),
        cache_dir.path(),
    )
    .await
    {
        Ok(_) => panic!("a protocol-v2 component must be rejected"),
        Err(error) => error,
    };
    let rendered = error.to_string();

    assert!(
        matches!(error, WasmProviderLoadError::Other(_)),
        "protocol rejection must not be classified as typed instantiation failure: {rendered}"
    );
    let expected = format!(
        "provider 'old-v2' was built against protocol version 2 but this host requires at least version {}",
        carina_provider_protocol::MIN_SUPPORTED_PROTOCOL_VERSION,
    );
    assert!(
        rendered.contains(&expected),
        "the stable info preflight must report the protocol mismatch: {rendered}"
    );
    assert!(
        !rendered.contains("failed to convert function to given type")
            && !rendered.contains("normalize-desired"),
        "provider world binding must not run before the version check: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn from_precompiled_classifies_failure_by_step() {
    let source_dir = tempfile::tempdir().expect("Failed to create source temp dir");
    let cwasm_path = source_dir.path().join("non-instantiable.cwasm");
    WasmProviderFactory::precompile(NON_INSTANTIABLE_COMPONENT_WAT.as_bytes(), &cwasm_path)
        .expect("test component should precompile");

    let bring_up_error =
        match WasmProviderFactory::from_precompiled(&cwasm_path, test_identity("mock")).await {
            Ok(_) => panic!("component should fail to instantiate"),
            Err(error) => error,
        };
    assert!(matches!(
        bring_up_error,
        WasmProviderLoadError::Instantiation(_)
    ));

    std::fs::write(&cwasm_path, b"not a precompiled component")
        .expect("Failed to corrupt precompiled component");
    let deserialization_error =
        match WasmProviderFactory::from_precompiled(&cwasm_path, test_identity("mock")).await {
            Ok(_) => panic!("corrupt component should fail deserialization"),
            Err(error) => error,
        };
    assert!(matches!(
        deserialization_error,
        WasmProviderLoadError::PrecompiledDeserialization(_)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn precompiled_factory_is_schema_only() {
    let wasm = std::fs::read(skip_if_no_wasm!()).expect("read mock provider fixture");
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwasm_path = cache_dir.path().join("carina_provider_mock.cwasm");

    WasmProviderFactory::precompile(&wasm, &cwasm_path).expect("precompile should succeed");

    let factory = WasmProviderFactory::from_precompiled(&cwasm_path, test_identity("mock"))
        .await
        .expect("from_precompiled should succeed");

    // `from_precompiled` is test-only and intentionally cannot mint the
    // consent witness required by credentialed runtime instances.
    let resolved = carina_core::parser::resolve_provider_attributes_with_remote(
        &carina_core::parser::InferredFile::default(),
        &std::collections::HashMap::new(),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("empty provider config set is resolved");
    let resolved = resolved.provider_or_implicit(factory.name());
    let config = carina_core::executor::prepare_provider_ready_config(&factory, resolved.as_ref())
        .expect("test provider config should pass preparation");
    let error = match factory.create_initialized_provider(None, &config).await {
        Ok(_) => panic!("a schema-only precompiled factory must reject runtime creation"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("provider runtime instance is missing consented egress")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_byte_constructor_uses_explicit_cache() {
    let wasm = std::fs::read(skip_if_no_wasm!()).expect("read mock provider fixture");

    // Use a temporary directory as the cache location to avoid polluting the real cache
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");

    let factory =
        WasmProviderFactory::from_bytes_with_cache_dir(test_load(&wasm, "mock"), cache_dir.path())
            .await
            .expect("byte constructor should succeed");

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

    let factory2 =
        WasmProviderFactory::from_bytes_with_cache_dir(test_load(&wasm, "mock"), cache_dir.path())
            .await
            .expect("second byte constructor should succeed");

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
    let wasm = std::fs::read(skip_if_no_wasm!()).expect("read mock provider fixture");
    let cache_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let cwasm_path = cache_dir.path().join("carina_provider_mock.cwasm");

    WasmProviderFactory::precompile(&wasm, &cwasm_path).expect("initial precompile should succeed");
    let factory1 = WasmProviderFactory::from_precompiled(&cwasm_path, test_identity("mock"))
        .await
        .expect("first from_precompiled should succeed");

    // Overwrite the cache file via the public API while `factory1` still
    // holds its mmap. A `precompile` implementation that writes in place
    // would pull backing pages out from under `factory1`'s Component.
    WasmProviderFactory::precompile(&wasm, &cwasm_path).expect("second precompile should succeed");

    let factory2 = WasmProviderFactory::from_precompiled(&cwasm_path, test_identity("mock"))
        .await
        .expect("second from_precompiled should succeed");

    assert_eq!(factory1.name(), "mock");
    assert_eq!(factory2.name(), "mock");
}
