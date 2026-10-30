# Carina vendor note

- Upstream: `wasmtime-internal-component-macro` 43.0.0
- Source: crates.io (`https://crates.io/crates/wasmtime-internal-component-macro/43.0.0`)
- License: `Apache-2.0 WITH LLVM-exception`

This vendored copy has exactly two code changes:

1. At upstream `build.rs:4`, remove
   `println!("cargo:rustc-env=DEBUG_OUTPUT_DIR={out_dir}");` and its now-unused
   `out_dir` binding, while keeping `cargo:rerun-if-changed`.
2. At vendored `src/bindgen.rs:42`, replace
   `Path::new(env!("DEBUG_OUTPUT_DIR"))` with
   `std::env::var_os("OUT_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)`.

The manifest also omits test targets and dev dependencies whose files or crates
are not vendored. Removing the compile-time absolute `OUT_DIR` keeps the proc
macro crate hash identical across worktrees. The runtime lookup is reached only
when debug/generated-file output is explicitly requested, so normal macro
expansion is unchanged.

When bumping, diff this copy against the new crates.io source and re-check both
changes. Drop the patch and this directory once upstream stops baking `OUT_DIR`
into the macro.
