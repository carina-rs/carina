# Implementation Plan — ExplicitFields

<!-- derived-from ../specs/2026-05-10-explicit-fields-design.md -->

## Goal

Implement `docs/specs/2026-05-10-explicit-fields-design.md`. Final
acceptance: real-infra `carina plan` against the awscc#206 reproduction
(`registry/dev/registry`) shows no
`- transition_default_minimum_object_size: "all_storage_classes_128K"`
line. All within the carina repo (no WIT or provider repo touch).

## Repo

`carina-rs/carina` only. No upstream submodule changes.

## Task list

Each task corresponds to one GitHub Issue and one PR. Tasks 1–8 are
sequential dependencies; tasks 9 (display) and 10 (TUI projection
audit) can begin once 4 lands. Tasks 11–13 (fixture rewrite,
new fixture, real-infra acceptance) run after the core work is in.

---

### Task 1/13 — `ExplicitFields` enum + serde + unit tests

**Repo:** carina
**Labels:** enhancement, rust, task-1/13
**Blocked by:** none

**Goal:** Land the new `ExplicitFields` type as a free-standing module,
with no consumer wiring yet.

**Files:**
- Create `carina-core/src/explicit.rs`
- Modify `carina-core/src/lib.rs` to add `pub mod explicit;`

**Tests (write first):**
```rust
// carina-core/src/explicit.rs (#[cfg(test)] mod tests)
#[test]
fn leaf_is_default() {
    let e: ExplicitFields = Default::default();
    assert!(matches!(e, ExplicitFields::Leaf));
}

#[test]
fn struct_round_trips_via_serde() {
    let e = ExplicitFields::Struct {
        children: HashMap::from([
            ("a".into(), ExplicitFields::Leaf),
            ("b".into(), ExplicitFields::Struct {
                children: HashMap::from([("nested".into(), ExplicitFields::Leaf)]),
            }),
        ]),
    };
    let json = serde_json::to_string(&e).unwrap();
    let back: ExplicitFields = serde_json::from_str(&json).unwrap();
    assert_eq!(e, back);
}

#[test]
fn list_round_trips_via_serde() {
    let e = ExplicitFields::List {
        element: Box::new(ExplicitFields::Struct {
            children: HashMap::from([("id".into(), ExplicitFields::Leaf)]),
        }),
    };
    let json = serde_json::to_string(&e).unwrap();
    let back: ExplicitFields = serde_json::from_str(&json).unwrap();
    assert_eq!(e, back);
}

#[test]
fn variant_serializes_kebab_case() {
    let leaf_json = serde_json::to_string(&ExplicitFields::Leaf).unwrap();
    assert_eq!(leaf_json, r#"{"kind":"leaf"}"#);
}
```

**Implementation:**
```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ExplicitFields {
    Leaf,
    Struct { children: HashMap<String, ExplicitFields> },
    List { element: Box<ExplicitFields> },
}

impl Default for ExplicitFields {
    fn default() -> Self { ExplicitFields::Leaf }
}
```

**Verification:**
```
cargo nextest run -p carina-core explicit
cargo test --workspace --doc
cargo clippy -p carina-core --all-targets -- -D warnings
```

**Acceptance:**
- `ExplicitFields` exported from `carina_core::explicit`.
- Serde round-trip test passes for all three variants.
- Default is `Leaf`.

---

### Task 2/13 — `build_from_resource` / `build_from_value` / `merge`

**Repo:** carina
**Labels:** enhancement, rust, task-2/13
**Blocked by:** Task 1/13

**Goal:** Construct `ExplicitFields` from `Resource` and `Value`.

**Files:**
- Modify `carina-core/src/explicit.rs`

