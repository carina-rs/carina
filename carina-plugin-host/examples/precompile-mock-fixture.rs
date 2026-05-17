//! Precompile the mock provider `.wasm` fixture to a `.cwasm` once, for the
//! whole CI Test job to share (refs #3089).
//!
//! `cargo nextest` runs every test in its own process, so a process-global
//! `OnceCell` cannot share the ~25s `precompile_component` across the 15
//! WASM tests on CI's slow runner. The only cross-process share is an
//! on-disk artifact produced *outside* the test processes. This example is
//! run as a CI build step (after the wasm fixture build, before
//! `cargo nextest run`); the test helper then loads this prebuilt `.cwasm`
//! via `from_precompiled` instead of re-running `precompile` per test.
//!
//! The `.cwasm` is wasmtime-version-matched because this example links the
//! same `carina-plugin-host` crate (same wasmtime) as the test binaries.

use std::path::PathBuf;
use std::process::ExitCode;

use carina_plugin_host::WasmProviderFactory;

/// Deterministic location of the mock fixture `.wasm` and its prebuilt
/// `.cwasm`, relative to the workspace target dir. Kept in sync with the
/// test helper's lookup (see `carina-plugin-host/tests/`).
fn fixture_paths() -> Option<(PathBuf, PathBuf)> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target/wasm32-wasip2/debug");
    for name in &["carina_provider_mock.wasm", "carina-provider-mock.wasm"] {
        let wasm = target.join(name);
        if wasm.exists() {
            let cwasm = target.join("carina_provider_mock.precompiled.cwasm");
            return Some((wasm, cwasm));
        }
    }
    None
}

fn main() -> ExitCode {
    let Some((wasm, cwasm)) = fixture_paths() else {
        eprintln!(
            "precompile-mock-fixture: mock .wasm not found under \
             target/wasm32-wasip2/debug; build it first with \
             `cargo build -p carina-provider-mock --target wasm32-wasip2`"
        );
        return ExitCode::from(1);
    };

    match WasmProviderFactory::precompile(&wasm, &cwasm) {
        Ok(()) => {
            eprintln!(
                "precompile-mock-fixture: wrote {} ({} bytes)",
                cwasm.display(),
                std::fs::metadata(&cwasm).map(|m| m.len()).unwrap_or(0)
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("precompile-mock-fixture: precompile failed: {e}");
            ExitCode::from(1)
        }
    }
}
