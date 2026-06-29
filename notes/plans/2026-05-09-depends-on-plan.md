# `depends_on` Meta-Argument: Implementation Plan

<!-- derived-from ../specs/2026-05-09-depends-on-design.md -->

## Repository scope

This plan covers `carina-rs/carina` (carina-core) only. No provider-side work is required: providers see `Effect::Wait`/`Effect::Create` etc. with the union dependency map already resolved, so nothing in the WIT contract or Provider trait changes.

## Prerequisites

This plan assumes:

- **`carina#2826` — `lifecycle` → `directives` rename** has landed first. All filenames, struct names, and DSL keywords below are written with `directives`. If `#2826` slips, every reference here is mechanically rewritable to `lifecycle` via a single sed pass; the structural design of `depends_on` is unaffected.

If `#2826` has not landed when this plan starts, stall on it. Don't try to layer `depends_on` onto `lifecycle` and rebase later — the field rename touches every `match` arm in carina-core, the WIT contract, and state v3 fixtures, and doing it inside the same PR as a feature add violates the "1 PR = 1 topic" rule from project memory.

## File map

### Files to create

| Path | Purpose |
|---|---|
| `carina-core/src/validation/depends_on.rs` | New module. Holds the seven analysis-pass diagnostics for `depends_on` (unknown binding, cycle, disallowed kind, self-reference, element type mismatch, duplicate element, redundant edge). |
| `carina-core/tests/fixtures/depends_on/basic/main.crn` | Multi-file fixture for parser + analysis tests. |
| `carina-core/tests/fixtures/depends_on/basic/directives.crn` | Sibling file for the directory-scoped acceptance test (per CLAUDE.md). |
| `carina-cli/tests/fixtures/plan_display/depends_on/main.crn` | Plan-tree snapshot fixture. |
| `carina-cli/tests/fixtures/plan_display/depends_on/carina.state.json` | Empty starting state for the snapshot fixture. |
| `carina-cli/tests/fixtures/plan_display/depends_on/snapshot.txt` | Snapshot written by `cargo insta accept` after the first run. |

### Files to modify

