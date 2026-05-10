# ExplicitFields — Per-resource user-authored field tree

## Goal

Eliminate spurious diff lines for AWS server-side default values that
the user never wrote. Concretely close the awscc#206 reproduction in
which `awscc.s3.Bucket` plans show:

```
- transition_default_minimum_object_size: "all_storage_classes_128K"
```

even though no `.crn` source mentions the field.

The fix extends the existing top-level `ResourceState.desired_keys:
Vec<String>` (state file's user-authored key list, consumed by the
differ via `prev_desired_keys`) to a **recursive tree** that captures
authoring information for nested struct fields and list-of-struct
elements as well. The differ projects the `current` state through this
tree before comparison, removing fields the user never wrote so they
do not surface as diffs.

## Non-goals

- Changing the WIT plugin contract. The fix is entirely inside
  carina-core / carina-state / carina-cli.
- Distinguishing per-list-element user authoring (different elements
  may have written different fields). The List variant captures the
  union across all elements; this is a documented simplification.
- Touching either provider crate (carina-provider-aws,
  carina-provider-awscc). The fix lives in core; providers are
  unaffected.

## Background

`ResourceState.desired_keys: Vec<String>` (carina-state/src/state/mod.rs:459)
already records the **top-level** attribute keys the user wrote in
their `.crn`. The differ uses it via `prev_desired_keys`
(carina-core/src/differ/comparison.rs:382) to detect "user removed
this attribute" — when a key is in `prev_desired_keys` but not in
the current desired state, the differ emits a `Remove` op.

The same mechanism is needed inside `Value::Struct` and
`Value::List` of structs: when AWS Cloud Control returns a struct
field the user never wrote, that field should not appear in the
diff. Today's `desired_keys` only tracks top-level keys, so the
nested case is unhandled and the spurious `- field: value` line
appears.

## Chosen approach

Replace `desired_keys: Vec<String>` with a recursive **`ExplicitFields`
tree** that mirrors the structure the user wrote, then **project** the
current state through the tree before diffing or display.

### Type

```rust
/// Tree describing which fields the user explicitly wrote in their
/// `.crn` for this resource. Each variant corresponds to a `Value`
/// shape:
///
/// - `Leaf`: user wrote this position as a scalar value (or as an
///   opaque value that has no nested authoring information). Treated
///   as "user wrote the whole thing" — no projection inside.
/// - `Struct`: user wrote a struct here. Only the listed `children`
///   are user-authored; struct fields not listed are server-only and
///   are removed by projection.
/// - `List`: user wrote a list of structs here. `element` is the
///   union of authoring across all elements (see "Edge cases").
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ExplicitFields {
    #[default]
    Leaf,
    Struct {
        children: HashMap<String, ExplicitFields>,
    },
    List {
        element: Box<ExplicitFields>,
    },
}
```

### Storage in state

```rust
pub struct ResourceState {
    // ... existing fields ...
    /// Tree of fields the user explicitly wrote in their `.crn` for this
    /// resource. Used by the differ to skip "removal" diff lines for
    /// fields the user never specified (server-side defaults like
    /// awscc.s3.Bucket.lifecycle_configuration.transition_default_minimum_object_size).
    /// Replaces the flat `desired_keys: Vec<String>` (state ≤ v5).
    #[serde(default)]
    pub explicit: ExplicitFields,
}
```

`desired_keys: Vec<String>` is **removed** in state v6.

### Construction

`carina-core/src/explicit.rs` (new module):

```rust
pub fn build_from_resource(resource: &Resource) -> ExplicitFields {
    ExplicitFields::Struct {
        children: resource.attributes.iter()
            .filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, v)| (k.clone(), build_from_value(v)))
            .collect(),
    }
}

fn build_from_value(value: &Value) -> ExplicitFields {
    match value {
        Value::Struct { fields, .. } => ExplicitFields::Struct {
            children: fields.iter()
                .map(|(k, v)| (k.clone(), build_from_value(v)))
                .collect(),
        },
        Value::List(items) => ExplicitFields::List {
            element: Box::new(
                items.iter()
                    .map(build_from_value)
                    .fold(ExplicitFields::Leaf, merge),
            ),
        },
        _ => ExplicitFields::Leaf,
    }
}

fn merge(a: ExplicitFields, b: ExplicitFields) -> ExplicitFields {
    use ExplicitFields::*;
    match (a, b) {
        (Leaf, b) => b,
        (a, Leaf) => a,
        (Struct { children: mut a }, Struct { children: b }) => {
            for (k, v) in b {
                let merged = match a.remove(&k) {
                    Some(existing) => merge(existing, v),
                    None => v,
                };
                a.insert(k, merged);
            }
            Struct { children: a }
        }
        (List { element: a }, List { element: b }) => List {
            element: Box::new(merge(*a, *b)),
        },
        // Mismatched shapes (shouldn't occur for well-typed inputs):
        // prefer the "richer" structural variant.
        (a @ Struct { .. }, _) => a,
        (a @ List { .. }, _) => a,
    }
}
```

