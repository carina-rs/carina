# Design: Replace `remote_state` with `upstream_state`

Date: 2026-04-16
Status: Proposed
Related: #1894 (depends on this)

## Goal

Replace the `remote_state` DSL with `upstream_state`, which takes a single `source` attribute pointing at the upstream directory. The upstream's own backend configuration is derived from its `.crn` files, so users declare the source of truth (a directory) rather than duplicating state location.

This is a prerequisite for #1894 (surface upstream export changes in downstream plans): having the upstream source directory addressable by the planner unlocks reading its `exports.crn` for shape/type checks.

## Non-Goals

- Cross-repo / external state references. Removed from scope; all upstream state is assumed to live in a sibling directory under the same filesystem.
- Upstream export shape mismatch detection and plan-output diff display. Those are #1894 and done on top of this work.
- State file format changes. `StateFile.exports` is unchanged.

## Chosen Approach

**Approach 1 — reuse `load_configuration`.** When the planner encounters an `upstream_state` block, it calls the existing configuration loader on the `source` directory to obtain a `ParsedFile`, reads the backend from that file, and loads the state through the already-instantiated backend resolver.

### Why

- Carina already solves "parse a directory → resolve its backend → read its state" for the top-level run. Reusing that avoids a parallel lightweight parser that would drift from the real one.
- A secondary light parser (Approach 2) would need to know about every backend DSL change twice — a long-term maintenance tax.
- A state-file-only approach (Approach 3) does not work: S3 backends have no local `carina.state.json`, so discovering the backend without parsing the upstream `.crn` is impossible.

## DSL

```
upstream_state "orgs" {
  source = "../organizations"
}
```

- `source` is required. Relative paths are resolved against the enclosing `.crn` file's directory.
- The directory must contain a valid Carina configuration (same rules as `load_module` applies: a `main.crn` or flat `*.crn` files).
- A single `upstream_state` block publishes one binding (`orgs` in the example). Its exports become `orgs.<export_name>`.

### Removed DSL

- `remote_state "<name>" { backend { ... } }` — gone. No alias, no deprecation shim. Memory rule "No backward compatibility required" applies.

## Architecture

### New types (carina-core)

```rust
// carina-core/src/parser/mod.rs
pub struct UpstreamState {
    pub binding: String,
    pub source: PathBuf,  // raw, unresolved
}

pub struct ParsedFile {
    // ...
    pub upstream_states: Vec<UpstreamState>,  // replaces `remote_states: Vec<RemoteState>`
}
```

`RemoteState`, `RemoteStateBackend`, and `ParsedFile.remote_states` are deleted.

### Resolution flow (carina-cli)

Rename `load_remote_states` → `load_upstream_states` in `carina-cli/src/commands/plan.rs`. New body:

```rust
async fn load_upstream_states(
    base_dir: &Path,
    upstream_states: &[UpstreamState],
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<PathBuf>,
) -> Result<HashMap<String, HashMap<String, Value>>, AppError> {
    let mut bindings = HashMap::new();
    for us in upstream_states {
        let source_abs = base_dir.join(&us.source).canonicalize()?;
        if !cycle_guard.insert(source_abs.clone()) {
            return Err(AppError::Config(format!(
                "upstream_state cycle detected at {}",
                source_abs.display()
            )));
        }
        let loaded = load_configuration_with_config(&source_abs, provider_context)?;
        let upstream_backend = resolve_backend(loaded.parsed.backend.as_ref()).await?;
        let state_file = upstream_backend.read_state().await?
            .ok_or_else(|| AppError::Config(format!(
                "upstream_state '{}': no state at {}",
                us.binding, source_abs.display()
            )))?;
        bindings.insert(us.binding.clone(), state_file.build_remote_bindings());
        cycle_guard.remove(&source_abs);
    }
    Ok(bindings)
}
```

- `cycle_guard` is threaded through so that `A → B → A` references error with a helpful message instead of stack-overflowing.
- `load_configuration_with_config` already handles directory modules (finds `main.crn` or flat files).
- `resolve_backend` is the same helper used by `run_plan` / `run_apply`.

### Parser (pest grammar)

In `carina-core/src/parser/carina.pest`:

- Remove the `remote_state` rule.
- Add:
  ```pest
  upstream_state = { "upstream_state" ~ string_literal ~ "{" ~ upstream_state_body ~ "}" }
  upstream_state_body = { attribute* }  // must contain exactly one `source` attribute
  ```

