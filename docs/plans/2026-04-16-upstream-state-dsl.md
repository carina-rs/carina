# Plan: Replace `remote_state` with `upstream_state`

Design: `docs/specs/2026-04-16-upstream-state-dsl-design.md`

All tasks below use:
- Test: `cargo test -p <crate> <test_name>` (crate-specific per CLAUDE.md)
- Lint: `cargo clippy -p <crate> --all-targets -- -D warnings`

Each task is a single TDD cycle: red → green → refactor → verify.

Task ordering is dependency-respecting. `task-1` blocks every downstream task because nothing compiles until the grammar and core types are in place.

---

## Task Map

| # | Goal | Primary files |
|---|---|---|
| 1 | Grammar + struct rename (`remote_state` → `upstream_state`), single required `source` attribute, carina-core compiles | `carina-core/src/parser/carina.pest`, `carina-core/src/parser/mod.rs` |
| 2 | Parser: reject missing/extra/wrong-type `source` with diagnostics | `carina-core/src/parser/mod.rs` |
| 3 | Duplicate-binding detection across `upstream_state` blocks | `carina-core/src/parser/mod.rs` |
| 4 | Rewrite `load_remote_states` as `load_upstream_states` using `load_configuration_with_config` + `resolve_backend`; cycle guard | `carina-cli/src/commands/plan.rs` |
| 5 | Wire `load_upstream_states` into `run_plan` and `run_apply` call sites | `carina-cli/src/commands/plan.rs`, `carina-cli/src/commands/apply.rs`, `carina-cli/src/commands/state.rs`, `carina-cli/src/wiring.rs` |
| 6 | Rename `RemoteState` leftovers in carina-core non-parser modules | `carina-core/src/config_loader.rs`, `validation.rs`, `resolver.rs`, `module_resolver/mod.rs` |
| 7 | LSP: completion + semantic tokens for `upstream_state` | `carina-lsp/src/semantic_tokens.rs`, `carina-lsp/src/completion/top_level.rs`, `carina-lsp/src/completion/values.rs`, `carina-lsp/src/completion/tests/extended.rs` |
| 8 | Rewrite plan fixtures and snapshots | `carina-cli/tests/fixtures/plan_display/remote_state/` → `upstream_state/`, `carina-cli/tests/fixtures/plan_display/deferred_for/main.crn`, `carina-cli/src/plan_snapshot_tests.rs` |
| 9 | Examples + docs rewrite | `examples/**/*.crn`, README/doc files with `remote_state` mentions |

Task 1 must land first. Tasks 2, 3 can run in parallel with each other. Tasks 4–6 depend on 1. Task 7, 8, 9 are independent of each other but depend on 1.

---

## Task 1: Grammar and core struct rename

**Goal**: `upstream_state "<name>" { source = "..." }` parses into a new `UpstreamState` struct with `binding: String`, `source: PathBuf`. `remote_state_block`, `RemoteState`, `RemoteStateBackend` are gone.

**Files**:
- `carina-core/src/parser/carina.pest` — modify
- `carina-core/src/parser/mod.rs` — modify

**Test** (`carina-core/src/parser/mod.rs`, test module):

```rust
#[test]
fn parses_upstream_state_block_with_source() {
    let input = r#"
        upstream_state "orgs" {
            source = "../organizations"
        }
    "#;
    let parsed = parse(input).expect("parse should succeed");
    assert_eq!(parsed.upstream_states.len(), 1);
    let us = &parsed.upstream_states[0];
    assert_eq!(us.binding, "orgs");
    assert_eq!(us.source, std::path::PathBuf::from("../organizations"));
}

#[test]
fn remote_state_keyword_is_no_longer_recognized() {
    let input = r#"
        let orgs = remote_state { path = "./foo.json" }
    "#;
    let result = parse(input);
    assert!(result.is_err(), "remote_state must be a parse error now");
}
```

**Implementation**:

1. In `carina.pest`:
   - Delete `remote_state_block` rule (lines ~11-13) and its reference inside `let_binding_rhs` (~line 195).
   - Add top-level statement:
     ```pest
     upstream_state_block = { "upstream_state" ~ string ~ "{" ~ attribute* ~ "}" }
     ```
   - Add `upstream_state_block` to the `statement` rule alternation.

