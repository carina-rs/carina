# Implementation Plan: Exports

## Task 1/5: Parser — add exports block

**Files:** `carina-core/src/parser/carina.pest`, `carina-core/src/parser/mod.rs`

**Changes:**
- Add `exports_block` grammar rule (mirror `attributes_block`)
- Add `ExportParameter` struct
- Add `ParsedFile.export_params: Vec<ExportParameter>`
- Parse `exports { name: type = expr }` blocks
- Merge across files in directory-based modules

**Tests:** Parser tests for exports block, single and multiple params, type annotations

## Task 2/5: State format — add exports field

**Files:** `carina-state/src/state.rs`

**Changes:**
- Add `exports: HashMap<String, serde_json::Value>` to `StateFile`
- Bump `CURRENT_VERSION` to 5
- Handle v4 → v5 migration (empty exports)
- Update `build_remote_bindings()` to return only exports

**Tests:** State roundtrip with exports, migration from v4, empty exports

## Task 3/5: Apply — resolve and persist exports

**Files:** `carina-cli/src/commands/apply.rs`, `carina-core/src/resolver.rs`

**Changes:**
- After apply, resolve export value expressions using binding map
- Convert resolved Values to JSON
- Store in `state_file.exports` before saving
- Handle exports in plan-file apply path too

**Tests:** Integration test: apply with exports, verify state contains them

## Task 4/5: Remote state — read exports

**Files:** `carina-cli/src/commands/plan.rs`

**Changes:**
- `load_remote_state_*` functions use `build_remote_bindings()` which now returns exports
- Verify consumer can reference `remote.export_name`
- Remove old binding-based exposure

**Tests:** End-to-end: directory A exports, directory B reads via remote_state

## Task 5/5: LSP — exports keyword support

**Files:** `carina-lsp/src/semantic_tokens.rs`, `carina-lsp/src/completion/top_level.rs`

**Changes:**
- Highlight `exports` keyword in semantic tokens
- Add `exports` to top-level completions
- Diagnostics for export references

**Tests:** LSP tests for highlighting and completion