**Tests (write first):**
```rust
#[test]
fn build_from_value_scalar_is_leaf() {
    let v = Value::String("x".into());
    assert_eq!(build_from_value(&v), ExplicitFields::Leaf);
}

#[test]
fn build_from_value_struct_collects_children() {
    let v = Value::Struct {
        name: "S".into(),
        fields: vec![
            ("a".into(), Value::String("x".into())),
            ("b".into(), Value::Int(1)),
        ],
    };
    let result = build_from_value(&v);
    let ExplicitFields::Struct { children } = result else { panic!() };
    assert_eq!(children.len(), 2);
    assert!(matches!(children["a"], ExplicitFields::Leaf));
    assert!(matches!(children["b"], ExplicitFields::Leaf));
}

#[test]
fn build_from_value_list_unions_element_authoring() {
    // Element 1 wrote {a, b}; element 2 wrote {b, c}; union = {a, b, c}
    let v = Value::List(vec![
        Value::Struct { name: "S".into(), fields: vec![
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::Int(1)),
        ]},
        Value::Struct { name: "S".into(), fields: vec![
            ("b".into(), Value::Int(2)),
            ("c".into(), Value::Int(2)),
        ]},
    ]);
    let result = build_from_value(&v);
    let ExplicitFields::List { element } = result else { panic!() };
    let ExplicitFields::Struct { children } = *element else { panic!() };
    let mut keys: Vec<&str> = children.keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["a", "b", "c"]);
}

#[test]
fn build_from_resource_skips_underscore_attrs() {
    let mut r = Resource::with_provider("aws", "s3.bucket", "x");
    r.set_attr("name".into(), Value::String("hi".into()));
    r.set_attr("_internal".into(), Value::String("skip".into()));
    let result = build_from_resource(&r);
    let ExplicitFields::Struct { children } = result else { panic!() };
    assert!(children.contains_key("name"));
    assert!(!children.contains_key("_internal"));
}

#[test]
fn merge_struct_into_struct_unions_keys() {
    let a = ExplicitFields::Struct {
        children: HashMap::from([("a".into(), ExplicitFields::Leaf)]),
    };
    let b = ExplicitFields::Struct {
        children: HashMap::from([("b".into(), ExplicitFields::Leaf)]),
    };
    let result = merge(a, b);
    let ExplicitFields::Struct { children } = result else { panic!() };
    assert_eq!(children.len(), 2);
}
```

**Implementation:** as in design doc § "Construction".

**Verification:**
```
cargo nextest run -p carina-core explicit
cargo test --workspace --doc
cargo clippy -p carina-core --all-targets -- -D warnings
```

**Acceptance:**
- Construction from `Resource` matches design semantics for scalar,
  struct, list, list-of-structs (union), underscore filtering.

---

### Task 3/13 — `project` / `project_attributes` (idempotent)

**Repo:** carina
**Labels:** enhancement, rust, task-3/13
**Blocked by:** Task 2/13

**Goal:** Add the projection function that strips fields not in the
authoring tree.

**Files:**
- Modify `carina-core/src/explicit.rs`

**Tests (write first):**
```rust
#[test]
fn project_struct_drops_unauthored_field() {
    let value = Value::Struct {
        name: "S".into(),
        fields: vec![
            ("authored".into(), Value::String("keep".into())),
            ("server_default".into(), Value::String("drop".into())),
        ],
    };
    let explicit = ExplicitFields::Struct {
        children: HashMap::from([("authored".into(), ExplicitFields::Leaf)]),
    };
    let result = project(value, &explicit);
    let Value::Struct { fields, .. } = result else { panic!() };
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].0, "authored");
}

#[test]
fn project_leaf_keeps_whole_value() {
    let value = Value::Struct {
        name: "S".into(),
        fields: vec![("any".into(), Value::Int(1))],
    };
    let result = project(value.clone(), &ExplicitFields::Leaf);
    assert_eq!(result, value);
}

#[test]
fn project_is_idempotent() {
    let value = Value::Struct {
        name: "S".into(),
        fields: vec![
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::Int(2)),
        ],
    };
    let explicit = ExplicitFields::Struct {
        children: HashMap::from([("a".into(), ExplicitFields::Leaf)]),
    };
    let once = project(value.clone(), &explicit);
    let twice = project(once.clone(), &explicit);
    assert_eq!(once, twice);
}

#[test]
fn project_list_recurses_into_each_element() {
    let value = Value::List(vec![
        Value::Struct { name: "S".into(), fields: vec![
            ("authored".into(), Value::Int(1)),
            ("server".into(), Value::Int(2)),
        ]},
        Value::Struct { name: "S".into(), fields: vec![
            ("authored".into(), Value::Int(3)),
            ("server".into(), Value::Int(4)),
        ]},
    ]);
    let explicit = ExplicitFields::List {
        element: Box::new(ExplicitFields::Struct {
            children: HashMap::from([("authored".into(), ExplicitFields::Leaf)]),
        }),
    };
    let Value::List(items) = project(value, &explicit) else { panic!() };
    for item in &items {
        let Value::Struct { fields, .. } = item else { panic!() };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "authored");
    }
}

#[test]
fn project_mismatched_shape_keeps_value() {
    // Authoring says Struct, value is a String — keep value as-is
    let value = Value::String("oops".into());
    let explicit = ExplicitFields::Struct { children: HashMap::new() };
    let result = project(value.clone(), &explicit);
    assert_eq!(result, value);
}

#[test]
fn project_attributes_drops_top_level_unauthored() {
    let attrs = HashMap::from([
        ("a".into(), Value::Int(1)),
        ("server_only".into(), Value::Int(99)),
    ]);
    let explicit = ExplicitFields::Struct {
        children: HashMap::from([("a".into(), ExplicitFields::Leaf)]),
    };
    let result = project_attributes(attrs, &explicit);
    assert_eq!(result.len(), 1);
    assert!(result.contains_key("a"));
}
```