2. In `parser/mod.rs`:
   - Delete `RemoteState` struct (lines 373-393), `RemoteStateBackend` enum and its `impl`.
   - Add:
     ```rust
     #[derive(Debug, Clone)]
     pub struct UpstreamState {
         pub binding: String,
         pub source: std::path::PathBuf,
     }
     ```
   - In `ParsedFile`: replace `pub remote_states: Vec<RemoteState>` with `pub upstream_states: Vec<UpstreamState>`.
   - In parser driver (around lines 808, 1010): replace the `remote_states` accumulator and the let-binding branch that used to recognize `remote_state` as a RHS — `upstream_state` is a top-level statement now, not a let-binding RHS.
   - Delete `_remote_state` placeholder resource emission (lines 881, 914, 959-967).

**Verify**:
```
cargo test -p carina-core parses_upstream_state_block_with_source
cargo test -p carina-core remote_state_keyword_is_no_longer_recognized
cargo check -p carina-core
```

---

## Task 2: Validation diagnostics for `source` attribute

**Goal**: Missing `source`, wrong-type `source`, or unknown attributes in `upstream_state` all produce clear errors at parse time.

**Files**:
- `carina-core/src/parser/mod.rs` — modify

**Test**:

```rust
#[test]
fn upstream_state_missing_source_is_error() {
    let input = r#"upstream_state "orgs" { }"#;
    let err = parse(input).unwrap_err();
    assert!(
        err.to_string().contains("upstream_state") && err.to_string().contains("source"),
        "error should mention upstream_state and source: {}",
        err
    );
}

#[test]
fn upstream_state_source_must_be_string() {
    let input = r#"upstream_state "orgs" { source = 42 }"#;
    let err = parse(input).unwrap_err();
    assert!(err.to_string().contains("source"), "got: {}", err);
}

#[test]
fn upstream_state_unknown_attribute_is_error() {
    let input = r#"
        upstream_state "orgs" {
            source = "../foo"
            backend = "s3"
        }
    "#;
    let err = parse(input).unwrap_err();
    assert!(err.to_string().contains("backend"), "got: {}", err);
}
```

**Implementation**:

In the parser driver for `upstream_state_block` (new code added in Task 1), after collecting attributes:

```rust
let mut source: Option<String> = None;
for (key, value) in attrs {
    match key.as_str() {
        "source" => {
            source = Some(match value {
                Value::String(s) => s,
                _ => return Err(ParseError::new(format!(
                    "upstream_state '{}': 'source' must be a string", binding
                ))),
            });
        }
        other => return Err(ParseError::new(format!(
            "upstream_state '{}': unknown attribute '{}'", binding, other
        ))),
    }
}
let source = source.ok_or_else(|| ParseError::new(format!(
    "upstream_state '{}': 'source' attribute is required", binding
)))?;
```

(Use whatever error type the surrounding parser uses — grep `ParseError::new\|ParseWarning` in `parser/mod.rs` to match style.)

**Verify**:
```
cargo test -p carina-core upstream_state_missing_source_is_error
cargo test -p carina-core upstream_state_source_must_be_string
cargo test -p carina-core upstream_state_unknown_attribute_is_error
```

---

## Task 3: Duplicate binding detection

**Goal**: Two `upstream_state` blocks with the same binding name error at parse time.

**Files**:
- `carina-core/src/parser/mod.rs` — modify

**Test**:

```rust
#[test]
fn upstream_state_duplicate_binding_is_error() {
    let input = r#"
        upstream_state "orgs" { source = "../a" }
        upstream_state "orgs" { source = "../b" }
    "#;
    let err = parse(input).unwrap_err();
    assert!(
        err.to_string().contains("orgs") && err.to_string().contains("duplicate"),
        "got: {}", err
    );
}
```

**Implementation**: after collecting all `upstream_states` at the end of the parser driver (around line 1010 post-Task-1), validate:

```rust
let mut seen = std::collections::HashSet::new();
for us in &upstream_states {
    if !seen.insert(us.binding.clone()) {
        return Err(ParseError::new(format!(
            "duplicate upstream_state binding: '{}'", us.binding
        )));
    }
}
```

**Verify**:
```
cargo test -p carina-core upstream_state_duplicate_binding_is_error
```

---

## Task 4: `load_upstream_states` with cycle guard

**Goal**: Replace `load_remote_states` with a function that resolves the source directory, loads its `ParsedFile`, derives its backend, reads the state file, and builds the binding map. Detects cycles via a `HashSet<PathBuf>`.

**Files**:
- `carina-cli/src/commands/plan.rs` — modify (replace `load_remote_states` and both local/S3 helpers)

**Tests** (add to the tests module in `plan.rs` or `tests.rs`):

