# LSP Provider Schema Loading — Implementation Plan

## Task Breakdown

### Task 1: Extract `carina-provider-resolver` crate

Extract provider resolution logic from `carina-cli` into a shared crate.

**Files:**
- New: `carina-provider-resolver/Cargo.toml`, `carina-provider-resolver/src/lib.rs`
- Move from: `carina-cli/src/provider_resolver.rs` → `carina-provider-resolver/src/lib.rs`
- Extract from: `carina-cli/src/wiring.rs` — `build_factories_from_providers()` function
- Update: `carina-cli/Cargo.toml` — add `carina-provider-resolver` dependency
- Update: `carina-cli/src/wiring.rs` — use `carina_provider_resolver::build_factories_from_providers()`
- Update: `Cargo.toml` (workspace) — add new crate member

**Test approach:**
- Existing `carina-cli` tests must continue to pass (no behavior change)
- Add unit tests in new crate for cache path resolution and factory building

**Acceptance criteria:**
- `cargo test -p carina-cli` passes
- `cargo test -p carina-provider-resolver` passes
- No duplicate code between CLI and resolver crate

### Task 2: Add workspace `.crn` file discovery to LSP

Parse provider configurations from workspace `.crn` files during LSP initialization.

**Files:**
- New: `carina-lsp/src/workspace.rs` — workspace scanning and provider config extraction
- Update: `carina-lsp/src/main.rs` — read `rootUri` from init params, scan workspace
- Update: `carina-lsp/src/backend.rs` — accept `rootUri` in constructor or init

**Test approach:**
- Unit test: given a temp directory with `.crn` files containing provider blocks, verify correct `ProviderConfig` extraction
- Unit test: empty directory returns empty configs
- Unit test: `.crn` file without provider blocks returns empty configs

**Acceptance criteria:**
- LSP correctly discovers and parses provider blocks from workspace `.crn` files
- Missing or unreadable files are skipped gracefully

### Task 3: Load WASM provider factories in LSP

Use the extracted resolver to build `WasmProviderFactory` instances from discovered provider configs.

**Files:**
- Update: `carina-lsp/Cargo.toml` — add `carina-provider-resolver`, `carina-plugin-host` dependencies
- Update: `carina-lsp/src/main.rs` — call `build_factories_from_providers()` with discovered configs
- Update: `carina-lsp/src/backend.rs` — pass real factories to `Backend::new()`

**Test approach:**
- Integration test: LSP initialized with a workspace containing a mock provider `.crn` file and cached WASM binary provides completions
- Test: missing WASM binary results in graceful degradation (no crash, warning logged)

**Acceptance criteria:**
- LSP loads schemas from cached WASM providers
- Missing WASM binaries produce a log warning, not an error
- Previously `#[ignore = "requires provider schemas"]` tests can be un-ignored or replaced

### Task 4: Support schema reload on `.crn` file changes

Re-parse provider configs and reload schemas when workspace `.crn` files change.

**Files:**
- Update: `carina-lsp/src/backend.rs` — register file watcher for `**/*.crn`, wrap providers in `Arc<RwLock<>>`
- Update: `carina-lsp/src/backend.rs` — implement `did_change_watched_files` handler
- Update: completion/diagnostics/hover providers to read from shared mutable state

**Test approach:**
- Integration test: modify `.crn` file, verify schema set is updated
- Test: adding a new provider triggers schema reload
- Test: removing a provider block removes its schemas

**Acceptance criteria:**
- Editing provider blocks in `.crn` files triggers schema reload without LSP restart
- All LSP features (completion, diagnostics, hover, semantic tokens) use updated schemas

### Task 5: Handle async WASM loading without blocking LSP

Ensure WASM provider loading does not block the LSP event loop.

**Files:**
- Update: `carina-lsp/src/main.rs` or `backend.rs` — use `tokio::task::spawn_blocking` for WASM loading
- Update: `carina-lsp/src/backend.rs` — initialize with empty schemas, populate asynchronously, notify client when ready

**Test approach:**
- Verify LSP responds to `initialize` promptly even with slow WASM loading
- Verify basic features (syntax highlighting, parse errors) work before WASM loading completes

**Acceptance criteria:**
- LSP `initialize` response is not delayed by WASM loading
- Schema-dependent features become available after WASM loading completes
- Client receives progress notification or log message when schemas are loaded

## Dependency Order

```
Task 1 (extract crate) → Task 2 (workspace discovery) → Task 3 (WASM loading) → Task 4 (reload)
                                                                                → Task 5 (async loading)
```

Tasks 4 and 5 are independent of each other but both depend on Task 3.