### Projection

`carina-core/src/explicit.rs`:

```rust
/// Strip from `value` everything not listed in `explicit`. Used to
/// remove server-side defaults from the actual-state side before
/// diffing. Idempotent: project(project(v)) == project(v).
pub fn project(value: Value, explicit: &ExplicitFields) -> Value {
    match (value, explicit) {
        // user wrote whole leaf: keep entire current value
        (v, ExplicitFields::Leaf) => v,
        (Value::Struct { name, fields }, ExplicitFields::Struct { children }) => {
            Value::Struct {
                name,
                fields: fields.into_iter()
                    .filter_map(|(k, v)| {
                        children.get(&k).map(|sub| (k, project(v, sub)))
                    })
                    .collect(),
            }
        }
        (Value::List(items), ExplicitFields::List { element }) => {
            Value::List(
                items.into_iter()
                    .map(|item| project(item, element))
                    .collect(),
            )
        }
        // shape mismatch (state inconsistent or schema drift): keep
        // value as-is to avoid hiding real data
        (v, _) => v,
    }
}

pub fn project_attributes(
    attrs: HashMap<String, Value>,
    explicit: &ExplicitFields,
) -> HashMap<String, Value> {
    match explicit {
        ExplicitFields::Struct { children } => attrs.into_iter()
            .filter_map(|(k, v)| {
                children.get(&k).map(|sub| (k, project(v, sub)))
            })
            .collect(),
        // top-level being Leaf or List shouldn't occur for a
        // resource's full attribute set; pass through conservatively
        _ => attrs,
    }
}
```

### Differ integration

Two changes to `carina-core/src/differ/comparison.rs`:

1. At the entry of `find_changed_attributes`, project the `current`
   map: `let current = project_attributes(current, explicit)`.
2. Replace the `prev_desired_keys: Option<&[String]>` parameter
   with `prev_explicit: Option<&ExplicitFields>`. The "user removed
   this top-level attribute" detection (lines 382–394) reads
   `prev_explicit` as a `Struct { children }` and iterates its keys.

Equivalent updates in `carina-core/src/differ/plan.rs` and
`carina-core/src/differ/mod.rs` to thread the new type instead of
`Vec<String>`.

### Display integration

`carina-core/src/detail_rows.rs::build_*_rows` calls
`compute_unchanged_count(&from.attributes, ...)` (line 488). Project
`from.attributes` (the current state) before counting:

```rust
let projected = project_attributes(from.attributes.clone(), explicit);
let unchanged_count = compute_unchanged_count(&projected, ...);
```

`build_*_rows` signatures gain an `explicit: &ExplicitFields`
parameter (or accept a pre-projected `from`). Same treatment for any
other display path that reads `from.attributes` directly. TUI mirrors
the CLI's row construction, so once `detail_rows.rs` is corrected the
TUI inherits the fix.

### State versioning (v5 → v6)

State file `version` bumps from 5 to 6.

- v6 is the only writable format.
- v5 read path constructs `ExplicitFields::Struct { children: ... }`
  with each `desired_keys` entry mapped to `ExplicitFields::Leaf`:

  ```rust
  fn convert_v5_resource_state(rs: ResourceStateV5) -> ResourceState {
      let explicit = ExplicitFields::Struct {
          children: rs.desired_keys.into_iter()
              .map(|k| (k, ExplicitFields::Leaf))
              .collect(),
      };
      ResourceState { explicit, /* other fields */ }
  }
  ```

- After v5 read, the **first plan** still surfaces nested-field
  spurious diffs (the v5 file has no nested authoring info). The
  **first apply** writes back v6 with a fully-built tree from
  `from_provider_state`, and from then on nested server defaults are
  hidden. This one-time degradation is acceptable per the project's
  "No backward compat" policy.

- The v5 reader is retained until the next state version (v7), at
  which point it is removed in the same PR that introduces v7.

### Test fixtures