```rust
#[tokio::test]
async fn load_upstream_states_reads_exports_from_source_backend() {
    // Arrange: create a temp dir with a minimal .crn and local state
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.crn"),
        r#"
            backend "local" {}
            exports { account_id = "123" }
        "#,
    ).unwrap();
    // Pre-seed state file with exports
    let state_path = dir.path().join("carina.state.json");
    let state = StateFile {
        exports: [("account_id".to_string(), serde_json::json!("123"))]
            .into_iter().collect(),
        ..Default::default()
    };
    std::fs::write(&state_path, serde_json::to_string(&state).unwrap()).unwrap();

    let upstream_states = vec![UpstreamState {
        binding: "orgs".to_string(),
        source: dir.path().to_path_buf(),
    }];

    let result = load_upstream_states(
        &upstream_states,
        dir.path().parent().unwrap(),
        &ProviderContext::default(),
        &mut HashSet::new(),
    ).await.unwrap();

    assert_eq!(result["orgs"]["account_id"], Value::String("123".to_string()));
}

#[tokio::test]
async fn load_upstream_states_errors_on_cycle() {
    // A → B → A
    let tmp = tempfile::tempdir().unwrap();
    let dir_a = tmp.path().join("a");
    let dir_b = tmp.path().join("b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_a.join("main.crn"), r#"upstream_state "b" { source = "../b" }"#).unwrap();
    std::fs::write(dir_b.join("main.crn"), r#"upstream_state "a" { source = "../a" }"#).unwrap();

    let upstream_states = vec![UpstreamState {
        binding: "b".to_string(),
        source: dir_b.clone(),
    }];

    let mut guard = HashSet::new();
    guard.insert(dir_a.canonicalize().unwrap());

    let err = load_upstream_states(
        &upstream_states,
        &dir_a,
        &ProviderContext::default(),
        &mut guard,
    ).await.unwrap_err();
    assert!(err.to_string().contains("cycle"), "got: {}", err);
}

#[tokio::test]
async fn load_upstream_states_errors_when_source_missing() {
    let upstream_states = vec![UpstreamState {
        binding: "orgs".to_string(),
        source: "/nonexistent/path".into(),
    }];
    let err = load_upstream_states(
        &upstream_states,
        std::path::Path::new("/"),
        &ProviderContext::default(),
        &mut HashSet::new(),
    ).await.unwrap_err();
    assert!(err.to_string().contains("orgs"), "error should name the binding: {}", err);
}
```

**Implementation**:

```rust
pub(crate) async fn load_upstream_states(
    upstream_states: &[UpstreamState],
    base_dir: &std::path::Path,
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<std::path::PathBuf>,
) -> Result<HashMap<String, HashMap<String, Value>>, AppError> {
    let mut result = HashMap::new();
    for us in upstream_states {
        let source_abs = base_dir.join(&us.source).canonicalize().map_err(|e| {
            AppError::Config(format!(
                "upstream_state '{}': cannot resolve source '{}': {}",
                us.binding, us.source.display(), e
            ))
        })?;
        if !cycle_guard.insert(source_abs.clone()) {
            return Err(AppError::Config(format!(
                "upstream_state '{}': cycle detected at {}",
                us.binding, source_abs.display()
            )));
        }
        let loaded = load_configuration_with_config(&source_abs, provider_context)
            .map_err(|e| AppError::Config(format!(
                "upstream_state '{}': {}", us.binding, e
            )))?;
        let backend = crate::commands::resolve_backend(loaded.parsed.backend.as_ref())
            .await
            .map_err(AppError::Backend)?;
        let state_file = backend.read_state().await
            .map_err(AppError::Backend)?
            .ok_or_else(|| AppError::Config(format!(
                "upstream_state '{}': no state at {}",
                us.binding, source_abs.display()
            )))?;
        result.insert(us.binding.clone(), state_file.build_remote_bindings());
        cycle_guard.remove(&source_abs);
    }
    Ok(result)
}
```

Delete old helpers: `load_remote_state_local`, `load_remote_state_s3`.

**Verify**:
```
cargo test -p carina-cli load_upstream_states
cargo clippy -p carina-cli --all-targets -- -D warnings
```

---

## Task 5: Wire `load_upstream_states` into run_plan / run_apply / state

**Goal**: All call sites of the old `load_remote_states` now call `load_upstream_states` and thread a `cycle_guard`. `parsed.remote_states` → `parsed.upstream_states` everywhere.

**Files**:
- `carina-cli/src/commands/plan.rs` — line 226 and nearby: swap call
- `carina-cli/src/commands/apply.rs` — same
- `carina-cli/src/commands/state.rs` — same
- `carina-cli/src/wiring.rs` — `create_plan_from_parsed_with_remote` → `create_plan_from_parsed_with_upstream`; field/parameter renames

