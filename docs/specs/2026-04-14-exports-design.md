# Exports: Published Output Surface for Remote State

## Goal

Add `exports { }` block to publish named values from a directory for `remote_state` consumers. Only exports are visible — internal `let` bindings are not exposed.

## Keyword

`exports` — distinct from `attributes` (module virtual resource interface).

## Syntax

```crn
exports {
  account_id: string = registry_prod.account_id
  vpc_id: string = vpc.vpc_id
}
```

Same syntax as `attributes { }` — name, optional type annotation, value expression.

## Design

### 1. Parser

Add `exports_block` to pest grammar and `ExportParameter` struct:

```rust
pub struct ExportParameter {
    pub name: String,
    pub type_expr: Option<TypeExpr>,
    pub value: Option<Value>,
}
```

`ParsedFile.export_params: Vec<ExportParameter>`

### 2. State Format

Add `exports` field to `StateFile`:

```rust
pub struct StateFile {
    pub version: u32,              // 4 → 5
    pub serial: u64,
    pub lineage: String,
    pub carina_version: String,
    pub resources: Vec<ResourceState>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub exports: HashMap<String, serde_json::Value>,
}
```

State version bump: 4 → 5. Migration: v4 state reads as v5 with empty exports.

### 3. Apply

After all resources are applied and state is built:

1. Resolve export value expressions using binding map (same as ResourceRef resolution)
2. Convert resolved `Value` to `serde_json::Value`
3. Store in `state_file.exports`
4. Persist to backend

### 4. Remote State

`build_remote_bindings()` returns only exports:

```rust
pub fn build_remote_bindings(&self) -> HashMap<String, Value> {
    self.exports
        .iter()
        .map(|(k, v)| (k.clone(), json_to_value(v)))
        .collect()
}
```

No exports → empty map → consumer sees nothing. No fallback to `let` bindings.

### 5. LSP

- Semantic tokens: highlight `exports` keyword
- Completion: suggest `exports` at top level
- Diagnostics: validate export references

### 6. Validation

- Export names must be unique
- Export value expressions must reference valid bindings
- Type annotations (when present) checked against resolved values

## File Changes

| File | Change |
|------|--------|
| `carina-core/src/parser/carina.pest` | Add `exports_block` rule |
| `carina-core/src/parser/mod.rs` | Parse `exports`, add `ExportParameter`, `ParsedFile.export_params` |
| `carina-state/src/state.rs` | Add `exports` to `StateFile`, bump version |
| `carina-cli/src/commands/apply.rs` | Resolve exports after apply, persist to state |
| `carina-cli/src/commands/plan.rs` | Pass exports to plan context for display |
| `carina-core/src/resolver.rs` | Resolve export value expressions |
| `carina-lsp/src/semantic_tokens.rs` | Highlight `exports` keyword |
| `carina-lsp/src/completion/top_level.rs` | Suggest `exports` |

## Edge Cases

- Multiple `exports { }` blocks in same directory: merge (like `attributes`)
- Export referencing another export: not supported (exports reference resource bindings only)
- Export referencing anonymous resource: allowed if the anonymous resource has the referenced attribute
- No exports block: remote_state returns empty map
