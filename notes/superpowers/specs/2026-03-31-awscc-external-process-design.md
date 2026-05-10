# Phase 2a: Convert awscc Provider to External Process Binary

**Date**: 2026-03-31
**Status**: Draft
**Depends on**: Phase 1 (pluggable provider infrastructure) — merged in PR #1415

## Overview

Convert the existing `carina-provider-awscc` crate into a dual lib+binary crate. The binary implements `CarinaProvider` from `carina-plugin-sdk` and runs as an external process communicating via stdin/stdout JSON-RPC. Also consolidate `carina-provider-mock-process` back into `carina-provider-mock` using the same pattern.

## Goals

- `carina-provider-awscc` works as an external process binary via `file://` source
- Full plan/apply cycle works through the external process (CRUD, normalization, state hydration, default tags, schemas)
- Existing in-process behavior is preserved (no user-facing changes for configs without `source`)
- `carina-provider-mock-process` is eliminated; `carina-provider-mock` gains the binary

## Non-Goals

- `carina init` / automatic download (Phase 2b)
- LSP-specific methods: `region_completions`, `get_enum_alias_reverse`, `format_schema_key`, `identity_attributes` (separate work)
- Removing `carina-provider-awscc` as a direct dependency of `carina-cli` (Phase 3+)

## Architecture

### Binary in Existing Crate

Add `main.rs` to existing crates rather than creating new crates:

```
carina-provider-awscc/
  src/
    lib.rs          — existing library code (unchanged)
    main.rs         — NEW: CarinaProvider wrapper + carina_plugin_sdk::run()

carina-provider-mock/
  src/
    lib.rs          — existing library code (unchanged)
    main.rs         — NEW: CarinaProvider wrapper + carina_plugin_sdk::run()
```

Each crate's `Cargo.toml` adds a `[[bin]]` section and `carina-plugin-sdk` dependency. The library interface remains unchanged for in-process consumers.

### Protocol Extensions

Two new RPC methods are needed for `ProviderNormalizer` functionality that Phase 1 did not cover:

| Method | Params | Result | Purpose |
|--------|--------|--------|---------|
| `hydrate_read_state` | `{states, saved_attrs}` | `{states}` | Restore attributes not returned by CloudControl API |
| `merge_default_tags` | `{resources, default_tags, schemas}` | `{resources}` | Merge provider default_tags into resources |

These are added to `carina-provider-protocol/src/methods.rs` and dispatched in `carina-plugin-sdk`.

### CarinaProvider Trait Extensions

Add to the `CarinaProvider` trait in `carina-plugin-sdk`:

```rust
/// Hydrate read state with saved attributes that APIs don't return.
fn hydrate_read_state(
    &self,
    states: &mut HashMap<String, State>,
    saved_attrs: &HashMap<String, HashMap<String, Value>>,
) {
    let _ = (states, saved_attrs);
}

/// Merge provider default_tags into resources.
fn merge_default_tags(
    &self,
    resources: &mut Vec<Resource>,
    default_tags: &HashMap<String, Value>,
    schemas: &Vec<ResourceSchema>,
) {
    let _ = (resources, default_tags, schemas);
}
```

Default implementations are no-ops, so existing providers (mock) are unaffected.

### Schema Transfer

Phase 1's `ProcessProviderFactory::schemas()` returned `vec![]`. Phase 2a implements the full pipeline:

1. Host sends `schemas` RPC to provider process
2. Provider returns `Vec<proto::ResourceSchema>` with all attribute schemas
3. Host converts `proto::ResourceSchema` → `core::ResourceSchema` in `carina-plugin-host/src/convert.rs`
4. `ProcessProviderFactory::schemas()` returns the converted schemas