All `carina-cli/tests/fixtures/plan_display/*/carina.state.json`
files are rewritten in v6 form (`explicit` instead of `desired_keys`).
A new fixture `server_default_struct_leaf/` reproduces the
awscc#206 scenario: desired state writes
`lifecycle_configuration { rules = [...] }` without
`transition_default_minimum_object_size`, current has it; expected
snapshot has no `- transition_default_minimum_object_size` line.

## Key design decisions

### D1. Replace `desired_keys` outright (not co-exist)

Per memory `feedback_no_backward_compat`, the project does not retain
deprecated fields. v5 read path performs a one-shot conversion; v6 is
the only writable format. No "two sources of truth" between
`desired_keys` and `explicit`.

### D2. Field name `explicit`, type name `ExplicitFields`

Earlier candidate `desired_keys` was rejected as imprecise (`keys`
implies a flat list; the structure is a tree). `Tracked` /
`Provenance` were rejected because a previous superseded design
already used those names. `explicit` matches the IaC literature
("explicit vs implicit" / "explicit vs server-default") and reads
naturally for a tree (`state.explicit.children["lifecycle_configuration"]`).

### D3. List of struct uses union semantics, not per-element

A `Value::List` of `Value::Struct` is represented as
`ExplicitFields::List { element: Box<ExplicitFields> }`. The
`element` is the **union** of authoring across all list elements —
if any element wrote a field, that field is considered authored for
all elements. Rationale:

- Per-element index tracking breaks under reordering, insertion, and
  deletion. The element identity problem is fundamental.
- Real `.crn` configurations almost always write the same shape
  across all elements of a list. The union is virtually never wider
  than what any single element wrote.
- If a future requirement genuinely needs per-element provenance,
  the `ExplicitFields` enum can grow a new variant; the existing
  `List { element }` form remains a valid restriction.

### D4. Build from `Value`, not from parser AST

`build_from_value` takes a `Value` and walks its structure. The
parser-level alternative (record authoring during parsing) was
considered and rejected as over-engineered: `Value`-walking handles
empty structs/lists correctly via the `match` arms, and avoids
threading authoring state through the parser.

The corner case "user wrote `lifecycle_configuration {}`" is correctly
captured: the value is `Value::Struct { fields: [] }`,
`build_from_value` returns `Struct { children: HashMap::new() }`,
projection then strips every server-side leaf inside that block. This
matches the user's intent ("I wrote an empty struct; everything
inside is server-managed").

### D5. Project both in differ and display, idempotently

The differ projects `current` before computing patch ops; the
display projects `from.attributes` before counting unchanged. Both
sites use the same `project_attributes` function, which is idempotent:
double-projection is a no-op. This yields a defense-in-depth: even
if a future code path forgets to project, double-projection by an
intermediate function does not produce wrong output.

A wrapper type `ProjectedAttributes` to enforce projection at the
type level was considered and rejected as over-engineered.
Idempotence + a clear naming convention (`current_projected`,
`from_projected`) is sufficient.

## File touch map

### carina-core
- `carina-core/src/explicit.rs` (new) — `ExplicitFields` enum,
  `build_from_resource`, `build_from_value`, `merge`, `project`,
  `project_attributes`, unit tests.
- `carina-core/src/lib.rs` — re-export `explicit` module.
- `carina-core/src/differ/comparison.rs` — replace
  `prev_desired_keys: Option<&[String]>` parameter with
  `prev_explicit: Option<&ExplicitFields>`; project `current` at
  function entry; rewrite the lines 382–394 removal-detection loop
  to walk `prev_explicit`'s `Struct { children }`.
- `carina-core/src/differ/plan.rs` — update `prev_desired_keys`
  parameter type and HashMap value type
  (`HashMap<ResourceId, ExplicitFields>`).
- `carina-core/src/differ/mod.rs` — same parameter type update.
- `carina-core/src/detail_rows.rs` — accept `&ExplicitFields`;
  project `from.attributes` before passing to
  `compute_unchanged_count`.
- `carina-core/src/differ/diff_tests.rs`,
  `carina-core/src/differ/comparison_tests.rs`,
  `carina-core/src/differ/plan_tests.rs` — update test fixtures
  to use `ExplicitFields`; add new tests covering nested struct
  field projection.

### carina-state
- `carina-state/src/state/mod.rs`:
  - Replace `desired_keys: Vec<String>` with
    `explicit: ExplicitFields` on `ResourceState`.
  - Bump `CURRENT_VERSION` to 6.
  - Replace `build_desired_keys()` with `build_explicit()` returning
    `HashMap<ResourceId, ExplicitFields>`.
  - In `from_provider_state`, replace `desired_keys` construction
    with `carina_core::explicit::build_from_resource(resource)`.
  - Add v5→v6 read path that converts `desired_keys: Vec<String>`
    into a flat `ExplicitFields::Struct` with `Leaf` children.