**Test**: integration test driven through an existing test fixture. Add:

```rust
#[tokio::test]
async fn run_plan_resolves_upstream_state_exports() {
    // Arrange: two temp dirs, A with exports, B with upstream_state pointing at A
    let tmp = tempfile::tempdir().unwrap();
    let dir_a = tmp.path().join("a");
    let dir_b = tmp.path().join("b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_a.join("main.crn"), r#"
        backend "local" {}
        exports { region = "ap-northeast-1" }
    "#).unwrap();
    // Seed A's state so exports can be read
    std::fs::write(dir_a.join("carina.state.json"),
        serde_json::to_string(&StateFile {
            exports: [("region".to_string(), serde_json::json!("ap-northeast-1"))]
                .into_iter().collect(),
            ..Default::default()
        }).unwrap()
    ).unwrap();
    std::fs::write(dir_b.join("main.crn"), r#"
        upstream_state "a" { source = "../a" }
    "#).unwrap();

    // Act
    let result = crate::commands::plan::run_plan(
        &dir_b,
        &ProviderContext::default(),
        PlanOptions::default(),
    ).await;

    // Assert: succeeds, no "not found" error
    assert!(result.is_ok(), "run_plan failed: {:?}", result);
}
```

**Implementation**:

In `plan.rs` ~line 226:
```rust
let remote_bindings = load_remote_states(&parsed.remote_states, base_dir).await?;
```
becomes:
```rust
let mut cycle_guard = HashSet::new();
if let Ok(abs) = base_dir.canonicalize() {
    cycle_guard.insert(abs);
}
let remote_bindings = load_upstream_states(
    &parsed.upstream_states,
    base_dir,
    provider_context,
    &mut cycle_guard,
).await?;
```

Same substitution in `apply.rs` and `state.rs`. In `wiring.rs`, rename function and its `remote_bindings` parameter is unchanged (it remains the flat `HashMap<String, HashMap<String, Value>>`).

**Verify**:
```
cargo test -p carina-cli run_plan_resolves_upstream_state_exports
cargo test -p carina-cli   # full carina-cli test suite passes
cargo clippy --all-targets -- -D warnings
```

---

## Task 6: Rename leftovers in carina-core non-parser modules

**Goal**: Every `remote_state`, `RemoteState`, `remote_states` identifier in carina-core is either deleted or renamed. `cargo check -p carina-core` compiles clean.

**Files**:
- `carina-core/src/config_loader.rs`
- `carina-core/src/validation.rs`
- `carina-core/src/resolver.rs`
- `carina-core/src/module_resolver/mod.rs`

**Test**: compile-driven. Before editing, run:
```
grep -rn "remote_state\|RemoteState" carina-core/src
```
After editing, the grep must return empty.