**Implementation:** as in design doc § "Projection".

**Verification:**
```
cargo nextest run -p carina-core explicit::tests::project
cargo test --workspace --doc
cargo clippy -p carina-core --all-targets -- -D warnings
```

**Acceptance:**
- All projection tests pass.
- Idempotence test passes.

---

### Task 4/13 — `ResourceState.explicit` field + state v6 + v5 read path

**Repo:** carina
**Labels:** enhancement, rust, task-4/13
**Blocked by:** Tasks 2/13, 3/13

**Goal:** Replace `desired_keys: Vec<String>` with
`explicit: ExplicitFields` on `ResourceState`. Bump
`CURRENT_VERSION` to 6. Add v5→v6 read conversion.

**Files:**
- Modify `carina-state/src/state/mod.rs`:
  - `ResourceState`: remove `desired_keys`, add
    `explicit: ExplicitFields` (with `#[serde(default)]`).
  - `from_provider_state`: replace
    ```
    rs.desired_keys = resource.attributes.keys()...collect();
    rs.desired_keys.sort();
    ```
    with
    ```
    rs.explicit = carina_core::explicit::build_from_resource(resource);
    ```
  - `build_desired_keys()` → rename to `build_explicit()` returning
    `HashMap<ResourceId, ExplicitFields>`.
  - `CURRENT_VERSION = 6`.
  - In the version-specific reader (around line 380), add a v5
    branch that converts `desired_keys: Vec<String>` to
    `ExplicitFields::Struct { children }` with each key mapped to
    `Leaf`, then drops the `desired_keys` field.

**Tests (write first):**
```rust
// carina-state/src/state/tests.rs (additions)
#[test]
fn v5_state_read_converts_desired_keys_to_explicit_leaves() {
    let v5 = r#"{
        "version": 5,
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "s3.Bucket",
            "name": "x",
            "provider": "aws",
            "identifier": null,
            "attributes": {"name": "x"},
            "desired_keys": ["name", "tags"]
        }]
    }"#;
    let state = parse_state_file(v5).unwrap();
    let rs = &state.resources[0];
    let ExplicitFields::Struct { children } = &rs.explicit else { panic!() };
    assert_eq!(children.len(), 2);
    assert!(matches!(children["name"], ExplicitFields::Leaf));
    assert!(matches!(children["tags"], ExplicitFields::Leaf));
}

#[test]
fn v6_state_writes_and_reads_full_explicit_tree() {
    let mut state = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "x", "aws");
    rs.explicit = ExplicitFields::Struct {
        children: HashMap::from([
            ("lifecycle_configuration".into(), ExplicitFields::Struct {
                children: HashMap::from([
                    ("rules".into(), ExplicitFields::List {
                        element: Box::new(ExplicitFields::Struct {
                            children: HashMap::from([
                                ("id".into(), ExplicitFields::Leaf),
                            ]),
                        }),
                    }),
                ]),
            }),
        ]),
    };
    state.upsert_resource(rs);
    let json = serde_json::to_string(&state).unwrap();
    let back = parse_state_file(&json).unwrap();
    assert_eq!(back.resources[0].explicit, state.resources[0].explicit);
    assert_eq!(back.version, 6);
}
```

