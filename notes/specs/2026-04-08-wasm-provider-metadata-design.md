# WASM Provider Metadata via WIT Interface

## Goal

WASM providers currently lack metadata needed for LSP completions and anonymous resource ID calculation. Extend the WIT interface so WASM providers can supply this metadata, removing the need for provider-specific code in core crates. Enum aliases are carried by schemas and resolved exclusively by the host; protocol v6 has no enum-alias metadata export.

## Problem

`WasmProviderFactory` uses default (empty) implementations for:

| Method | Purpose | Consumer |
|--------|---------|----------|
| `region_completions()` | Provider config attribute value suggestions | LSP completion, semantic tokens |
| `identity_attributes()` | Attributes for anonymous resource ID hashing | CLI identifier computation |

Currently, this data comes from `carina-aws-types` which lives in the core repository — an architectural violation since it's AWS-specific.

## Chosen Approach: WIT Interface Extension

Add two new functions to the `provider` WIT interface. Complex data follows the existing JSON pattern used by `info()` and `schemas()`.

### WIT Changes

```wit
interface provider {
    // ... existing functions ...

    /// Returns JSON-serialized HashMap<String, Vec<CompletionValue>>
    /// Key is attribute name (e.g., "region"), value is completion candidates.
    /// Each CompletionValue has { value, description }.
    /// This is provider-agnostic: AWS providers return region completions,
    /// other providers may return different attribute completions.
    provider-config-completions: func() -> string;

    /// Returns the list of identity attribute names (e.g., ["region"]).
    identity-attributes: func() -> list<string>;

}
```

### ProviderFactory Trait Changes

Replace `region_completions()` with a generic method:

```rust
// Before (AWS-specific)
fn region_completions(&self) -> Vec<CompletionValue> { vec![] }

// After (provider-agnostic)
fn config_completions(&self) -> HashMap<String, Vec<CompletionValue>> { HashMap::new() }
```

Callers that currently do `f.region_completions()` will change to:
```rust
f.config_completions().get("region").cloned().unwrap_or_default()
```

Or better, the LSP completion provider can iterate all config completions and match by attribute name contextually.

### Data Flow

```
WASM Provider (aws/awscc repo)
  └─ implements provider-config-completions and identity-attributes
       │
       ▼
carina-plugin-host (WasmProviderFactory)
  └─ calls WIT functions at init, caches results
       │
       ▼
carina-core (ProviderFactory trait)
  └─ config_completions(), identity_attributes()
       │
       ▼
LSP / CLI
  └─ uses completions and identity attrs
```

### Caching Strategy

Both are called once during factory construction and cached:
- `provider-config-completions` → `HashMap<String, Vec<CompletionValue>>`
- `identity-attributes` → `Vec<String>`

### Protocol Compatibility

The SDK supplies empty defaults for completion and identity metadata. Protocol
v6 is intentionally breaking: providers must be rebuilt against the v6 WIT,
which removes the former enum-alias export.

**Important**: New WIT exports must be present in the WASM component. The SDK provides defaults, so provider code changes are minimal (recompile with new SDK). Both provider repos must be updated before the host expects these exports.

## Edge Cases

- **Provider without regions** (non-AWS): Returns empty `config_completions` — no effect
- **Provider with custom config attributes**: Returns completions for its own attributes (e.g., "endpoint" for a custom provider)
- **WASM function call failure**: Log warning, return default (empty) — graceful degradation

## Key Design Decisions

1. **`provider-config-completions` instead of `region-completions`** — provider-agnostic; any provider can offer completions for any config attribute
2. **JSON serialization for complex types** — follows `info()` / `schemas()` pattern
3. **`list<string>` for `identity-attributes`** — simple enough for native WIT types
4. **Cache everything at init** — metadata doesn't change during a session
