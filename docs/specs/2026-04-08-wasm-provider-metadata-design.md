# WASM Provider Metadata via WIT Interface

## Goal

WASM providers currently lack metadata needed for LSP completions, anonymous resource ID calculation, and enum alias resolution. Extend the WIT interface so WASM providers can supply this metadata, removing the need for provider-specific code in core crates.

## Problem

`WasmProviderFactory` uses default (empty) implementations for:

| Method | Purpose | Consumer |
|--------|---------|----------|
| `region_completions()` | Provider config attribute value suggestions | LSP completion, semantic tokens |
| `identity_attributes()` | Attributes for anonymous resource ID hashing | CLI identifier computation |
| `get_enum_alias_reverse()` | DSL alias → canonical value mapping | CLI state comparison |

Currently, this data comes from `carina-aws-types` which lives in the core repository — an architectural violation since it's AWS-specific.

## Chosen Approach: WIT Interface Extension

Add three new functions to the `provider` WIT interface. Each returns JSON-serialized data, following the existing pattern used by `info()` and `schemas()`.

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

    /// Returns JSON-serialized HashMap<String, HashMap<String, HashMap<String, String>>>
    /// Structure: resource_type -> attr_name -> alias -> canonical_value
    /// Used for DSL alias resolution (e.g., "all" -> "-1" for ip_protocol).
    get-enum-aliases: func() -> string;
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
  └─ implements provider-config-completions, identity-attributes, get-enum-aliases
       │
       ▼
carina-plugin-host (WasmProviderFactory)
  └─ calls WIT functions at init, caches results
       │
       ▼
carina-core (ProviderFactory trait)
  └─ config_completions(), identity_attributes(), get_enum_alias_reverse()
       │
       ▼
LSP / CLI
  └─ uses completions, identity attrs, alias resolution
```

### Caching Strategy

All three are called once during factory construction and cached:
- `provider-config-completions` → `HashMap<String, Vec<CompletionValue>>`
- `identity-attributes` → `Vec<String>`
- `get-enum-aliases` → nested `HashMap` for alias lookups

`get_enum_alias_reverse()` on the `ProviderFactory` trait does a lookup in the cached alias map.

### Backward Compatibility

All new WIT functions have default implementations in the SDK (return empty). Existing WASM providers that don't implement them continue to work — they just won't provide completions or aliases until updated.

**Important**: New WIT exports must be present in the WASM component. The SDK provides defaults, so provider code changes are minimal (recompile with new SDK). Both provider repos must be updated before the host expects these exports.

## Edge Cases

- **Provider without regions** (non-AWS): Returns empty `config_completions` — no effect
- **Provider with custom config attributes**: Returns completions for its own attributes (e.g., "endpoint" for a custom provider)
- **Provider without enum aliases**: Returns empty alias map — no effect
- **WASM function call failure**: Log warning, return default (empty) — graceful degradation

## Key Design Decisions

1. **`provider-config-completions` instead of `region-completions`** — provider-agnostic; any provider can offer completions for any config attribute
2. **JSON serialization for complex types** — follows `info()` / `schemas()` pattern
3. **`list<string>` for `identity-attributes`** — simple enough for native WIT types
4. **`get-enum-aliases` returns full map** — one WASM call at init vs. many per-resource calls
5. **Cache everything at init** — metadata doesn't change during a session