**Implementation:** as in design doc § "State versioning".

**Verification:**
```
cargo nextest run -p carina-state
cargo test --workspace --doc
cargo clippy -p carina-state --all-targets -- -D warnings
```

**Acceptance:**
- v5→v6 conversion test passes.
- v6 round-trip test passes.
- `CURRENT_VERSION == 6` and writes always emit v6.

---

### Task 5/13 — Differ comparison: project current + accept `ExplicitFields`

**Repo:** carina
**Labels:** enhancement, rust, task-5/13
**Blocked by:** Task 4/13

**Goal:** Project `current` through `ExplicitFields` at
`find_changed_attributes` entry. Replace
`prev_desired_keys: Option<&[String]>` parameter with
`prev_explicit: Option<&ExplicitFields>`.

**Files:**
- Modify `carina-core/src/differ/comparison.rs`:
  - At entry of `find_changed_attributes`, if `prev_explicit` is
    `Some(e)`, run `let current = project_attributes(current.clone(), e)`.
  - Lines 382–394: replace
    `for key in prev_keys` with iteration over the children of
    `prev_explicit` when it is a `Struct` variant. For each child key
    not in `desired` but present in `current` (now projected), push
    to `changed`.

**Tests (write first):**
```rust
// carina-core/src/differ/diff_tests.rs (additions)
#[test]
fn server_default_struct_field_does_not_appear_in_diff() {
    let desired = HashMap::from([
        ("lifecycle_configuration".into(), Value::Struct {
            name: "LC".into(),
            fields: vec![
                ("rules".into(), Value::List(vec![/*...*/])),
            ],
        }),
    ]);
    let current = HashMap::from([
        ("lifecycle_configuration".into(), Value::Struct {
            name: "LC".into(),
            fields: vec![
                ("rules".into(), Value::List(vec![/*...*/])),
                ("transition_default_minimum_object_size".into(),
                 Value::String("all_storage_classes_128K".into())),
            ],
        }),
    ]);
    let explicit = ExplicitFields::Struct {
        children: HashMap::from([
            ("lifecycle_configuration".into(), ExplicitFields::Struct {
                children: HashMap::from([
                    ("rules".into(), ExplicitFields::Leaf),
                ]),
            }),
        ]),
    };
    let changed = find_changed_attributes(
        &desired, &current, None, Some(&explicit), None, None,
    );
    assert!(!changed.iter().any(|k| k == "lifecycle_configuration"),
        "server-default leaf should not flag the parent attr as changed");
}

#[test]
fn explicit_top_level_removal_still_detected() {
    let desired: HashMap<String, Value> = HashMap::new();
    let current = HashMap::from([
        ("tags".into(), Value::Map(IndexMap::from([
            ("Env".into(), Value::String("prod".into())),
        ]))),
    ]);
    let explicit = ExplicitFields::Struct {
        children: HashMap::from([("tags".into(), ExplicitFields::Leaf)]),
    };
    let changed = find_changed_attributes(
        &desired, &current, None, Some(&explicit), None, None,
    );
    assert!(changed.contains(&"tags".to_string()),
        "user-removed top-level attr must still produce a Remove signal");
}
```

**Implementation:** signature change + projection at entry +
removal-detection rewrite that walks `ExplicitFields::Struct`.

**Verification:**
```
cargo nextest run -p carina-core differ
cargo test --workspace --doc
cargo clippy -p carina-core --all-targets -- -D warnings
```

**Acceptance:**
- New tests pass.
- Existing differ tests pass after parameter type change.
- All `find_changed_attributes` call sites in carina-core updated.

---

### Task 6/13 — Differ plan + mod: thread `ExplicitFields` through

**Repo:** carina
**Labels:** refactor, rust, task-6/13
**Blocked by:** Task 5/13

**Goal:** Update `carina-core/src/differ/plan.rs` and
`carina-core/src/differ/mod.rs` to take
`HashMap<ResourceId, ExplicitFields>` and pass it through to
`find_changed_attributes`.