The conversion must handle all `AttributeType` variants including `Struct` with nested fields and `Custom` types (mapped to `String` for external providers since validation functions can't cross the process boundary).

### ProcessProviderFactory Normalizer Support

Phase 1's `ProcessProviderFactory` did not implement `create_normalizer()`. Phase 2a adds a `ProcessProviderNormalizer` that forwards normalizer calls to the provider process:

```rust
pub struct ProcessProviderNormalizer {
    process: Mutex<ProviderProcess>,
}

impl ProviderNormalizer for ProcessProviderNormalizer {
    fn normalize_desired(&self, resources: &mut [Resource]) { ... }
    fn normalize_state(&self, current_states: &mut HashMap<ResourceId, State>) { ... }
    fn hydrate_read_state(&self, current_states: &mut HashMap<ResourceId, State>, saved_attrs: &SavedAttrs) { ... }
    fn merge_default_tags(&self, resources: &mut [Resource], default_tags: &HashMap<String, Value>, schemas: &HashMap<String, ResourceSchema>) { ... }
}
```

This requires the provider process to stay alive across the entire plan/apply lifecycle (not just CRUD operations). The `ProcessProviderFactory::create_normalizer()` returns a normalizer backed by the same (or a separate) process.

### awscc Binary Implementation

`carina-provider-awscc/src/main.rs` wraps existing code:

```rust
struct AwsccProcessProvider {
    provider: Option<AwsccProvider>,    // initialized lazily via initialize()
    normalizer: AwsccNormalizer,
}

impl CarinaProvider for AwsccProcessProvider {
    fn info(&self) -> ProviderInfo { ... }
    fn schemas(&self) -> Vec<ResourceSchema> { /* convert from existing all_schemas() */ }
    fn validate_config(&self, attrs: &HashMap<String, Value>) -> Result<(), String> { ... }
    fn initialize(&mut self, attrs: &HashMap<String, Value>) -> Result<(), String> {
        // Create AwsccProvider with region from attrs
        // This is async internally — use tokio runtime
    }
    fn read(&self, ...) -> Result<State, ProviderError> { /* delegate to self.provider */ }
    fn create(&self, ...) -> Result<State, ProviderError> { /* delegate to self.provider */ }
    fn update(&self, ...) -> Result<State, ProviderError> { /* delegate to self.provider */ }
    fn delete(&self, ...) -> Result<State, ProviderError> { /* delegate to self.provider */ }
    fn normalize_desired(&self, ...) { /* delegate to self.normalizer */ }
    fn normalize_state(&self, ...) { /* delegate to self.normalizer */ }
    fn hydrate_read_state(&self, ...) { /* delegate to self.normalizer */ }
    fn merge_default_tags(&self, ...) { /* delegate to self.normalizer */ }
}
```

The `AwsccProvider` methods are async (return `BoxFuture`), but the `CarinaProvider` trait methods are sync. The binary creates a tokio runtime in `main()` and the wrapper blocks on async calls via `runtime.block_on()`.

### Mock Provider Consolidation

Delete `carina-provider-mock-process` crate. Add `main.rs` to `carina-provider-mock`:

- Move the `MockProcessProvider` logic into `carina-provider-mock/src/main.rs`
- Add `carina-plugin-sdk` dependency to `carina-provider-mock`
- Update workspace `Cargo.toml` to remove `carina-provider-mock-process`
- Update integration tests to build `carina-provider-mock` binary instead

## Data Flow

```
.crn with source = "file://path/to/carina-provider-awscc"
  │
  ├─ CLI parses source, spawns binary
  │
  ├─ schemas RPC → provider returns all resource schemas
  │   └─ Host converts proto::ResourceSchema → core::ResourceSchema
  │
  ├─ validate_config RPC → provider validates region etc.
  │
  ├─ initialize RPC → provider creates AWS CloudControl client
  │
  ├─ normalize_desired RPC → provider resolves enum identifiers
  │
  ├─ read RPC (per resource) → provider calls CloudControl GetResource
  │
  ├─ normalize_state RPC → provider normalizes state enum values
  │
  ├─ hydrate_read_state RPC → provider restores unreturned attributes
  │
  ├─ merge_default_tags RPC → provider merges default tags
  │
  ├─ (differ produces plan)
  │
  ├─ create/update/delete RPCs → provider calls CloudControl
  │
  └─ shutdown RPC → provider exits
```

## Process Lifecycle Change

Phase 1 spawns a process per operation (factory spawns for info, then for provider). Phase 2a needs the process to live across the entire plan/apply cycle, shared between the Provider and Normalizer.

The `ProcessProviderFactory::create_provider()` and `create_normalizer()` must share the same `ProviderProcess`. Approach: `create_provider()` spawns the process and stores it in an `Arc<Mutex<ProviderProcess>>`, `create_normalizer()` clones the Arc.

## Test Strategy

- **Unit tests**: Schema conversion round-trips (proto ↔ core)
- **Integration test**: Build awscc binary, spawn via `ProcessProviderFactory`, verify `schemas()` returns non-empty list
- **E2E test**: Use `file://` source in a `.crn`, run `carina plan` against real AWS (acceptance test)
- **Regression**: All existing `cargo test` must pass unchanged

## Migration Path

1. Phase 2a (this spec): `file://` works, in-process preserved
2. Phase 2b: `carina init` + `.carina/providers/` + GitHub Releases download
3. Phase 3: Remove in-process awscc from carina-cli dependencies
