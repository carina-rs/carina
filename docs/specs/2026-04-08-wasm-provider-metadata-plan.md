# Implementation Plan: WASM Provider Metadata

## Task 1/5 — WIT + SDK: Add metadata functions

**Files:**
- `carina-plugin-wit/wit/provider.wit` — add `provider-config-completions`, `identity-attributes`, `get-enum-aliases`
- `carina-plugin-sdk/src/lib.rs` — add trait methods with defaults, wire up WIT dispatch

**Acceptance Criteria:**
- [ ] WIT compiles
- [ ] `cargo build -p carina-plugin-sdk` passes
- [ ] Default implementations return empty

## Task 2/5 — ProviderFactory trait: Replace region_completions with config_completions

**Files:**
- `carina-core/src/provider.rs` — rename `region_completions()` to `config_completions()`, return `HashMap<String, Vec<CompletionValue>>`
- `carina-lsp/src/backend.rs` — update ProviderState to use `config_completions`
- `carina-lsp/src/completion/` — update to use attribute-keyed completions
- `carina-lsp/src/semantic_tokens.rs` — update region detection
- `carina-lsp/src/hover.rs` — update region display

**Acceptance Criteria:**
- [ ] `region_completions()` removed from ProviderFactory trait
- [ ] `config_completions()` returns `HashMap<String, Vec<CompletionValue>>`
- [ ] LSP uses config_completions with attribute name lookup
- [ ] All existing LSP tests pass

## Task 3/5 — Plugin Host: Call WIT functions and cache results

**Files:**
- `carina-plugin-host/src/wasm_factory.rs` — call new WIT functions at init, cache results, implement ProviderFactory trait methods (`config_completions`, `identity_attributes`, `get_enum_alias_reverse`)

**Acceptance Criteria:**
- [ ] `WasmProviderFactory` returns cached metadata from WASM
- [ ] `cargo test -p carina-plugin-host` passes
- [ ] Graceful handling if WASM returns empty/errors

## Task 4/5 — Provider repos: Implement metadata methods

**Repos:** `carina-provider-awscc`, `carina-provider-aws`

**Files (each repo):**
- Provider impl — implement `config_completions()`, `identity_attributes()`, `get_enum_aliases()` using `carina-aws-types`

**Acceptance Criteria:**
- [ ] Both providers compile with new SDK
- [ ] WASM builds succeed
- [ ] Config completions return region values under "region" key
- [ ] Identity attributes return `["region"]`
- [ ] Enum aliases return correct mappings

## Task 5/5 — Remove carina-aws-types usage from CLI wiring

**Files:**
- `carina-cli/src/wiring.rs` — use ProviderFactory methods instead of direct `carina-aws-types` calls
- `carina-cli/Cargo.toml` — remove `carina-aws-types` dep if no longer needed at runtime

**Acceptance Criteria:**
- [ ] CLI no longer imports `carina-aws-types` for runtime behavior
- [ ] LSP region completions work with WASM providers
- [ ] `cargo test` passes across workspace
- [ ] Acceptance tests pass