**Files:**
- Modify `carina-core/src/differ/plan.rs`:
  - Function `compute_diff` (around line 144):
    `prev_desired_keys: &HashMap<ResourceId, Vec<String>>` →
    `prev_explicit: &HashMap<ResourceId, ExplicitFields>`.
  - Around line 172: `let prev_keys = prev_desired_keys.get(&resource.id);`
    → `let prev_explicit = prev_explicit.get(&resource.id);`
- Modify `carina-core/src/differ/mod.rs`:
  - `diff` function (line 62, 66, 77): same parameter rename and
    type change.

**Tests (write first):**
```rust
// carina-core/src/differ/plan_tests.rs (replace
// diff_detects_attribute_removal_with_prev_desired_keys at line 369)
#[test]
fn diff_detects_attribute_removal_with_prev_explicit() {
    let resources = vec![/* ... */];
    let prev_explicit = HashMap::from([
        (resources[0].id.clone(), ExplicitFields::Struct {
            children: HashMap::from([("tags".into(), ExplicitFields::Leaf)]),
        }),
    ]);
    let current_states = HashMap::from([/* ... with tags present ... */]);
    let plan = compute_diff(&resources, &current_states, &prev_explicit, /* ... */);
    // Assert plan contains an Update with a Remove patch op for "tags"
    // ...
}
```

**Implementation:** type-driven; cargo will guide each call site.

**Verification:**
```
cargo nextest run -p carina-core differ
cargo test --workspace --doc
cargo clippy -p carina-core --all-targets -- -D warnings
```

**Acceptance:**
- carina-core compiles.
- Existing differ tests pass.

---

### Task 7/13 — carina-cli: thread `ExplicitFields` through wiring

**Repo:** carina
**Labels:** refactor, rust, task-7/13
**Blocked by:** Task 6/13

**Goal:** Update all carina-cli call sites that build
`prev_desired_keys` to build `prev_explicit` instead.

**Files:**
- Modify `carina-cli/src/fixture_plan.rs` (line 175): replace
  `state_file.build_desired_keys()` with `state_file.build_explicit()`.
- Modify `carina-cli/src/wiring/tests.rs` (lines 172, 180, 197, 257,
  259, 260, 263, 275, 303): variable type
  `HashMap<ResourceId, Vec<String>>` →
  `HashMap<ResourceId, ExplicitFields>`. Constructions:
  `vec!["tags".to_string()]` → `ExplicitFields::Struct { children: HashMap::from([("tags".into(), ExplicitFields::Leaf)]) }`.
- Modify `carina-cli/src/tests.rs` (line 1263): same rename to
  `build_explicit()` and update local variable type.
- Modify `carina-cli/src/plan_snapshot_tests.rs` (lines 442, 444, 446,
  457): rename comments and local variables. Behavior of
  `moved_prev_keys` test stays the same.

**Tests:** existing tests carry the assertions; this task is
type-driven plumbing.

**Verification:**
```
cargo nextest run -p carina-cli
cargo test --workspace --doc
cargo clippy -p carina-cli --all-targets -- -D warnings
```

**Acceptance:**
- carina-cli compiles.
- All wiring tests pass with the new type.

---

### Task 8/13 — Display detail_rows: project `from.attributes`

**Repo:** carina
**Labels:** enhancement, rust, task-8/13
**Blocked by:** Tasks 4/13, 7/13

**Goal:** Project `from.attributes` (current state) before
`compute_unchanged_count` so server-default fields don't inflate the
"N unchanged attributes hidden" count.

**Files:**
- Modify `carina-core/src/detail_rows.rs`:
  - `build_update_rows` and `build_replace_rows`: accept an
    `explicit: &ExplicitFields` parameter (or extract `explicit`
    from `from` by passing the whole `ResourceState` rather than
    just `attributes`).
  - At line 488 area, project `from.attributes` before passing to
    `compute_unchanged_count`:
    ```rust
    let from_projected = project_attributes(
        from.attributes.clone(),
        explicit,
    );
    let unchanged_count = compute_unchanged_count(
        &from_projected,
        &to.resolved_attributes(),
        None,
    );
    ```
- Update all callers of `build_update_rows` / `build_replace_rows`
  to pass `explicit`. Cargo will surface them.

