//! Shared fixtures for plugin-host unit and integration tests.

use std::path::{Path, PathBuf};

/// Construct an engine with the production provider loader's configuration.
pub fn engine() -> Result<wasmtime::Engine, crate::WasmProviderLoadError> {
    crate::WasmProviderFactory::create_engine()
}

/// Use the nextest run's compiled cache, with a private fallback for standalone
/// tests. This shares immutable code while callers retain their own lock/state
/// directories. The setup script keeps this outside the default UserDirs cache.
pub fn compiled_wasm_cache_dir(private: &Path) -> PathBuf {
    std::env::var_os("CARINA_TEST_WASM_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| private.join("compiled"))
}