Semantic validation (in `parser/mod.rs`):
- Exactly one `source` attribute, required, of type string.
- Reject any other attribute with a diagnostic.

### Call sites to update

All of the following move from `remote_state` / `RemoteState` / `remote_states` / `remote_bindings` nomenclature to `upstream_state` / `UpstreamState` / `upstream_states`:

- `carina-core/src/parser/mod.rs` — struct + parser
- `carina-core/src/config_loader.rs` — wherever remote_state is picked up
- `carina-core/src/validation.rs` — block validation
- `carina-core/src/resolver.rs` — binding resolution
- `carina-core/src/module_resolver/mod.rs` — module handling
- `carina-cli/src/commands/plan.rs` — `load_remote_states` → `load_upstream_states`
- `carina-cli/src/commands/apply.rs` — same rename
- `carina-cli/src/commands/state.rs` — same rename
- `carina-cli/src/commands/mod.rs` — exports
- `carina-cli/src/wiring.rs` — `create_plan_from_parsed_with_remote` → `...with_upstream`
- `carina-cli/src/tests.rs` / `plan_snapshot_tests.rs` — test fixtures and helpers
- `carina-lsp/src/semantic_tokens.rs` — keyword highlighting: `remote_state` → `upstream_state`
- `carina-lsp/src/completion/top_level.rs` — top-level completion list
- `carina-lsp/src/completion/values.rs` — attribute completion (`source` instead of `backend`)
- `carina-lsp/src/completion/tests/extended.rs` — update expected completions

### Tests

Existing tests to rename/rewrite:

- `carina-cli/tests/fixtures/plan_display/remote_state/main.crn` — rename fixture to `upstream_state/`, update content to new DSL.
- `carina-cli/tests/fixtures/plan_display/deferred_for/main.crn` — replace `remote_state` block.
- `plan_snapshot_remote_state` → `plan_snapshot_upstream_state` + update snapshot.
- Parser unit tests for `RemoteState` variants.
- Wiring tests for binding map construction.

New tests:

- `upstream_state` parse happy path (binding + source).
- `upstream_state` parse: missing `source` → diagnostic.
- `upstream_state` parse: extra attribute → diagnostic.
- `upstream_state` cycle detection: A's `upstream_state` points at B, B's points at A → `AppError::Config` mentioning the cycle.
- `upstream_state` with nonexistent source dir → actionable error.
- `upstream_state` with source dir that has no backend → falls back to local backend at that dir (matches how `run_plan` behaves there).
- `upstream_state` with source dir that has no state yet → clear error message naming the binding.

LSP tests:
- completion suggests `upstream_state` at top level.
- completion suggests `source` inside the block.
- semantic tokens highlight `upstream_state` as keyword.

### Documentation

- Update any `examples/**/*.crn` that use `remote_state`.
- Update READMEs / docs that mention the DSL.

## Edge Cases

| Case | Behavior |
|---|---|
| `source` relative path | Resolved against the enclosing `.crn` file's directory (same convention as `include` / module refs). |
| `source` is a file not a dir | Error: "upstream_state source must be a directory". |
| `source` directory has no `.crn` | Error: "no Carina configuration found in <path>". |
| `source` has a `.crn` but no `backend` block | Use default local backend at `<source>/carina.state.json` (matches top-level default). |
| Upstream state file missing | Error naming the binding and expected state location. |
| Upstream has its own `upstream_state` | Allowed, walked recursively via cycle_guard. |
| Cycle (A→B→A) | Error "upstream_state cycle detected at <path>". |
| Two `upstream_state` blocks with same binding name | Error at parse time (duplicate binding). |
| `source` points outside the repo | Allowed. The loader doesn't care about repo boundaries; filesystem access is the only gate. |

## Out of Scope

- Export value resolution or shape check against upstream `.crn` exports — #1894 will extend this layer to also retrieve `ParsedFile.export_params` from the loaded upstream and feed them into a new delta detector.
- Display of upstream export changes in downstream plans — #1894.

## Rollout

Single PR, single commit history. No migration mode, no deprecation warnings. Memory rule "No backward compatibility required" applies: every `remote_state` occurrence in the repo (code, tests, fixtures, examples, docs) is rewritten in one pass.