**Tests (write first):**
```rust
// carina-core/src/detail_rows.rs (#[cfg(test)] additions)
#[test]
fn unchanged_count_excludes_server_only_field() {
    let from_attrs = HashMap::from([
        ("authored".into(), Value::String("a".into())),
        ("server_only".into(), Value::String("s".into())),
    ]);
    let to = /* ResourceState with authored="a" — unchanged */;
    let explicit = ExplicitFields::Struct {
        children: HashMap::from([("authored".into(), ExplicitFields::Leaf)]),
    };
    // build update rows; in Full mode the HiddenUnchanged count
    // should be 1 (just "authored"), not 2 ("authored" + "server_only").
    let rows = build_update_rows(&state_with(from_attrs), &to, &[], DetailLevel::Full, &explicit);
    let hidden = rows.iter().find_map(|r| match r {
        DetailRow::HiddenUnchanged { count } => Some(*count),
        _ => None,
    });
    assert_eq!(hidden, Some(1));
}
```

**Verification:**
```
cargo nextest run -p carina-core detail_rows
cargo test --workspace --doc
cargo clippy -p carina-core --all-targets -- -D warnings
```

**Acceptance:**
- Unchanged-count test passes.
- All `build_*_rows` callers updated.

---

### Task 9/13 — TUI: audit and project `current` reads

**Repo:** carina
**Labels:** refactor, rust, task-9/13
**Blocked by:** Task 8/13

**Goal:** Audit `carina-tui/src/ui/detail.rs` and
`carina-tui/src/ui/diff.rs` for any direct reads of
`from.attributes` / `current` that bypass the `DetailRow` pipeline.
If found, project them; if not, no code change beyond a comment
documenting the audit.

**Files:**
- Read-only audit of `carina-tui/src/`. Modify only if a direct
  read is found.
- If a direct read is found, modify the relevant file to project
  via `carina_core::explicit::project_attributes`.

**Tests:**
- If a code change is made, add a TUI snapshot or unit test that
  asserts a server-default field does not surface.
- If no code change, no test required; the audit is the deliverable
  and is recorded in the PR description.

**Verification:**
```
cargo nextest run -p carina-tui
cargo test --workspace --doc
cargo clippy -p carina-tui --all-targets -- -D warnings
```

**Acceptance:**
- Audit recorded in PR body.
- TUI passes existing tests.

---

### Task 10/13 — Rewrite 19 `plan_display` fixture state files to v6

**Repo:** carina
**Labels:** test, rust, task-10/13
**Blocked by:** Task 4/13

**Goal:** Convert all `carina-cli/tests/fixtures/plan_display/*/carina.state.json`
files from v5 (`desired_keys: [...]`) to v6
(`explicit: {"kind": "struct", "children": {...}}`).

**Files** (19 fixture state files):
- `all_create/`, `compact/`, `default_tags/`, `default_values/`,
  `deferred_for/`, `delete_orphan/`, `destroy_full/`,
  `destroy_orphans/`, `enum_display/`, `explicit/`, `exports/`,
  `exports_multifile/`, `list_diff_added_struct/`,
  `list_diff_modified_with_unchanged/`,
  `list_diff_modified_with_unchanged_nested/`,
  `list_diff_removed_struct/`, `map_key_diff/`,
  `mixed_operations/`, `module_anonymous_resource/`,
  `moved_prev_keys/`, `moved_pure/`, `moved_with_changes/`,
  `nested_map_diff/`, `no_changes_enum/`, `no_changes/`,
  `policy_pretty/`, `policy_pretty_dynamic_key_list/`,
  `policy_pretty_nested/`, `pretty_long_string_list/`,
  `pretty_short_string_list/`, `provider_prefix/`,
  `secret_values/`, `state_blocks/`.

**Implementation:**
- Write a one-shot conversion script
  `scripts/convert-fixtures-v5-to-v6.sh` (or Rust binary in
  `carina-state/src/bin/`) that reads each file, applies the v5→v6
  conversion logic from Task 4, writes back. The conversion is
  the same as the in-memory v5 reader: each `desired_keys` entry
  becomes `ExplicitFields::Leaf` under `explicit.children`.
- Run the script once, commit the diff.
- Delete the script after the commit (or leave it under
  `scripts/` if the team prefers a record).

**Verification:**
```
cargo nextest run -p carina-cli plan_snapshot
cargo insta review  # expected: zero drift; the conversion is
                    # semantically equivalent for top-level keys
```