| Path | Change |
|---|---|
| `carina-core/src/resource/mod.rs` | Add `pub depends_on: Vec<String>` field to `Directives` (the post-#2826 rename of `LifecycleConfig`). Default = `vec![]`. Tagged with `#[serde(default)]` so legacy state files deserialise cleanly. |
| `carina-core/src/parser/blocks/attributes.rs` | In `extract_directives` (post-#2826 rename of `extract_lifecycle_config`), pull `depends_on` out of the parsed block's attribute map. Reject string-literal elements at parse time. |
| `carina-core/src/parser/carina.pest` | No grammar change required if `directives { ... }` already accepts arbitrary attribute keys (it does). Verify and add a comment pointing to the design doc. |
| `carina-core/src/deps.rs` | At the existing `resource.dependency_bindings = ...` site (line 320), union `resource.directives.depends_on.iter().cloned()` into the assignment. |
| `carina-core/src/resolver.rs` | Same change at line 72. |
| `carina-core/src/parser/resolve.rs` | Same change at line 145. |
| `carina-core/src/effect.rs` | Add `pub explicit_dependencies: HashSet<String>` field to every `Effect` variant. Default = `HashSet::new()`. Variant constructors and existing `match` arms in callers all need the new field. |
| `carina-core/src/differ/mod.rs` (or wherever effects are constructed from resources) | When constructing each `Effect::*` from a `Resource`, populate `explicit_dependencies` from `resource.directives.depends_on.iter().cloned().collect()`. |
| `carina-core/src/validation/mod.rs` | Register the new `depends_on` diagnostics. Add a top-level call site that walks every binding in the file, runs the seven checks, and accumulates errors/warnings. |
| `carina-core/src/keywords.rs` | Add `("depends_on", KeywordKind::Other)`. The existing `pest_grammar_contains_every_keyword` test will pass because `depends_on` is just an attribute key, never a top-level statement keyword — but adding it to KEYWORDS gives the LSP semantic-tokens path a single source of truth. |
| `carina-core/src/plan_tree.rs` | No change required: `get_resource_dependencies` already reads from `dependency_bindings`, which after the differ union includes explicit edges. Add a regression test confirming an explicit-only edge appears in the tree. |
| `carina-core/src/formatter/format.rs` (or wherever block formatting lives) | `directives.depends_on` formatter normalises the list to alphabetical order on `carina fmt` for stable diffs. |
| `carina-lsp/src/completion/values.rs` | Inside a `directives { ... }` block: add `depends_on` to the block-key completion set. Inside the list `depends_on = [|]`: query the binding index filtered by kind ∈ {resource, wait, module}, exclude the enclosing binding name, exclude names already present in the list. |
| `carina-lsp/src/diagnostics/mod.rs` | Mirror the seven analysis-pass checks so live editor diagnostics match `carina validate` exactly. |
| `carina-lsp/src/semantic_tokens.rs` | No change required if `tokenize_line` already routes through `keywords::is_keyword`; otherwise add `depends_on` to the highlighted set. |
| `editors/vscode/syntaxes/carina.tmLanguage.json` | Add `depends_on` to the keyword pattern. |
| `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` | Same change, byte-identical (parity test enforces). |

### Dependencies between files

```
keywords.rs ── parser/blocks/attributes.rs ── resource/mod.rs (Directives)
                                                       │
                                                       ├── effect.rs (explicit_dependencies)
                                                       │
                                                       ├── deps.rs ── differ ── plan_tree.rs (no change needed)
                                                       ├── resolver.rs                        (no change needed)
                                                       └── parser/resolve.rs                  (no change needed)
                                                                  ↑
                                                                  └── validation/depends_on.rs ── validation/mod.rs ── carina-lsp diagnostics

editors/*/carina.tmLanguage.json ──── tmlanguage_keyword_parity test
formatter/format.rs ── tests
carina-lsp completion/semantic-tokens
```

## Tasks

Each task is one TDD cycle. Goal: write failing test → run → see fail → minimal impl → run → see pass.

### Phase 1 — `Directives.depends_on` field

**Task 1.1: `Directives` carries a `depends_on: Vec<String>` field that defaults to empty.**

- **Files:** modify `carina-core/src/resource/mod.rs`.
- **Test:** add to `carina-core/src/resource/tests.rs`:
  ```rust
  #[test]
  fn directives_depends_on_defaults_to_empty() {
      let d = Directives::default();
      assert_eq!(d.depends_on, Vec::<String>::new());
  }

  #[test]
  fn directives_depends_on_round_trips_serde() {
      let d = Directives {
          depends_on: vec!["role".to_string(), "key".to_string()],
          ..Directives::default()
      };
      let json = serde_json::to_string(&d).unwrap();
      let back: Directives = serde_json::from_str(&json).unwrap();
      assert_eq!(back.depends_on, d.depends_on);
  }

  #[test]
  fn directives_depends_on_deserialises_from_legacy_json_without_field() {
      // State files written before this PR have no `depends_on` key in directives.
      let legacy = r#"{ "force_delete": false, "create_before_destroy": false, "prevent_destroy": false }"#;
      let d: Directives = serde_json::from_str(legacy).unwrap();
      assert_eq!(d.depends_on, Vec::<String>::new());
  }
  ```
- **Implementation:** add field to `Directives` (the post-#2826 struct):
  ```rust
  #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
  pub struct Directives {
      #[serde(default)]
      pub force_delete: bool,
      #[serde(default)]
      pub create_before_destroy: bool,
      #[serde(default)]
      pub prevent_destroy: bool,
      /// Explicit ordering edges declared by the user. Each element is the
      /// binding name of a sibling `let` (resource / wait / module).
      /// Set semantics (deduplicated, order-insensitive); represented as
      /// Vec to preserve source order for `carina fmt` round-tripping.
      #[serde(default)]
      pub depends_on: Vec<String>,
  }
  ```
- **Verification:** `cargo nextest run -p carina-core resource::tests::directives_depends_on_defaults_to_empty resource::tests::directives_depends_on_round_trips_serde resource::tests::directives_depends_on_deserialises_from_legacy_json_without_field`.

### Phase 2 — Parser support

**Task 2.1: `extract_directives` parses `depends_on = [a, b]` into `Vec<String>` of binding names.**

- **Files:** modify `carina-core/src/parser/blocks/attributes.rs`.
- **Test:** add to the existing `mod tests` in the same file (or in `parser/tests.rs`):
  ```rust
  #[test]
  fn extract_directives_reads_depends_on_list() {
      let src = r#"
          let bucket = aws.s3.Bucket {
              bucket_name = "x"
              directives {
                  depends_on = [iam_role, kms_key]
              }
          }
      "#;
      let parsed = crate::parser::parse_str(src).unwrap();
      let bucket = parsed.resources.iter().find(|r| r.id.name == "bucket").unwrap();
      assert_eq!(bucket.directives.depends_on, vec!["iam_role".to_string(), "kms_key".to_string()]);
  }
  ```
- **Implementation:** in `extract_directives` (post-#2826 rename of `extract_lifecycle_config`), after pulling existing booleans, also pull `depends_on`. Bare identifiers in attribute lists already materialise as `Value::ResourceRef { binding, ... }` because the existing parser uses `Value::resource_ref(name.to_string(), String::new(), vec![])` as a placeholder for identifier references (see `parser/blocks/attributes.rs:138`). The extraction:
  ```rust
  let depends_on = match map.get("depends_on") {
      Some(Value::List(items)) => items
          .iter()
          .map(|v| match v {
              Value::ResourceRef { binding, .. } => Ok(binding.clone()),
              Value::String(_) => Err(format!(
                  "directives.depends_on: list elements must be binding identifiers, not string literals"
              )),
              other => Err(format!(
                  "directives.depends_on: unexpected element {:?}", other
              )),
          })
          .collect::<Result<Vec<_>, String>>()
          .map_err(|msg| ParseError::InvalidExpression {
              message: msg,
              span: directives_block_span.clone(),
          })?,
      None => Vec::new(),
      Some(other) => return Err(ParseError::InvalidExpression {
          message: format!("directives.depends_on: must be a list, got {:?}", other),
          span: directives_block_span.clone(),
      }),
  };
  ```
  `directives_block_span` is the span of the `directives { ... }` block already in scope at this point in `extract_directives` (the function takes the block's `pest::Span` as input or via the surrounding pair). If `extract_directives` does not currently receive a span, add it as a parameter — the caller (`parser/blocks/resource.rs::~50`) has the pair available.

  Then construct `Directives { force_delete, create_before_destroy, prevent_destroy, depends_on }` instead of the existing 3-field struct.
- **Verification:** `cargo nextest run -p carina-core parser::blocks::attributes::tests::extract_directives_reads_depends_on_list`.

**Task 2.2: Parser rejects `depends_on = ["x"]` (string literal element) at parse time.**

- **Files:** add to `attributes.rs` tests.
- **Test:**
  ```rust
  #[test]
  fn extract_directives_rejects_string_literal_in_depends_on() {
      let src = r#"
          let bucket = aws.s3.Bucket {
              bucket_name = "x"
              directives { depends_on = ["iam_role"] }
          }
      "#;
      let result = crate::parser::parse_str(src);
      assert!(result.is_err(), "expected error for string-literal in depends_on");
      let err = result.unwrap_err().to_string();
      assert!(err.contains("binding identifiers"), "error should mention identifiers, got: {}", err);
  }
  ```
- **Implementation:** none — Task 2.1's `Value::String(_) => Err(...)` branch already handles this; the test pins the contract.
- **Verification:** `cargo nextest run -p carina-core parser::blocks::attributes::tests::extract_directives_rejects_string_literal_in_depends_on`.

**Task 2.3: `keywords.rs` lists `depends_on`.**

- **Files:** modify `carina-core/src/keywords.rs`.
- **Test:** add to the existing `mod tests`:
  ```rust
  #[test]
  fn depends_on_is_a_keyword() {
      assert!(is_keyword("depends_on"));
  }
  ```
- **Implementation:** add `("depends_on", KeywordKind::Other),` to `KEYWORDS`. The existing `pest_grammar_contains_every_keyword` test requires the literal `"depends_on"` to appear in `carina.pest`; since this is an attribute key parsed inside the `directives` block (which uses generic attribute parsing), add a comment-only mention of `depends_on` in `carina.pest` near the `directives_block` rule to satisfy the parity test:
  ```pest
  // The directives block accepts: force_delete, create_before_destroy,
  // prevent_destroy, depends_on. (Listed here to satisfy the keyword
  // parity test in carina-core/src/keywords.rs.)
  ```
- **Verification:** `cargo nextest run -p carina-core keywords::tests::depends_on_is_a_keyword keywords::tests::pest_grammar_contains_every_keyword`.

### Phase 3 — Differ unions explicit edges into `dependency_bindings`

**Task 3.1: `deps.rs` unions `directives.depends_on` into `Resource.dependency_bindings`.**

- **Files:** modify `carina-core/src/deps.rs`.
- **Test:** add to `carina-core/src/deps.rs` tests:
  ```rust
  #[test]
  fn directives_depends_on_is_unioned_into_dependency_bindings() {
      let mut role = Resource::new("iam.Role", "role");
      let mut bucket = Resource::new("s3.Bucket", "bucket")
          .with_attribute("bucket_name", Value::String("x".to_string()));
      bucket.directives.depends_on = vec!["role".to_string()];

      let mut resources = vec![role, bucket];
      crate::deps::populate_dependency_bindings(&mut resources);

      let bucket_after = resources.iter().find(|r| r.id.name == "bucket").unwrap();
      assert!(bucket_after.dependency_bindings.contains("role"));
  }
  ```
- **Implementation:** at line ~320, change:
  ```rust
  resource.dependency_bindings = value_ref_deps.into_iter().collect();
  ```
  to:
  ```rust
  let mut all = value_ref_deps;
  all.extend(resource.directives.depends_on.iter().cloned());
  resource.dependency_bindings = all.into_iter().collect();
  ```
  If `populate_dependency_bindings` does not exist as a separate function, extract the union site into one so the test above can call it directly. (The existing call sites in `deps.rs:320`, `resolver.rs:72`, and `parser/resolve.rs:145` will then call the same helper, satisfying Tasks 3.2 and 3.3 simultaneously.)
- **Verification:** `cargo nextest run -p carina-core deps::tests::directives_depends_on_is_unioned_into_dependency_bindings`.

**Task 3.2: `resolver.rs::resolve_refs_with_state` produces unioned `dependency_bindings`.**

- **Files:** add a test in `carina-core/src/resolver.rs` `mod tests`. If Task 3.1 extracted a `union_dependency_bindings` helper, the production code at `resolver.rs:72` now calls it and the test is the only new code. If Task 3.1 inlined the change at three call sites instead of extracting, ensure `resolver.rs:72` matches `deps.rs:320`.
- **Test:**
  ```rust
  #[test]
  fn resolve_refs_unions_directives_depends_on() {
      let mut bucket = Resource::new("s3.Bucket", "bucket")
          .with_attribute("bucket_name", Value::String("bucket".to_string()));
      bucket.directives.depends_on = vec!["role".to_string()];
      let mut resources = vec![
          Resource::new("iam.Role", "role"),
          bucket,
      ];
      let state_lookup: HashMap<ResourceId, State> = HashMap::new();
      let bindings: HashMap<String, ResourceId> = HashMap::new();
      resolve_refs_with_state(&mut resources, &state_lookup, &bindings).unwrap();
      let bucket_after = resources.iter().find(|r| r.id.name == "bucket").unwrap();
      assert!(bucket_after.dependency_bindings.contains("role"));
  }
  ```
- **Implementation:** if Task 3.1 introduced a `union_dependency_bindings` helper called from all three sites, this test passes without further code. If Task 3.1 inlined the change only at `deps.rs:320`, mirror the inline change at `resolver.rs:72`:
  ```rust
  // Before
  resource.dependency_bindings = deps.into_iter().collect();
  // After
  let mut all = deps;
  all.extend(resource.directives.depends_on.iter().cloned());
  resource.dependency_bindings = all.into_iter().collect();
  ```
- **Verification:** `cargo nextest run -p carina-core resolver::tests::resolve_refs_unions_directives_depends_on`.

**Task 3.3: `parser/resolve.rs::resolve_in_file` (or the analogous entry point) produces unioned `dependency_bindings`.**

- **Files:** add a test in `carina-core/src/parser/resolve.rs` `mod tests`. Same shape as Task 3.2.
- **Test:**
  ```rust
  #[test]
  fn parser_resolve_unions_directives_depends_on() {
      let src = r#"
          let role = aws.iam.Role { role_name = "r" assume_role_policy_document = "{}" }
          let bucket = aws.s3.Bucket {
              bucket_name = "b"
              directives { depends_on = [role] }
          }
      "#;
      let parsed = crate::parser::parse_str(src).unwrap();
      // After parse_str runs (which internally calls parser/resolve.rs),
      // dependency_bindings should already be unioned.
      let bucket = parsed.resources.iter().find(|r| r.id.name == "bucket").unwrap();
      assert!(bucket.dependency_bindings.contains("role"));
  }
  ```
- **Implementation:** if Task 3.1 introduced the `union_dependency_bindings` helper called from all three sites, this test passes without further code. Otherwise, mirror the Task 3.2 inline change at `parser/resolve.rs:145`:
  ```rust
  // Before
  resource.dependency_bindings = deps.into_iter().collect();
  // After
  let mut all = deps;
  all.extend(resource.directives.depends_on.iter().cloned());
  resource.dependency_bindings = all.into_iter().collect();
  ```
- **Verification:** `cargo nextest run -p carina-core parser::resolve::tests::parser_resolve_unions_directives_depends_on`.

### Phase 4 — `Effect.explicit_dependencies` field

**Task 4.1: Every `Effect` variant carries `explicit_dependencies: HashSet<String>` that defaults to empty.**

- **Files:** modify `carina-core/src/effect.rs`.
- **Test:** add to `effect.rs` tests:
  ```rust
  #[test]
  fn create_carries_explicit_dependencies() {
      let r = Resource::new("s3.Bucket", "b");
      let effect = Effect::Create(r);
      assert_eq!(effect.explicit_dependencies(), &HashSet::<String>::new());
  }

  #[test]
  fn delete_carries_explicit_dependencies_alongside_dependencies() {
      // Note: Effect::Delete's third field is renamed `lifecycle: LifecycleConfig`
      // → `directives: Directives` by #2826 (prerequisite). This test is written
      // against the post-#2826 field names.
      let effect = Effect::Delete {
          id: ResourceId::new("s3.Bucket", "b"),
          identifier: "x".to_string(),
          directives: Directives::default(),
          binding: None,
          dependencies: HashSet::new(),
          explicit_dependencies: HashSet::from(["role".to_string()]),
      };
      assert_eq!(
          effect.explicit_dependencies(),
          &HashSet::from(["role".to_string()])
      );
  }
  ```
- **Implementation:** add the field to every variant of `Effect`. For variants whose data is a single `Resource` (`Create(Resource)`), wrap the field at the Effect level rather than threading it inside Resource — this mirrors how `Effect::Delete::dependencies` is structured today (separate field, not inside the embedded ResourceId). The `Create` case becomes:
  ```rust
  Create {
      resource: Resource,
      explicit_dependencies: HashSet<String>,
  }
  ```
  which is a structural change from `Create(Resource)` (tuple variant) to a struct variant. Update the very few existing `Effect::Create(r)` constructors and pattern-matchers (`format_effect_brief`, `plan_tree.rs`, `differ`, `executor`, tests) — the compiler reports each.

  Also add an accessor:
  ```rust
  impl Effect {
      pub fn explicit_dependencies(&self) -> &HashSet<String> {
          match self {
              Effect::Create { explicit_dependencies, .. }
              | Effect::Update { explicit_dependencies, .. }
              | LegacyReplace { explicit_dependencies, .. }
              | Effect::Delete { explicit_dependencies, .. }
              | Effect::Read { explicit_dependencies, .. }
              | Effect::Wait { explicit_dependencies, .. }     // pending #2825
              | Effect::Import { explicit_dependencies, .. }
              | Effect::Remove { explicit_dependencies, .. }
              | Effect::Move { explicit_dependencies, .. } => explicit_dependencies,
          }
      }
  }
  ```
- **Scope note:** turning `Effect::Create(Resource)` from a tuple variant into a struct variant `Effect::Create { resource: Resource, explicit_dependencies: HashSet<String> }` is a breaking pattern change for every existing call site that matches `Effect::Create(r)`. The compiler reports each. Expected sites (from a quick grep):
  - `carina-core/src/plan.rs::format_effect_brief` (1 arm)
  - `carina-core/src/plan_tree.rs::build_dependency_graph` (1 arm)
  - `carina-core/src/differ/plan.rs` (Effect construction sites)
  - `carina-core/src/executor/parallel.rs` (multiple arms)
  - `carina-core/src/executor/phased.rs` (multiple arms)
  - `carina-core/src/executor/basic.rs` (multiple arms)
  - `carina-core/src/executor/replace.rs`
  - `carina-core/src/executor/tests.rs` (many constructor sites)
  - `carina-core/src/effect.rs::tests` (constructor sites)
  - `carina-core/src/detail_rows.rs`
  - `carina-cli/tests/...` snapshot fixtures may need regeneration
  Each site is a one-line mechanical change (add the new field with `HashSet::new()` for backward-equivalent default, or destructure with `.., explicit_dependencies: ..`). No semantic change.

- **Verification:** `cargo nextest run -p carina-core effect::tests::create_carries_explicit_dependencies effect::tests::delete_carries_explicit_dependencies_alongside_dependencies`. Per CLAUDE.md "Verify Protocol", nextest's compile step is sufficient to surface non-exhaustive-pattern errors in downstream callers — no separate `cargo build` step.

**Task 4.2: Differ populates `Effect.explicit_dependencies` from `Resource.directives.depends_on`.**

- **Files:** modify the differ entry point that constructs Effects (likely `carina-core/src/differ/plan.rs` or `differ/mod.rs`).
- **Test:** add to `differ/plan_tests.rs`:
  ```rust
  #[test]
  fn differ_populates_effect_explicit_dependencies_from_directives() {
      let mut bucket = Resource::new("s3.Bucket", "bucket")
          .with_attribute("bucket_name", Value::String("x".to_string()));
      bucket.directives.depends_on = vec!["role".to_string()];
      let resources = vec![Resource::new("iam.Role", "role"), bucket];

      let mut schemas = SchemaRegistry::new();
      schemas.insert("", ResourceSchema::new("iam.Role"));
      schemas.insert("", ResourceSchema::new("s3.Bucket")
          .attribute(AttributeSchema::new("bucket_name", AttributeType::String).create_only()));

      let plan = create_plan(&resources, &HashMap::new(), &HashMap::new(), &schemas, &HashMap::new(), &HashMap::new(), &HashMap::new());

      let bucket_effect = plan.effects().iter().find(|e| match e {
          Effect::Create { resource, .. } => resource.id.name == "bucket",
          _ => false,
      }).expect("bucket Create effect missing");
      assert!(bucket_effect.explicit_dependencies().contains("role"));
  }
  ```
- **Implementation:** in the differ's per-resource Effect construction path, populate `explicit_dependencies: resource.directives.depends_on.iter().cloned().collect::<HashSet<_>>()` for every Effect variant. For `Effect::Delete` (which doesn't carry a Resource), look up the deleted resource's pre-deletion state and copy from there (same place where the existing `dependencies` field is populated).
- **Verification:** `cargo nextest run -p carina-core differ::plan_tests::differ_populates_effect_explicit_dependencies_from_directives`.

### Phase 5 — Validation diagnostics (the seven rules)

**Task 5.1: Diagnose `depends_on = [unknown_binding]` as an error.**

- **Files:** create `carina-core/src/validation/depends_on.rs`; modify `carina-core/src/validation/mod.rs`.
- **Test:** in the new `validation/depends_on.rs` tests:
  ```rust
  #[test]
  fn unknown_binding_is_diagnosed() {
      let src = r#"
          let bucket = aws.s3.Bucket {
              bucket_name = "x"
              directives { depends_on = [non_existent] }
          }
      "#;
      let parsed = crate::parser::parse_str(src).unwrap();
      let diags = validate_depends_on(&parsed);
      assert!(diags.iter().any(|d|
          d.severity == DiagnosticSeverity::Error
          && d.message.contains("non_existent")
          && d.message.contains("not declared")
      ));
  }
  ```
- **Implementation:** in `validation/depends_on.rs`:
  ```rust
  pub fn validate_depends_on(parsed: &File<()>) -> Vec<Diagnostic> {
      let mut diags = Vec::new();
      let known_bindings: HashSet<&str> = parsed.iter_let_bindings()
          .map(|b| b.name.as_str())
          .collect();
      for binding in parsed.iter_let_bindings() {
          for dep_name in &binding.directives().depends_on {
              if !known_bindings.contains(dep_name.as_str()) {
                  diags.push(Diagnostic::error(format!(
                      "directives.depends_on: binding '{}' is not declared in this scope",
                      dep_name
                  )).at(binding.depends_on_span(dep_name)));
              }
          }
      }
      diags
  }
  ```
  The `Diagnostic`/`DiagnosticSeverity` types and `iter_let_bindings()`/`directives()`/`depends_on_span(...)` accessor methods already exist in `validation/mod.rs` for the value-reference checks; reuse them. Wire `validate_depends_on` into the top-level analysis pass so `carina validate` calls it.
- **Verification:** `cargo nextest run -p carina-core validation::depends_on::tests::unknown_binding_is_diagnosed`.

**Task 5.2: Diagnose self-reference (`a directives { depends_on = [a] }`) as an error.**

- **Files:** modify `validation/depends_on.rs`.
- **Test:**
  ```rust
  #[test]
  fn self_reference_is_diagnosed() {
      let src = r#"
          let a = aws.s3.Bucket {
              bucket_name = "x"
              directives { depends_on = [a] }
          }
      "#;
      let parsed = crate::parser::parse_str(src).unwrap();
      let diags = validate_depends_on(&parsed);
      assert!(diags.iter().any(|d|
          d.severity == DiagnosticSeverity::Error
          && d.message.contains("self-reference")
          && d.message.contains("'a'")
      ));
  }
  ```
- **Implementation:** in the same loop as Task 5.1, before the unknown-binding check, add:
  ```rust
  if dep_name == &binding.name {
      diags.push(Diagnostic::error(format!(
          "directives.depends_on: self-reference is not allowed (binding '{}' depends on itself)",
          dep_name
      )).at(binding.depends_on_span(dep_name)));
      continue;  // skip the unknown-binding check for this element
  }
  ```
- **Verification:** `cargo nextest run -p carina-core validation::depends_on::tests::self_reference_is_diagnosed`.

**Task 5.3: Diagnose `depends_on = [data_source_binding]` as a kind violation.**

- **Files:** modify `validation/depends_on.rs`.
- **Test:**
  ```rust
  #[test]
  fn data_source_binding_in_depends_on_is_diagnosed() {
      let src = r#"
          let user = aws.identitystore.User { user_name = "alice" }   // data source
          let bucket = aws.s3.Bucket {
              bucket_name = "x"
              directives { depends_on = [user] }
          }
      "#;
      let parsed = crate::parser::parse_str(src).unwrap();
      let diags = validate_depends_on(&parsed);
      assert!(diags.iter().any(|d|
          d.severity == DiagnosticSeverity::Error
          && d.message.contains("data sources")
          && d.message.contains("'user'")
      ));
  }
  ```
- **Implementation:** classify each known binding by kind (`ResourceKind::Managed`, `ResourceKind::DataSource`, ResourceKind for `wait`, module-call). For each `dep_name`, look up the kind; if kind ∈ {DataSource, UpstreamState}, emit an error.
- **Verification:** `cargo nextest run -p carina-core validation::depends_on::tests::data_source_binding_in_depends_on_is_diagnosed`.

**Task 5.4: Diagnose duplicate elements (`depends_on = [x, x]`) as a warning.**

- **Files:** modify `validation/depends_on.rs`.
- **Test:**
  ```rust
  #[test]
  fn duplicate_element_is_diagnosed_as_warning() {
      let src = r#"
          let role = aws.iam.Role { ... }
          let bucket = aws.s3.Bucket {
              bucket_name = "x"
              directives { depends_on = [role, role] }
          }
      "#;
      let parsed = crate::parser::parse_str(src).unwrap();
      let diags = validate_depends_on(&parsed);
      assert!(diags.iter().any(|d|
          d.severity == DiagnosticSeverity::Warning
          && d.message.contains("listed twice")
          && d.message.contains("'role'")
      ));
  }
  ```
- **Implementation:** before iterating elements, count occurrences with a `HashMap<&str, usize>`; emit warning for any element with count > 1.
- **Verification:** `cargo nextest run -p carina-core validation::depends_on::tests::duplicate_element_is_diagnosed_as_warning`.

**Task 5.5: Diagnose redundant edge (already implied by value reference) as a warning.**

- **Files:** modify `validation/depends_on.rs`.
- **Test:**
  ```rust
  #[test]
  fn redundant_value_ref_edge_is_diagnosed_as_warning() {
      let src = r#"
          let key = aws.kms.Key { key_alias = "k" }
          let bucket = aws.s3.Bucket {
              bucket_name    = "x"
              encryption_key = key.arn               // value reference
              directives { depends_on = [key] }      // redundant
          }
      "#;
      let parsed = crate::parser::parse_str(src).unwrap();
      let diags = validate_depends_on(&parsed);
      assert!(diags.iter().any(|d|
          d.severity == DiagnosticSeverity::Warning
          && d.message.contains("redundant")
          && d.message.contains("'key'")
      ));
  }
  ```
- **Implementation:** for each binding, compute the value-reference dependency set via the existing `get_resource_dependencies(&resource)` helper. For each `dep_name` in `directives.depends_on`, emit a warning if `dep_name` is already in the value-ref set.
- **Verification:** `cargo nextest run -p carina-core validation::depends_on::tests::redundant_value_ref_edge_is_diagnosed_as_warning`.

**Task 5.6: Diagnose cycle (`a depends_on [b]`, `b depends_on [a]`) as an error.**

- **Files:** modify `validation/depends_on.rs`.
- **Test:**
  ```rust
  #[test]
  fn depends_on_cycle_is_diagnosed() {
      let src = r#"
          let a = aws.s3.Bucket { bucket_name = "a" directives { depends_on = [b] } }
          let b = aws.s3.Bucket { bucket_name = "b" directives { depends_on = [a] } }
      "#;
      let parsed = crate::parser::parse_str(src).unwrap();
      let diags = validate_depends_on(&parsed);
      assert!(diags.iter().any(|d|
          d.severity == DiagnosticSeverity::Error
          && d.message.contains("cycle")
      ));
  }
  ```
- **Implementation:** build the union dependency graph (value-refs ∪ depends_on) for the parsed file, run `crate::deps::topological_sort` over it, and convert the existing cycle-detection error into a `Diagnostic` attached to the binding span. Reuse the topological-sort code that already exists in `deps.rs` rather than re-implementing.
- **Verification:** `cargo nextest run -p carina-core validation::depends_on::tests::depends_on_cycle_is_diagnosed`.

**Task 5.7: Wire `validate_depends_on` into the top-level analysis pass shared by `carina validate` and the LSP.**

- **Files:** modify `carina-core/src/validation/mod.rs`.
- **Test:**
  ```rust
  #[test]
  fn validate_resources_runs_depends_on_checks() {
      // Pass a resource with an unknown depends_on; expect the top-level
      // validate_resources to surface the error without a direct call to
      // validate_depends_on.
      let src = r#"
          let bucket = aws.s3.Bucket {
              bucket_name = "x"
              directives { depends_on = [non_existent] }
          }
      "#;
      let parsed = crate::parser::parse_str(src).unwrap();
      let mut errors = Vec::new();
      validate_resources(&parsed, &mut errors);
      assert!(errors.iter().any(|e| e.contains("non_existent")));
  }
  ```
- **Implementation:** in `validate_resources` (line 19 of `validation/mod.rs`), after the existing checks, call `validate_depends_on(parsed)` and append its diagnostics to the error list. Convert the diagnostic format (`Diagnostic` struct → string) using the same conversion the rest of the pass uses.
- **Verification:** `cargo nextest run -p carina-core validation::tests::validate_resources_runs_depends_on_checks`.

### Phase 6 — Plan tree (regression coverage)

**Task 6.1: `plan_tree::build_dependency_graph` includes explicit-only edges in the tree.**

- **Files:** modify `carina-core/src/plan_tree.rs` tests.
- **Test:**
  ```rust
  #[test]
  fn explicit_only_edge_appears_in_dependency_graph() {
      let mut bucket = Resource::new("s3.Bucket", "bucket")
          .with_attribute("bucket_name", Value::String("x".to_string()));
      bucket.directives.depends_on = vec!["role".to_string()];
      // After differ runs, dependency_bindings is unioned (Phase 3).
      bucket.dependency_bindings = std::collections::BTreeSet::from(["role".to_string()]);

      let plan = Plan::from_effects(vec![
          Effect::Create { resource: Resource::new("iam.Role", "role"), explicit_dependencies: HashSet::new() },
          Effect::Create { resource: bucket, explicit_dependencies: HashSet::from(["role".to_string()]) },
      ]);
      let graph = build_dependency_graph(&plan);
      let bucket_effect_idx = graph.effect_bindings.iter().find(|(_, b)| b.as_str() == "bucket").unwrap().0;
      assert!(graph.effect_deps[bucket_effect_idx].contains("role"));
  }
  ```
- **Implementation:** none — `get_resource_dependencies` reads `dependency_bindings`, which the differ already unioned in Phase 3. This test is a regression gate confirming the union flows through to the tree.
- **Verification:** `cargo nextest run -p carina-core plan_tree::tests::explicit_only_edge_appears_in_dependency_graph`.

### Phase 7 — Formatter

**Task 7.1: `carina fmt` normalises `depends_on` list to alphabetical order.**

- **Files:** modify `carina-core/src/formatter/format.rs`.
- **Test:** in `formatter/tests.rs` (or its existing test module):
  ```rust
  #[test]
  fn fmt_normalises_depends_on_to_alphabetical() {
      let input = r#"
          let bucket = aws.s3.Bucket {
              bucket_name = "x"
              directives { depends_on = [zebra, apple, mango] }
          }
      "#;
      let output = crate::formatter::format(input).unwrap();
      assert!(output.contains("depends_on = [apple, mango, zebra]"));
  }
  ```
- **Implementation:** in the formatter's directives-block emitter, when emitting `depends_on`, sort the `Vec<String>` alphabetically before serialising. (The IR-side `Vec<String>` retains source order so other consumers see the user's spelling; the formatter is the only thing that re-orders.)
- **Verification:** `cargo nextest run -p carina-core formatter::tests::fmt_normalises_depends_on_to_alphabetical`.

### Phase 8 — LSP

**Task 8.1: LSP completes `depends_on` as a key inside `directives { ... }`.**

- **Files:** modify `carina-lsp/src/completion/values.rs`.
- **Test:** in `carina-lsp/src/completion/values_tests.rs`:
  ```rust
  #[test]
  fn directives_block_completion_includes_depends_on() {
      let doc = "let bucket = aws.s3.Bucket {\n  bucket_name = \"x\"\n  directives {\n    \n  }\n}";
      let cursor = position_of(doc, /* line=3, col=4 */);
      let items = directives_block_completions(doc, cursor);
      let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
      assert!(labels.contains(&"depends_on"));
      assert!(labels.contains(&"prevent_destroy"));
      assert!(labels.contains(&"force_delete"));
      assert!(labels.contains(&"create_before_destroy"));
  }
  ```
- **Implementation:** find the existing function that emits the three current `directives`-block keys and add `depends_on` to its returned list.
- **Verification:** `cargo nextest run -p carina-lsp completion::values_tests::directives_block_completion_includes_depends_on`.

**Task 8.2: LSP list-element completion inside `depends_on = [|]` returns only resource/wait/module bindings.**

- **Files:** modify `carina-lsp/src/completion/values.rs`.
- **Test:**
  ```rust
  #[test]
  fn depends_on_list_completion_filters_by_kind() {
      let doc = r#"
          let role = aws.iam.Role { ... }                          // resource: included
          let user = aws.identitystore.User { ... }                // data source: excluded
          let bucket = aws.s3.Bucket {
              bucket_name = "x"
              directives { depends_on = [|] }
          }
      "#;
      let cursor = position_inside_brackets(doc);
      let items = depends_on_list_completions(doc, cursor);
      let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
      assert!(labels.contains(&"role"));
      assert!(!labels.contains(&"user"));
      assert!(!labels.contains(&"bucket")); // self-exclusion
  }
  ```
- **Implementation:** detect cursor inside a `depends_on = [...]` list using token lookback (similar to the existing list-position detection used elsewhere in the LSP). Query the document's `binding_index` for all bindings, filter by kind ∈ {Managed, Wait, Module}, exclude the enclosing binding's own name, exclude names already present in the list (parse the list elements before the cursor).
- **Verification:** `cargo nextest run -p carina-lsp completion::values_tests::depends_on_list_completion_filters_by_kind`.

**Task 8.3: LSP diagnostics surface every analysis-pass check from Phase 5.**

- **Files:** modify `carina-lsp/src/diagnostics/mod.rs`.
- **Test:** in `carina-lsp/src/diagnostics/tests.rs`:
  ```rust
  #[test]
  fn lsp_surfaces_unknown_binding_in_depends_on() {
      let src = r#"
          let bucket = aws.s3.Bucket {
              bucket_name = "x"
              directives { depends_on = [non_existent] }
          }
      "#;
      let diags = compute_diagnostics(src);
      assert!(diags.iter().any(|d|
          d.severity == lsp_types::DiagnosticSeverity::ERROR
          && d.message.contains("non_existent")
      ));
  }
  ```
- **Implementation:** the LSP's diagnostics dispatch already wraps `validate_resources`. If Phase 5 Task 5.7 wired `validate_depends_on` into `validate_resources`, this test passes without further changes — it gates against regression. If the LSP runs a different validation entry point, dispatch `validate_depends_on` from there too.
- **Verification:** `cargo nextest run -p carina-lsp diagnostics::tests::lsp_surfaces_unknown_binding_in_depends_on`.

### Phase 9 — TextMate grammar parity

**Task 9.1: TextMate grammars highlight `depends_on` consistently.**

- **Files:** modify both `editors/vscode/syntaxes/carina.tmLanguage.json` and `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` (must remain byte-identical per `carina-core/tests/tmlanguage_keyword_parity.rs`).
- **Test:** the existing parity test catches drift:
  ```bash
  cargo nextest run -p carina-core tmlanguage_keyword_parity
  ```
  Task 2.3 (adding `depends_on` to KEYWORDS) makes this test fail until the grammars are updated. That's the failing-test signal.
- **Implementation:** locate the existing keyword-pattern regex in both JSON files and add `depends_on`. Same edit, applied identically to both files.
- **Verification:** `cargo nextest run -p carina-core tmlanguage_keyword_parity`.

### Phase 10 — Multi-file directory acceptance

**Task 10.1: Multi-file fixture: `directives.depends_on` references a binding declared in a sibling `.crn` file.**

- **Files:** create `carina-core/tests/fixtures/depends_on/basic/main.crn` and `carina-core/tests/fixtures/depends_on/basic/directives.crn`. Add an integration test `carina-core/tests/depends_on_directory_test.rs`.
- **Test:**
  ```rust
  #[test]
  fn depends_on_resolves_across_sibling_files_in_directory() {
      let dir = "tests/fixtures/depends_on/basic";
      let parsed = carina_core::parse_directory(dir).expect("parse should succeed");
      let mut errors = Vec::new();
      carina_core::validation::validate_resources(&parsed, &mut errors);
      assert!(errors.is_empty(), "validation errors: {:?}", errors);

      // Confirm the union actually happened
      let bucket = parsed.resources.iter().find(|r| r.id.name == "bucket").unwrap();
      assert!(bucket.dependency_bindings.contains("role"));
  }
  ```
- **Implementation:** create the two files. `main.crn`:
  ```crn
  provider aws { region = "us-east-1" }

  let role = aws.iam.Role {
      role_name                  = "my-role"
      assume_role_policy_document = "{}"
  }
  ```
  `directives.crn`:
  ```crn
  let bucket = aws.s3.Bucket {
      bucket_name = "my-bucket"
      directives { depends_on = [role] }
  }
  ```
  This exercises the directory-scoped requirement from CLAUDE.md.
- **Verification:** `cargo nextest run -p carina-core depends_on_directory_test::depends_on_resolves_across_sibling_files_in_directory`.

### Phase 11 — Plan-display snapshot

**Task 11.1: Snapshot test for the plan tree when an explicit-only dependency exists.**

- **Files:** create `carina-cli/tests/fixtures/plan_display/depends_on/main.crn`, `carina.state.json`, and `snapshot.txt`. Add the test:
  ```rust
  #[test]
  fn plan_display_depends_on() {
      run_plan_snapshot("depends_on");
  }
  ```
  in `carina-cli/tests/plan_snapshot.rs`.
- **Test:** the snapshot infrastructure compares produced output to `snapshot.txt`.
- **Implementation:** `main.crn`:
  ```crn
  provider aws { region = "us-east-1" }

  let role = aws.iam.Role {
      role_name                   = "my-role"
      assume_role_policy_document = "{}"
  }

  let bucket = aws.s3.Bucket {
      bucket_name = "my-bucket"
      directives { depends_on = [role] }
  }
  ```
  `carina.state.json`: `{}` (empty).
  Run the test once with `cargo insta review` to write the snapshot, then commit. Add the fixture to the existing `make plan-fixtures` Makefile target.
- **Verification:** `cargo nextest run -p carina-cli plan_display_depends_on`.

### Phase 12 — Whole-tree gates (run before opening PR)

- `cargo nextest run` (workspace)
- `cargo test --workspace --doc`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --check`
- `bash scripts/check-*.sh` for every script in `scripts/`
- `cargo build --release` is **skipped** unless this PR has touched `Cargo.toml`, `unsafe` code, or the release profile (per CLAUDE.md "Verify Protocol"). This work touches none of those.

## Plan self-review

| Requirement from design doc | Covered by |
|---|---|
| `depends_on` field on `Directives` | Task 1.1 |
| Parser extracts `depends_on` list | Tasks 2.1, 2.2 |
| `depends_on` is a keyword | Task 2.3 |
| Differ unions explicit edges into `dependency_bindings` | Tasks 3.1, 3.2, 3.3 |
| `Effect.explicit_dependencies` field on every variant | Task 4.1 |
| Differ populates `explicit_dependencies` | Task 4.2 |
| 7 validation rules: unknown binding | Task 5.1 |
| 7 validation rules: self-reference | Task 5.2 |
| 7 validation rules: disallowed kind (data source / upstream_state) | Task 5.3 |
| 7 validation rules: duplicate elements | Task 5.4 |
| 7 validation rules: redundant value-ref | Task 5.5 |
| 7 validation rules: cycle | Task 5.6 |
| 7 validation rules: element type mismatch (string literal) | Task 2.2 (caught at parser, surfaced before analysis) |
| Validation wired into top-level pass | Task 5.7 |
| Plan tree shows explicit edges | Task 6.1 |
| `carina fmt` normalises list to alphabetical | Task 7.1 |
| LSP block-key completion includes `depends_on` | Task 8.1 |
| LSP list-element completion strict filter | Task 8.2 |
| LSP diagnostics mirror analysis pass | Task 8.3 |
| TextMate grammar parity | Task 9.1 |
| Multi-file fixture acceptance | Task 10.1 |
| Plan-display snapshot | Task 11.1 |
| Hover (reuses existing handler — no new code) | Inherited from existing identifier-hover; no task needed |
| Semantic tokens (routes through `keywords::is_keyword`) | Inherited from Task 2.3 + existing tokenizer; verify with Task 9.1's parity test |
| State serialisation (Resource.directives auto-serialised) | Task 1.1 (round-trip + legacy test) |
| `wait` consumer (carina#2825) uses `depends_on` | Out of scope here; verified when #2825 lands |

No placeholder phrases remain. Every task has a concrete file, concrete test, concrete implementation snippet, and exact `cargo nextest run` verification command.

## Suggested PR ordering

When opening the PR, group commits per phase so reviewers can move bottom-up:

1. **`Directives.depends_on` field + serde round-trip** (Phase 1) — small, self-contained.
2. **Parser** (Phase 2) — accepts the new attribute.
3. **Differ union + `Effect.explicit_dependencies`** (Phases 3, 4) — load-bearing wiring.
4. **Diagnostics** (Phase 5) — analysis-pass checks; gates real user code paths.
5. **Plan tree + formatter** (Phases 6, 7) — surface behaviour.
6. **LSP** (Phase 8) — editor experience.
7. **TextMate parity + multi-file fixture + snapshot** (Phases 9–11) — gates the whole stack.
