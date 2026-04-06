# LSP Provider Schema Loading

## Goal

Enable the LSP to load provider schemas from WASM plugins so that completions, diagnostics, hover, and semantic tokens work with real resource type information. Currently, the LSP initializes with an empty factories vector, making all schema-dependent features non-functional.

## Chosen Approach

**Read provider configuration from workspace `.crn` files** — the same approach the CLI uses.

### Rationale

- `.crn` files are the single source of truth for provider configuration
- `build_factories_from_providers()` in `carina-cli/src/wiring.rs` already implements the full pipeline (parse → resolve WASM → create factory)
- WASM binaries are typically already cached from prior CLI usage (`carina plan`, etc.)
- No additional editor configuration required — opening a workspace "just works"

### Rejected Alternatives

- **LSP `initializationOptions`**: Requires users to configure providers in both `.crn` and editor settings. Violates DRY.
- **Embedded/bundled schemas**: Would require LSP rebuilds for every provider schema change. Not scalable.

## Design

### Initialization Flow

1. Client sends `initialize` with `rootUri` / `workspaceFolders`
2. LSP scans workspace root for `.crn` files
3. Parse `provider` blocks from found files (reuse `carina-core` parser)
4. For each provider with a `source` attribute:
   - Resolve WASM binary path (check cache only — no auto-download)
   - If binary exists: load `WasmProviderFactory` and collect schemas
   - If binary missing: skip, log warning via `window/logMessage`
5. Build `CompletionProvider`, `DiagnosticEngine`, `HoverProvider`, `SemanticTokensProvider` with collected schemas

### Key Design Decisions

1. **No auto-download of WASM binaries** — LSP should not have network side effects. If a provider binary is not cached, the LSP degrades gracefully and logs a message suggesting the user run `carina plan` (or a future `carina init` command) to download providers.

2. **Schema reload on file change** — Watch `.crn` files via `didChangeWatchedFiles`. When a provider block changes, re-parse and reload schemas. This requires rebuilding the completion/diagnostic/hover providers, which means they need to be behind `Arc<RwLock<>>` or similar.

3. **Reuse existing code** — Extract provider resolution logic from `carina-cli` into a shared location (or add `carina-cli` as a dependency of `carina-lsp`, or extract a `carina-provider-resolver` crate) so the LSP can call `build_factories_from_providers()` and the provider cache resolution.

### Architecture Changes

#### Option A: Extract `carina-provider-resolver` crate

Move provider resolution logic (`provider_resolver.rs`, relevant parts of `wiring.rs`) into a new crate that both `carina-cli` and `carina-lsp` depend on.

#### Option B: LSP depends on `carina-cli`

Simpler but creates an unusual dependency direction (LSP depending on CLI).

#### Option C: Duplicate minimal resolution logic in LSP

Only need cache lookup (no download), so the code is small. But risks drift.

**Recommended: Option A** — cleanest separation of concerns.

### File Changes

- **New crate**: `carina-provider-resolver/` — extracted from `carina-cli/src/provider_resolver.rs` and factory-building logic from `wiring.rs`
- **`carina-lsp/src/main.rs`**: Use workspace root to find `.crn` files, parse providers, build factories
- **`carina-lsp/src/backend.rs`**: Support schema reload (providers behind `Arc<RwLock<>>`)
- **`carina-lsp/Cargo.toml`**: Add dependencies on `carina-provider-resolver`, `carina-plugin-host`
- **`carina-cli/Cargo.toml`**: Replace inlined resolver with `carina-provider-resolver` dependency

### Edge Cases

- **No `.crn` files in workspace**: Operate in schema-less mode (current behavior)
- **Multiple `.crn` files with different providers**: Merge all provider configs
- **WASM load failure**: Skip provider, log warning, continue with remaining providers
- **Workspace root not available**: Fall back to schema-less mode
- **Lock file validation**: LSP should validate lock constraints same as CLI

## Constraints

- LSP must remain responsive during initialization — WASM loading should not block the event loop
- No network access from LSP
- Graceful degradation when providers are unavailable