**Acceptance:**
- All 19 fixture state files declare `"version": 6` and
  `"explicit": {...}`.
- `cargo nextest run -p carina-cli plan_snapshot` passes with no
  snapshot drift.

---

### Task 11/13 — New fixture: `server_default_struct_leaf`

**Repo:** carina
**Labels:** test, rust, task-11/13
**Blocked by:** Tasks 5/13, 8/13, 10/13

**Goal:** Add a fixture that reproduces the awscc#206 scenario at
the carina-cli level (mock provider) and asserts no `-` line for
the server-default struct leaf.

**Files:**
- Create
  `carina-cli/tests/fixtures/plan_display/server_default_struct_leaf/main.crn`:
  use a mock-provider resource that has a Struct-typed attribute
  with multiple fields. The `.crn` writes only one of them.
- Create
  `carina-cli/tests/fixtures/plan_display/server_default_struct_leaf/carina.state.json`:
  v6 state where `current.attributes` has both fields, but
  `explicit.children` has only the user-authored one.
- Modify `Makefile`: new `plan-server-default-struct-leaf` target
  per memory rule `feedback_makefile_for_fixtures`.
- Modify `carina-cli/src/plan_snapshot_tests.rs`: register the
  fixture.

**Tests:** the snapshot file itself, with assertion that no `-`
line for the server-default leaf appears in the rendered plan.

**Verification:**
```
cargo nextest run -p carina-cli plan_snapshot
make plan-server-default-struct-leaf
make plan-fixtures
bash scripts/check-docs-drift.sh
```

**Acceptance:**
- New fixture's snapshot has zero `-` lines for the server-default
  leaf.
- All other fixtures' snapshots unchanged.
- Make target works.

---

### Task 12/13 — LSP audit: `desired_keys` references

**Repo:** carina
**Labels:** refactor, rust, task-12/13
**Blocked by:** Task 4/13

**Goal:** Audit `carina-lsp/` for any references to `desired_keys`
or the old `Vec<String>` parameter. Replace with `ExplicitFields`
where they exist.

**Files:**
- Audit: `grep -rn "desired_keys\|prev_desired_keys" carina-lsp/`.
- Modify any matches; expected scope is small (LSP doesn't run the
  differ in practice, but may build state for diagnostics).

**Tests:** if changes are made, add an LSP unit test for the
affected behavior. If no changes, audit is the deliverable.

**Verification:**
```
cargo nextest run -p carina-lsp
cargo clippy -p carina-lsp --all-targets -- -D warnings
```

**Acceptance:**
- carina-lsp compiles.
- LSP tests pass.

---

### Task 13/13 — Real-infra acceptance for awscc#206

**Repo:** carina (validation only)
**Labels:** test, task-13/13
**Blocked by:** all of 1–12

**Goal:** Confirm the awscc#206 reproduction is gone against the
real registry/dev/registry stack.

**Steps (run by user):**
1. Build carina from the merged main: `cargo build --release -p carina-cli`.
2. `cd /Users/mizzy/src/github.com/carina-rs/infra`.
3. `aws-vault exec mizzy -- /path/to/carina plan registry/dev/registry`.
4. Confirm output does NOT contain
   `transition_default_minimum_object_size: "all_storage_classes_128K"`.
5. Inspect the resulting `carina.state.json`: should be `"version": 6`
   with `explicit` populated as a full tree for the bucket.

**Acceptance:**
- No spurious `-` line.
- State file is v6.
- awscc#206 closed with the real-infra evidence pasted in.

---

## Cross-cutting notes

- **State v6 lock-in**: per memory rule `feedback_no_backward_compat`,
  no migration docs. Once v6 is written, older binaries can't read.
- **Snapshot drift policy**: any drift outside the new
  `server_default_struct_leaf` fixture is a bug in the projection
  logic, not an "expected update". Investigate before accepting.
- **No `cargo build` in verify cycle** (CLAUDE.md): use
  `cargo nextest run` which handles the build.
- **Repo-specific CI gates**: `bash scripts/check-*.sh` must pass
  in addition to cargo green (CLAUDE.md / memory
  `feedback_local_green_is_not_ci_green`).
