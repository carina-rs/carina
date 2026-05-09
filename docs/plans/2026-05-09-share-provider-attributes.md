# Implementation plan: shareable provider attributes

<!-- derived-from ../specs/2026-05-09-share-provider-attributes-design.md -->

Issue: [#2717](https://github.com/carina-rs/carina/issues/2717)

Spec: `docs/specs/2026-05-09-share-provider-attributes-design.md`

This plan decomposes the design into 7 TDD-sized tasks. Each task is
independently verifiable and produces a single failing test before
implementation. Tasks are ordered so each builds on the previous one.

The work splits cleanly into two PRs:

- **PR 1 — language change** (Tasks 1–6): make `default_tags`
  accept resolved `let`-binding references in `carina-core`.
- **PR 2 — LSP follow-through** (Task 7): bring `carina-lsp`
  diagnostics in line so editor warnings stay in parity with CLI
  validate.

Both PRs need to land before the rule
"validate and LSP warnings must reach parity"
(memory) is satisfied; the issue is closed only after Task 7 ships.

## Files

### Modified
- `carina-core/src/parser/ast.rs` — `ProviderConfig` gains a transient
  field for unresolved well-known attributes.
- `carina-core/src/parser/blocks/provider.rs` — stops peeling
  `default_tags` at parse time; collects it into the new transient
  field instead.
- `carina-core/src/parser/resolve.rs` — new post-resolver step
  `finalize_provider_configs`, plus an extension to
  `accumulate_undefined_reference_errors` that visits provider
  attribute values.
- `carina-core/src/parser/tests.rs` — existing
  `parse_provider_block_with_default_tags`,
  `parse_provider_block_without_default_tags`,
  `provider_config_default_tags_preserve_insertion_order` may need to
  be invoked through the post-resolver path. New tests cover the
  reference-form input.
- `carina-cli/tests/fixtures/` — new multi-file fixture
  `share_provider_attrs/` with `providers.crn` + `modules/standard-tags/`.
- `carina-cli/tests/` — integration test that loads the fixture and
  asserts the resolved `default_tags`.
- `carina-lsp/src/diagnostics/mod.rs` (or sibling) — keep
  diagnostics in parity with CLI after the peel moves.

### Created
- `carina-cli/tests/fixtures/share_provider_attrs/component/providers.crn`
- `carina-cli/tests/fixtures/share_provider_attrs/component/main.crn`
- `carina-cli/tests/fixtures/share_provider_attrs/modules/standard-tags/main.crn`

## Verification commands

Per CLAUDE.md verify protocol — crate-scoped first, workspace before PR:

```bash
cargo check -p carina-core
cargo nextest run -p carina-core
cargo nextest run -p carina-cli
cargo nextest run -p carina-lsp        # only for Task 7
cargo test --workspace --doc           # before PR
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-*.sh
```

---

## Task 1 — Add transient `unresolved_attributes` field to `ProviderConfig`

### Goal
Give `ProviderConfig` a place to carry un-peeled attribute values
through the resolver. This is the type-level prerequisite for moving
the peel; no behaviour change yet.

### Files
- `carina-core/src/parser/ast.rs`

### Test
Add a unit test asserting that the field exists, defaults to empty,
and survives `Clone`/`Debug`/`PartialEq` (the existing derives on
`ProviderConfig`):

```rust
// carina-core/src/parser/tests.rs
#[test]
fn provider_config_carries_unresolved_attributes_field() {
    use crate::parser::ast::ProviderConfig;
    use indexmap::IndexMap;

    let pc = ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
    };
    assert!(pc.unresolved_attributes.is_empty());
}
```

### Implementation
In `carina-core/src/parser/ast.rs`, add the field to `ProviderConfig`:

```rust
pub struct ProviderConfig {
    pub name: String,
    pub attributes: IndexMap<String, Value>,
    pub default_tags: IndexMap<String, Value>,
    pub source: Option<String>,
    pub version: Option<VersionConstraint>,
    pub revision: Option<String>,

    /// Well-known attributes whose values were not literal at parse
    /// time (e.g. `default_tags = some_let.field`). Populated by the
    /// parser; drained and validated by `finalize_provider_configs`
    /// after the resolver pass. Always empty after finalization.
    pub unresolved_attributes: IndexMap<String, Value>,
}
```

`parse_provider_block` continues to populate `unresolved_attributes`
with `IndexMap::new()` for now (no peel change yet); other
construction sites in tests get the same `IndexMap::new()`.

### Verification
```bash
cargo nextest run -p carina-core provider_config_carries_unresolved_attributes_field
```
Expected: passes after the field is added.

---

## Task 2 — Parse defers `default_tags` when its value is not a literal map

### Goal
Stop the silent-empty fallback. When `parse_provider_block` sees
`default_tags = some_reference`, route the value to
`unresolved_attributes` instead of dropping it on the floor. Literal
`default_tags = { ... }` continues to populate
`ProviderConfig.default_tags` directly so existing tests keep passing.

### Files
- `carina-core/src/parser/blocks/provider.rs`

### Test
```rust
// carina-core/src/parser/tests.rs
#[test]
fn parse_provider_block_defers_non_literal_default_tags() {
    let input = r#"
        let shared = { Env = 'dev', Team = 'infra' }

        provider awscc {
          source       = 'github.com/carina-rs/carina-provider-awscc'
          revision     = 'main'
          default_tags = shared
        }
    "#;

    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let pc = &parsed.providers[0];

    // Not silently dropped: held in unresolved_attributes for the
    // resolver to finish.
    assert!(pc.default_tags.is_empty(), "literal-only field stays empty");
    assert!(
        pc.unresolved_attributes.contains_key("default_tags"),
        "non-literal default_tags must be deferred, not dropped"
    );
}
```

### Implementation
In `carina-core/src/parser/blocks/provider.rs`, replace the
silent-empty fallback:

```rust
let mut unresolved_attributes: IndexMap<String, Value> = IndexMap::new();

let default_tags = match attributes.shift_remove("default_tags") {
    Some(Value::Map(tags)) => tags,
    Some(other) => {
        unresolved_attributes.insert("default_tags".to_string(), other);
        IndexMap::new()
    }
    None => IndexMap::new(),
};
```

Pass `unresolved_attributes` into the constructed `ProviderConfig`.
Existing `default_tags = { ... }` literal tests keep passing because
the `Value::Map` arm is unchanged.

### Verification
```bash
cargo nextest run -p carina-core parse_provider_block_defers_non_literal_default_tags
cargo nextest run -p carina-core parse_provider_block_with_default_tags
cargo nextest run -p carina-core parse_provider_block_without_default_tags
cargo nextest run -p carina-core provider_config_default_tags_preserve_insertion_order
```
Expected: new test passes, existing literal-form tests keep passing.

---

## Task 3 — Identifier-scope check visits provider attribute values

### Goal
A typo in a `let`-reference inside a provider block must surface as
`UndefinedIdentifier`, not a silent runtime miss. Today
`accumulate_undefined_reference_errors` walks resources, attribute
params, module-call args, and export params; provider attributes are
not in the list.

### Files
- `carina-core/src/parser/resolve.rs`

### Test
```rust
// carina-core/src/parser/tests.rs
#[test]
fn provider_block_undefined_let_reference_flagged() {
    let input = r#"
        provider awscc {
          source       = 'github.com/carina-rs/carina-provider-awscc'
          revision     = 'main'
          default_tags = nonexistent_binding
        }
    "#;

    let parsed = parse(input, &ProviderContext::default()).unwrap();
    let errs = check_identifier_scope(&parsed);
    assert!(
        errs.iter().any(|e| format!("{e}").contains("nonexistent_binding")),
        "expected UndefinedIdentifier for `nonexistent_binding`, got: {errs:?}"
    );
}
```

### Implementation
In `accumulate_undefined_reference_errors`
(`carina-core/src/parser/resolve.rs:296`), add a pass over provider
configs:

```rust
for provider in &parsed.providers {
    for value in provider.attributes.values() {
        check(value);
    }
    for value in provider.unresolved_attributes.values() {
        check(value);
    }
}
```

The closure `check` already calls `value.visit_refs` and uses the
shared `known` binding set, so this is a one-loop addition.

### Verification
```bash
cargo nextest run -p carina-core provider_block_undefined_let_reference_flagged
cargo nextest run -p carina-core check_identifier_scope
```
Expected: new test passes; existing identifier-scope tests stay
green.

---

## Task 4 — `finalize_provider_configs` resolves deferred provider attributes

### Goal
After the resolver substitutes `Value::ResourceRef`s with concrete
values, walk every `ProviderConfig`, drain
`unresolved_attributes["default_tags"]`, validate it as a map of
strings, and write it into `ProviderConfig.default_tags`. Emit a
typed error if the resolved value is not a map.

### Files
- `carina-core/src/parser/resolve.rs`
- `carina-core/src/parser/error.rs` (if a new error variant is the
  cleanest fit; otherwise reuse `InvalidExpression`)

### Test
```rust
// carina-core/src/parser/tests.rs
#[test]
fn finalize_provider_configs_promotes_resolved_default_tags() {
    let input = r#"
        let shared = { Env = 'dev', Team = 'infra' }

        provider awscc {
          source       = 'github.com/carina-rs/carina-provider-awscc'
          revision     = 'main'
          default_tags = shared
        }
    "#;

    // Full pipeline: parse -> resolve refs -> finalize providers.
    let parsed = parse_and_resolve(input).unwrap();

    let pc = &parsed.providers[0];
    assert!(pc.unresolved_attributes.is_empty(),
            "finalize must drain unresolved_attributes");
    assert_eq!(pc.default_tags.len(), 2);
    assert_eq!(
        pc.default_tags.get("Env"),
        Some(&Value::String("dev".to_string())),
    );
    assert_eq!(
        pc.default_tags.get("Team"),
        Some(&Value::String("infra".to_string())),
    );
}

#[test]
fn finalize_provider_configs_rejects_non_map_default_tags() {
    let input = r#"
        let bad = "not a map"

        provider awscc {
          source       = 'github.com/carina-rs/carina-provider-awscc'
          revision     = 'main'
          default_tags = bad
        }
    "#;

    let err = parse_and_resolve(input).unwrap_err();
    assert!(format!("{err}").contains("default_tags"),
            "error must mention default_tags; got: {err}");
}
```

(`parse_and_resolve` is the existing or new helper that runs parse +
resolver + finalize. If it doesn't exist, expose a thin wrapper in
`carina-core/src/parser/mod.rs` that the tests can call.)

### Implementation
In `carina-core/src/parser/resolve.rs`, add:

```rust
pub fn finalize_provider_configs(parsed: &mut ParsedFile) -> Result<(), ParseError> {
    for provider in parsed.providers.iter_mut() {
        if let Some(value) = provider.unresolved_attributes.shift_remove("default_tags") {
            match value {
                Value::Map(tags) => {
                    provider.default_tags = tags;
                }
                other => {
                    return Err(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "Provider '{}': default_tags must resolve to a map, got {other:?}",
                            provider.name
                        ),
                    });
                }
            }
        }
        // Future: source / version / revision peel land here.
        debug_assert!(
            provider.unresolved_attributes.is_empty(),
            "unresolved_attributes must be drained by finalize",
        );
    }
    Ok(())
}
```

Wire `finalize_provider_configs` into the existing post-parse pipeline
(the place where `check_identifier_scope` and friends are already
invoked — typically `carina-core/src/parser/mod.rs::parse` or the CLI
load path).

### Verification
```bash
cargo nextest run -p carina-core finalize_provider_configs_promotes_resolved_default_tags
cargo nextest run -p carina-core finalize_provider_configs_rejects_non_map_default_tags
cargo nextest run -p carina-core             # full crate
```
Expected: both new tests pass; the full carina-core suite stays green.

---

## Task 5 — Multi-file fixture for shared `default_tags`

### Goal
Codify the directory-scoped acceptance shape from the design doc as a
fixture, and add an integration test that drives the same path
`carina validate` would.

### Files
- `carina-cli/tests/fixtures/share_provider_attrs/component/providers.crn` (new)
- `carina-cli/tests/fixtures/share_provider_attrs/component/main.crn` (new)
- `carina-cli/tests/fixtures/share_provider_attrs/modules/standard-tags/main.crn` (new)
- `carina-cli/tests/share_provider_attrs.rs` (new) — integration test

### Test
Fixture content:

`modules/standard-tags/main.crn`:
```crn
arguments {
  environment: string
  component:   string
}

exports {
  tags = {
    ManagedBy   = 'carina'
    Project     = 'carina-rs'
    Repository  = 'carina-rs/infra'
    Environment = environment
    Component   = component
  }
}
```

`component/providers.crn`:
```crn
let st = use { source = '../modules/standard-tags' }

let tags = st {
  environment = 'dev'
  component   = 'registry'
}

provider mock {
  source       = 'github.com/carina-rs/carina-provider-mock'
  version      = '~0.1.0'
  default_tags = tags.tags
}
```

`component/main.crn`: a single mock resource so the directory parses
as a complete component (use the existing mock-resource shape from
other fixtures, e.g. `version_constraint`).

Integration test:

```rust
// carina-cli/tests/share_provider_attrs.rs
use std::path::PathBuf;

#[test]
fn share_provider_attrs_resolves_default_tags() {
    let fixture: PathBuf = ["tests", "fixtures", "share_provider_attrs", "component"]
        .iter()
        .collect();

    // Loads the directory through the same path the CLI uses.
    let parsed = carina_core::load_directory(&fixture).expect("parse + resolve");
    let provider = parsed
        .providers
        .iter()
        .find(|p| p.name == "mock")
        .expect("provider mock present");

    let tags = &provider.default_tags;
    assert_eq!(tags.get("ManagedBy"), Some(&carina_core::Value::String("carina".into())));
    assert_eq!(tags.get("Environment"), Some(&carina_core::Value::String("dev".into())));
    assert_eq!(tags.get("Component"),   Some(&carina_core::Value::String("registry".into())));
}
```

(If `carina_core::load_directory` is not the public entry point, swap
to whatever `carina-cli` uses today — `ModuleResolver::load_module` or
similar. The test must run the *full* directory-scoped pipeline, not
a single-string parse.)

### Implementation
Drop the three fixture files; write the integration test against the
public entry point currently used by `carina validate`.

### Verification
```bash
cargo nextest run -p carina-cli share_provider_attrs_resolves_default_tags
```
Expected: passes — and proves end-to-end that directory-scoped resolve
+ finalize produces the exact tag map we want.

---

## Task 6 — Real-infra smoke verification

### Goal
The design doc requires a real-infra smoke test. Confirm `carina
validate` on a `carina-rs/infra` component using a shared
`standard-tags` module produces the same effective tags as a literal
map.

### Files
- None checked into this repo.
- A throwaway scratch directory under `carina-rs/infra` (the user's
  working copy) with a `standard-tags/` module and one component
  using it via `let st = use { ... }` + `default_tags = tags.tags`.

### Test
Run sequentially against the user's `carina-rs/infra` working copy:

```bash
# Build the worktree's binary (no cargo install side effects)
cargo build -p carina-cli

CARINA=$(pwd)/target/debug/carina

# Baseline: literal default_tags variant — capture the rendered plan
aws-vault exec mizzy -- "$CARINA" plan /path/to/literal-variant > /tmp/plan-literal.txt

# New shape: same directory rewritten to use the shared module
aws-vault exec mizzy -- "$CARINA" plan /path/to/shared-variant > /tmp/plan-shared.txt

# Effective tags must match
diff /tmp/plan-literal.txt /tmp/plan-shared.txt
```

Acceptance: tag-bearing diff lines (`ManagedBy`, `Project`,
`Repository`, `Environment`, `Component`) are identical between
literal and shared variants.

### Implementation
Run the steps above. Capture the output in a comment on the issue or
PR description so reviewers can see the smoke result. No code change
in this repo.

### Verification
- Both `plan` invocations exit `0`.
- `diff` shows no difference in the rendered tag set.

---

## Task 7 — LSP diagnostics parity for provider attributes

### Goal
When the user writes `default_tags = nonexistent_binding` in a
provider block, the editor must show the same `UndefinedIdentifier`
warning that `carina validate` shows. Same for "resolved value is not
a map." Today LSP diagnostics drive off the same parse pipeline as
the CLI; after Task 4 the validation site moved to a post-resolver
pass, so the LSP entry point needs to invoke that pass too.

### Files
- `carina-lsp/src/diagnostics/mod.rs` (or sibling — wherever the LSP
  invokes `parse`)
- `carina-lsp/src/diagnostics/checks.rs` (if module-loading interacts
  with finalize)

### Test
```rust
// carina-lsp/src/diagnostics/tests.rs (or the existing diagnostics
// test module)
#[test]
fn lsp_flags_undefined_let_reference_in_provider_block() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("providers.crn"),
        r#"
        provider awscc {
          source       = 'github.com/carina-rs/carina-provider-awscc'
          revision     = 'main'
          default_tags = nonexistent_binding
        }
        "#,
    ).unwrap();

    let diags = run_diagnostics(dir.path());
    assert!(
        diags.iter().any(|d| d.message.contains("nonexistent_binding")),
        "LSP must surface UndefinedIdentifier for nonexistent_binding; got: {diags:?}"
    );
}

#[test]
fn lsp_flags_non_map_default_tags() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("providers.crn"),
        r#"
        let bad = "not a map"

        provider awscc {
          source       = 'github.com/carina-rs/carina-provider-awscc'
          revision     = 'main'
          default_tags = bad
        }
        "#,
    ).unwrap();

    let diags = run_diagnostics(dir.path());
    assert!(
        diags.iter().any(|d| d.message.contains("default_tags")),
        "LSP must surface non-map default_tags error; got: {diags:?}"
    );
}
```

`run_diagnostics` is the existing test helper (or a thin wrapper over
the public diagnostics entry point). If it doesn't yet support
multi-file directories, extend it minimally.

### Implementation
Audit the LSP entry that today produces parser diagnostics — typically
`carina-lsp/src/diagnostics/mod.rs` in the `analyze`/`diagnose`
function — and ensure it calls the same post-parse pipeline used by
the CLI: `parse` → resolve → `check_identifier_scope` →
`finalize_provider_configs`. If the LSP has its own slimmer pipeline,
add the new step there too.

`finalize_provider_configs` returns a `ParseError`; the LSP already
has machinery for converting `ParseError` to a `Diagnostic` — use it.

### Verification
```bash
cargo nextest run -p carina-lsp lsp_flags_undefined_let_reference_in_provider_block
cargo nextest run -p carina-lsp lsp_flags_non_map_default_tags
cargo nextest run -p carina-lsp                    # full crate
cargo test --workspace --doc
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-*.sh
```
Expected: both new LSP tests pass; full carina-lsp suite stays green;
workspace doctests, clippy, and repo invariant scripts are clean.

---

## Self-review checklist

- [x] Every task has a concrete failing test before implementation.
- [x] No "implement similar to Task N" — each task's
      Implementation block names the exact file lines / function /
      structural change.
- [x] Each task is independently verifiable with a runnable command.
- [x] Task order respects dependencies: AST field (1) → parser
      defers (2) → identifier scope (3) → finalize (4) → fixture (5)
      → real infra (6) → LSP (7).
- [x] Existing tests (`parse_provider_block_with_default_tags`,
      `parse_provider_block_without_default_tags`,
      `provider_config_default_tags_preserve_insertion_order`) are
      either preserved or explicitly migrated; the design's "user-
      facing diagnostic doesn't regress" contract is upheld in Task 4.
- [x] Multi-file fixture covers the directory-scoped invariant from
      CLAUDE.md.
- [x] LSP/CLI parity (memory rule) is reached before the issue is
      closed.
- [x] `merge` DSL function and `source`/`version`/`revision`
      reference-form support are *out of scope* per the design doc;
      not in any task.