**Implementation**: mechanical rename. For each file:
- `remote_state` → `upstream_state` (in identifiers, strings, comments)
- `RemoteState` struct references → `UpstreamState`
- `remote_states` Vec/field → `upstream_states`
- `remote_bindings` HashMap → keep this name (it's the resolved binding map, orthogonal to the DSL keyword)

**Verify**:
```
cargo check -p carina-core
cargo test -p carina-core
! grep -rn 'remote_state\|RemoteState' carina-core/src
```

---

## Task 7: LSP updates

**Goal**: LSP recognizes `upstream_state` as a top-level keyword, highlights it, and completes `source` inside the block.

**Files**:
- `carina-lsp/src/semantic_tokens.rs` — `tokenize_line` keyword list
- `carina-lsp/src/completion/top_level.rs` — top-level completions
- `carina-lsp/src/completion/values.rs` — attribute completions for `upstream_state` block
- `carina-lsp/src/completion/tests/extended.rs` — update test expectations

**Test** (in `carina-lsp/src/completion/tests/extended.rs`):

```rust
#[test]
fn top_level_completion_suggests_upstream_state() {
    let completions = top_level_completions("");
    let labels: Vec<_> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(labels.contains(&"upstream_state"), "got: {:?}", labels);
    assert!(!labels.contains(&"remote_state"));
}

#[test]
fn upstream_state_block_completes_source_attribute() {
    let completions = attribute_completions_for_type("upstream_state");
    let labels: Vec<_> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(labels.contains(&"source"));
}
```

Add a semantic-tokens test:

```rust
#[test]
fn upstream_state_keyword_is_highlighted() {
    let tokens = tokenize_line(r#"upstream_state "orgs" {"#);
    // Expect the first token to be classified as keyword
    assert!(tokens.first().map(|t| t.is_keyword()).unwrap_or(false));
}
```

**Implementation**:
- In `top_level.rs`: find the list of keywords (grep for `"remote_state"` in that file), replace with `"upstream_state"`, drop any `remote_state` entry.
- In `values.rs`: add a branch for `"upstream_state"` block context returning `[source]` as the only attribute. Remove any old `remote_state` branch.
- In `semantic_tokens.rs`: the keyword list at line start — replace `remote_state` with `upstream_state`.

**Verify**:
```
cargo test -p carina-lsp top_level_completion_suggests_upstream_state
cargo test -p carina-lsp upstream_state_block_completes_source_attribute
cargo test -p carina-lsp upstream_state_keyword_is_highlighted
cargo test -p carina-lsp
```

---

## Task 8: Plan fixtures and snapshots

**Goal**: All plan-display fixtures and snapshot tests reflect the new DSL. `cargo test -p carina-cli plan_snapshot` passes.

**Files**:
- Rename `carina-cli/tests/fixtures/plan_display/remote_state/` → `upstream_state/` (use `git mv`)
- Rewrite `carina-cli/tests/fixtures/plan_display/upstream_state/main.crn` — replace `remote_state` block with `upstream_state "<name>" { source = "..." }` form; may need to add a source-dir fixture sibling.
- `carina-cli/tests/fixtures/plan_display/deferred_for/main.crn` — same rewrite
- `carina-cli/src/plan_snapshot_tests.rs` — rename `plan_snapshot_remote_state` → `plan_snapshot_upstream_state`; register the new fixture path
- `carina-cli/src/snapshots/*remote_state*.snap` — delete and regenerate via `cargo insta accept`
- `carina-cli/Makefile` if there's a fixture target (check with grep): update path

**Test**: the snapshot test itself.

**Implementation**:
1. `git mv carina-cli/tests/fixtures/plan_display/remote_state carina-cli/tests/fixtures/plan_display/upstream_state`
2. Edit `main.crn` inside the renamed dir to use new DSL. Create sibling fixture dir if the `source` needs a real upstream:
   ```
   carina-cli/tests/fixtures/plan_display/upstream_state/
     main.crn                    # uses `upstream_state "orgs" { source = "./upstream" }`
     upstream/
       main.crn                  # backend + exports
       carina.state.json         # pre-seeded with exports
   ```
3. Update `plan_snapshot_tests.rs`:
   ```rust
   snapshot_test!(upstream_state, "upstream_state");
   ```
   (or whatever the helper pattern is — match the file's style).
4. Run `cargo insta test --accept` after manually verifying the new snapshot looks right.

**Verify**:
```
cargo test -p carina-cli plan_snapshot
cargo insta review   # confirm no pending, or review new snap
make plan-fixtures   # if Makefile was touched
```

---

## Task 9: Examples and docs

**Goal**: No occurrence of `remote_state` remains in `.crn` examples, README, or other docs.

**Files**:
- `examples/**/*.crn` (grep first)
- Any `README.md`, `docs/**/*.md` that mentions `remote_state`

**Test**: final grep invariant.

```
! grep -rn 'remote_state' examples docs README.md 2>/dev/null
```

**Implementation**: mechanical. For each hit:
- `.crn` files: rewrite to `upstream_state "<binding>" { source = "./<dir>" }`, create the `./<dir>` with a minimal upstream if needed for the example to be runnable, or add a comment explaining the upstream isn't included.
- Markdown docs: update prose and code blocks.

**Verify**:
```
cargo test   # full workspace — all still green
cargo run -- validate examples/<each-example>/ 2>&1  # for examples that previously worked
! grep -rn 'remote_state' examples docs 2>/dev/null
! grep -rn 'remote_state' README.md CLAUDE.md 2>/dev/null
```

---

## Post-plan self-review

- Every requirement in the design doc is covered: grammar (1), validation (2-3), loader (4), integration (5), core rename (6), LSP (7), tests/fixtures (8), examples (9).
- No placeholders like "similar to previous", "etc.", "add necessary" — every task has concrete code or commands.
- Dependencies: Task 1 blocks all others. 2-3 can go parallel but after 1. 4 after 1. 5 after 1+4. 6 after 1. 7, 8, 9 after 1.
- Task 6 is compile-driven (tests green via `cargo check`), which is fine because Tasks 2, 3, 4 carry the observable behavior tests that this rename must preserve.
- State file format unchanged; no migration task needed.