- `carina-state/src/state/tests.rs` — update existing tests; add
  v5→v6 conversion test.

### carina-cli
- `carina-cli/src/fixture_plan.rs`,
  `carina-cli/src/wiring/tests.rs`,
  `carina-cli/src/tests.rs`,
  `carina-cli/src/plan_snapshot_tests.rs` — replace
  `prev_desired_keys` variable type and constructions
  (`HashMap<ResourceId, Vec<String>>` → `HashMap<ResourceId, ExplicitFields>`).
- `carina-cli/tests/fixtures/plan_display/*/carina.state.json` —
  rewrite each fixture from v5 (`desired_keys`) to v6
  (`explicit`). Use the v5→v6 conversion as a helper one-shot
  script if desired.
- `carina-cli/tests/fixtures/plan_display/server_default_struct_leaf/`
  (new) — `main.crn` and `carina.state.json` reproducing
  awscc#206. Snapshot asserts no `-` line for the server-default
  field.
- `Makefile` — new `plan-server-default-struct-leaf` target per
  memory rule `feedback_makefile_for_fixtures`.

### carina-tui
- `carina-tui/src/ui/detail.rs`,
  `carina-tui/src/ui/diff.rs` — if either reads `from.attributes`
  directly (rather than going through `DetailRow`), apply
  projection. Otherwise unchanged.

## Edge cases

- **Empty struct authored** (`lifecycle_configuration {}`):
  `build_from_value` produces `Struct { children: empty }`;
  projection strips every nested field. Correct: user explicitly
  wrote "empty config; everything inside is server-managed".

- **Empty list authored** (`tags = []`): `Value::List([])`;
  `build_from_value` returns `List { element: Leaf }` (default).
  Projection of an empty `Value::List([])` is unchanged. If the
  current side has elements, those elements pass through projection
  with `element: Leaf` (which keeps the whole element, since Leaf
  means "user wrote the whole thing"). This correctly surfaces
  added elements as diffs.

- **State file from before this change** (v5 with
  `desired_keys`): converted on read to `Struct` with `Leaf`
  children. First plan after upgrade still shows nested-field
  spurious diffs; first apply writes v6 with full tree; subsequent
  plans are clean. Acceptable.

- **`Value::Map` (not Struct, not List)**: maps are treated as
  Leaf in `build_from_value`. Projection of a Leaf leaves the
  whole map intact. This means map values are not projected
  field-by-field — sufficient for current AWS schemas where
  server-side defaults appear inside Struct fields, not inside
  user-authored Maps. A future Map variant on `ExplicitFields`
  can be added if needed.

- **Schema drift** (current has shape that doesn't match
  authoring tree): the projection's catch-all `(v, _) => v` keeps
  the value as-is. Conservative: better to over-show than
  under-show in this rare case.

## Testing

- **Unit tests** in `carina-core/src/explicit.rs`:
  serde round-trip, `merge` associativity, `project` idempotence,
  `build_from_value` for each `Value` variant.
- **Differ tests** in `carina-core/src/differ/diff_tests.rs`:
  - server-default struct leaf does not produce a `Remove` op
  - user-removed top-level attribute still produces a `Remove` op
    (regression guard for the existing `prev_desired_keys` behavior)
  - server-default in list element does not produce a diff
- **State tests** in `carina-state/src/state/tests.rs`:
  v5→v6 conversion preserves top-level keys as `Leaf` children;
  v6 round-trip preserves full tree.
- **Snapshot fixture** in
  `carina-cli/tests/fixtures/plan_display/server_default_struct_leaf/`:
  full-stack assertion that no `-` line appears for a server-default
  struct leaf.
- **Real-infra smoke** (manual, by user): `aws-vault exec mizzy --
  carina plan` against `carina-rs/infra/.../registry/dev/registry`
  shows no `- transition_default_minimum_object_size` line.

## Risks

- **State v6 lock-in**: once carina writes v6, older carina
  binaries fail to read it. Standard for this experimental project.
- **Test fixture rewrite churn**: every `carina.state.json` in
  `plan_display/` fixtures is bumped to v6. Mechanical change but
  large surface; a one-shot script is cheap insurance.
- **Plan output silently improves**: snapshot fixtures must be
  reviewed to confirm the *only* changes are server-default lines
  disappearing; any other delta is a regression.
