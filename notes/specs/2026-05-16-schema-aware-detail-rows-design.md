# Schema-Aware Detail-Row Renderer: Design

<!-- constrained-by ./2026-05-12-strict-enum-identifier-design.md -->

## Goal

Make the plan **detail-row renderer** (`carina-core/src/detail_rows.rs`)
schema-aware on the Update and Replace paths, so a nested `StringEnum`
leaf that the schema-typed differ already considers equal
(`EnumIdentifier("allow")` vs `String("Allow")`) is no longer rendered
as a phantom `~ effect: "allow" → "Allow"` row.

The trigger is carina#3073 (root cause of carina-provider-aws#323 and
carina-provider-awscc#254): the differ's `find_changed_attributes` uses
the schema-typed `type_aware_equal`, whose `StringEnum` arm correctly
alias-folds both sides via `canonical()`. But the per-leaf rows are
built by `build_update_rows` / `build_replace_rows`, which the renderer
calls **without the schema** and which compare every nested leaf with
the schema-blind `Value::semantically_equal` (`PartialEq`).
`EnumIdentifier("allow") == String("Allow")` is `false` under
`PartialEq`, so the row is rendered even when the attribute is in
`changed_attributes` only because an unrelated sibling field differs —
or, worse, even when the provider emits perfectly reconciled state. The
loop never converges: every `carina plan` reports a `~` change, every
`carina apply` re-puts the resource.

This design moves enum-equality reconciliation into the **one** place
that renders the diff, so the displayed rows agree with the differ's
own verdict for every current and future provider, with zero provider
code.

## Non-goals

- **Provider-side state canonicalization.** carina-provider-aws#327
  (`AwsNormalizer::normalize_state`) and carina-provider-awscc#255
  (read-path → API-canonical) are per-provider workarounds for this
  core gap. #327 is already closed in favor of this design; #255's
  *layering principle* ("state holds the API form; reconciliation is
  owned by carina-core") stays correct and is **not** reverted by this
  work — a follow-up may simplify the awscc read path *after* this
  lands, but that is out of scope here.
- **Changing `find_changed_attributes` granularity.** It flags a
  top-level attribute key as changed when any nested leaf differs
  (comparison.rs:488, 506). This design does not make change-detection
  finer-grained; it makes the *renderer* stop emitting phantom per-leaf
  rows under an already-flagged attribute. Whether to also narrow
  change-detection granularity is a separate question, explicitly
  deferred.
- **Wiring a state-side enum lift on `current_states`** (the "Core fix
  2 / Defect A" half of carina#3073). That is a real, separate gap
  (`lift_string_enum_leaves` is wired only on `saved_attrs`, never on
  the live `read()` result; its recognizer does not namespace-strip),
  but it is **defense-in-depth**, not the dominant defect. It is
  carved out to its own design + implementation pair so this PR series
  stays single-topic. carina-provider-aws additionally pins a
  pre-#3055 `carina-core` (`7f4c2145`); the eventual aws fix is *this*
  renderer fix + an aws `carina-core` pin bump, independent of Defect
  2.
- **`AttributeType::Custom` / closure-backed enums.** Out of scope by
  construction: `string_enum_parts()` (the arm `type_aware_equal`
  keys off) is `StringEnum`-only, so `Custom` enums are untouched on
  both the differ and the renderer side — they remain symmetric, as
  they are today. The known `ConditionOperator` Map-*key* gap is
  tracked separately (carina-provider-aws#325) and is orthogonal:
  neither the differ nor the renderer descends Map keys.

## Why this lives in the renderer, not in a per-provider carve-out

Three alternatives were ruled out before settling on threading the
schema into the Update/Replace renderer:

| Approach | Why rejected |
|---|---|
| **A: Each provider canonicalizes its own state (status quo: awscc#255, aws#316/#327)** | Per-provider re-implementation of the same reconciliation. Each impl can drift from the differ's own `type_aware_equal` semantics — exactly the divergence class carina#3060/#3063 were created to stop on the *desired* side. The asymmetry (desired reconciled in core, state delegated to providers) **is** the bug. 3+ instances (awscc#251, awscc#254/#255, aws#316/#327) ⇒ fix the root primitive. |
| **B: Make `find_changed_attributes` not flag the attribute when the only differing leaves are enum-equal** | Helps the *summary* count but not the *rendered rows*: `build_update_rows` re-derives per-key sameness independently with `semantically_equal`, so a legitimately-changed sibling still drags the enum leaf into a phantom row. Also a larger, riskier change to change-detection semantics than the rendering bug requires. |
| **C: Make `Value::semantically_equal` itself enum-aware** | `semantically_equal` is schema-blind by design and used pervasively (not just here); it has no `AttributeType` in scope and giving it one would mean threading schema through dozens of unrelated call sites. Wrong layer — the renderer is the site that *has* the registry and *should* be using it. |

The chosen design — forward the `registry` the renderer already
receives into `build_update_rows` / `build_replace_rows`, resolve the
resource `ResourceSchema`, and replace the schema-blind sameness checks
with the existing `type_aware_equal` — fixes the root cause once:

- The displayed rows become consistent with `find_changed_attributes`'
  own verdict (both now use `type_aware_equal`).
- Every provider, current and future, benefits with zero provider code.
- It reuses the differ's already-correct `StringEnum` arm rather than
  inventing a second equality notion.

## Current call graph (verified)

```
build_detail_rows(effect, registry: Option<&SchemaRegistry>, …)
  ├─ Effect::Create  → build_create_rows(r, registry, …)   ← already schema-aware (Full mode)
  ├─ Effect::Update  → build_update_rows(from, to, changed_attributes, …)   ← registry DROPPED
  └─ Effect::Replace → build_replace_rows(from, to, …)                      ← registry DROPPED
```

- `build_detail_rows` already has `registry: Option<&SchemaRegistry>`
  in hand (detail_rows.rs:324) and forwards it to `build_create_rows`
  (line 334) but **not** to `build_update_rows` (line 342) or
  `build_replace_rows` (line 354).
- The two production callers — `carina-cli/src/display/mod.rs:686`
  (`self.schemas`) and `carina-tui/src/app/mod.rs:692` (`schemas`) —
  **already pass a `SchemaRegistry`**. So no public-API or caller
  change is required; this is an internal forwarding fix.
- `build_create_rows` already demonstrates the resolution pattern:
  `registry.get_for(r)` → `ResourceSchema` (detail_rows.rs:451-452).
- `type_aware_equal` (`carina-core/src/differ/comparison.rs:48`) is
  currently `pub(super)` (visible only within the `differ` module).
  `find_changed_attributes` (the change-detection authority) already
  calls it (comparison.rs:477, 483). It must be widened to
  `pub(crate)` so `detail_rows.rs` can reuse the *same* function — not
  a copy.

## Chosen design

### 1. Widen `type_aware_equal` visibility

`pub(super) fn type_aware_equal` → `pub(crate) fn type_aware_equal` in
`carina-core/src/differ/comparison.rs:48`. No signature change. This is
the single shared equality primitive; the renderer must call the exact
function the differ uses, never a reimplementation (avoids the very
drift this issue is about).

Blast radius: `callers_of(type_aware_equal)` = only
`differ/comparison.rs` (self-recursion + `find_changed_attributes`).
Widening visibility adds a caller; it removes none and changes no
behavior for existing callers.

### 2. Thread `registry` into the Update/Replace renderers

```rust
// build_detail_rows
Effect::Update { from, to, changed_attributes, .. } =>
    build_update_rows(from, to, changed_attributes, registry, detail, explicit),
Effect::Replace { from, to, .. } =>
    build_replace_rows(from, to, …, registry, detail, explicit),
```

`build_update_rows` / `build_replace_rows` gain a
`registry: Option<&SchemaRegistry>` parameter. Inside, resolve the
schema once via the existing pattern (`registry.and_then(|r|
r.get_for(to))` — `to` is the `Resource`, same as `build_create_rows`
uses for Create).

### 3. Replace the schema-blind sameness checks with `type_aware_equal`

**Five** schema-blind `semantically_equal` sites — three in
`detail_rows.rs`, two in `diff_helpers.rs` reached via the MapDiff
path — switch to the resolved attribute type when the schema is
available, falling back to `semantically_equal` when it is not (so
behavior is unchanged for the no-registry test/embedded paths). Line
numbers and *enclosing function* verified against the worktree
(the original 4-site draft mis-attributed two sites and missed the
MapDiff path entirely — see Risks):

| # | Enclosing fn (file) | Site | Today | After |
|---|---|---|---|---|
| 1 | `build_update_rows` (detail_rows.rs) | per-key sameness, 562 | `ov.semantically_equal(nv)` | `type_aware_equal(ov, nv, attr_type, None)` |
| 2 | `build_replace_rows` (detail_rows.rs) | per-key sameness, 663 | `ov.semantically_equal(nv)` | `type_aware_equal(ov, nv, attr_type, None)` |
| 3 | `compute_list_of_maps_diff_parts` (detail_rows.rs) | item match, 1017 | `old_item.semantically_equal(new_item)` | `type_aware_equal` with the list-inner type |
| 4 | `compute_list_of_maps_diff_parts` (detail_rows.rs) | per-key within matched item, 1106 | `ov.semantically_equal(&new_map[k])` | `type_aware_equal` with the inner struct-field type |
| 5 | `compute_map_diff` (diff_helpers.rs:111) | MapDiff changed-entry test | `!ov.semantically_equal(nv)` | schema-aware variant (see below) |

Sites 3 & 4 are **both inside `compute_list_of_maps_diff_parts`**
(992-1189), reached from `build_list_of_maps_diff_row` (972).
`build_map_diff_row` (865) / `compute_map_diff_entries` (878) contain
**no** `semantically_equal` of their own — the MapDiff path's
equality is delegated to `diff_helpers::compute_map_diff`
(diff_helpers.rs:94, the changed-entry check at line 111). That is
**site 5** and was missing from the original 4-site list; without it,
an enum leaf rendered through the *Map-diff* shape (as opposed to the
list-of-maps shape) would still show a phantom row.

Site 5 needs care: `compute_map_diff(old_map, new_map) -> MapDiff` is
a `pub` helper also used by tests and other callers, with **no**
`AttributeType` parameter. Two options for the implementation PR
(decide there, with the radius measured):

- **5a (preferred):** add a schema-aware sibling
  `compute_map_diff_typed(old, new, value_type: Option<&AttributeType>)`
  (or thread an optional `AttributeType` through `compute_map_diff`
  with the current signature kept as a thin `None` forwarder so
  existing `pub` callers and tests are untouched). `compute_map_diff_entries`
  passes the resolved struct-field/map-value type.
- **5b:** keep `compute_map_diff` as-is and post-filter its `changed`
  vec in `compute_map_diff_entries` using `type_aware_equal` with the
  field type, dropping entries that are type-aware-equal. Simpler but
  does redundant work and re-derives equality outside the helper.

The other unrelated `semantically_equal` in diff_helpers.rs
(`compute_unchanged_count`, line ~29) is the **summary count**, not a
rendered row; it is in scope only insofar as the count must stay
consistent with the rows (a count that still treats the enum leaf as
changed while the row is suppressed would make the tally wrong). The
implementation PR must keep count and rows consistent — flagged as a
Risk.

`attr_type` is `schema.attributes.get(key).map(|a| &a.attr_type)` — the
same `Option<&AttributeType>` shape `type_aware_equal` already accepts;
nested recursion (`type_aware_struct_equal` / `type_aware_maps_equal`)
descends from there exactly as it does for `find_changed_attributes`.
`secret_ctx` is `None` at the render site (the renderer never had it;
secret reconciliation is unchanged because secrets are already resolved
into the row model before this point — to be confirmed in
implementation, see Risks).

## Blast radius

- **Functions changed:**
  - `type_aware_equal` (`differ/comparison.rs`) — visibility only,
    `pub(super)` → `pub(crate)`.
  - `build_detail_rows` (detail_rows.rs) — 2 call-site arg additions
    (forward `registry` into Update/Replace). Signature unchanged.
  - `build_update_rows`, `build_replace_rows`,
    `build_list_of_maps_diff_row`, `compute_list_of_maps_diff_parts`,
    `compute_map_diff_entries` (detail_rows.rs) — gain a
    `registry`/`AttributeType` thread; **private to `detail_rows.rs`**.
  - `compute_map_diff` (`diff_helpers.rs`) — site 5; the chosen 5a/5b
    approach decides whether this `pub` helper's signature changes. 5a
    with a `None`-forwarding shim keeps every existing `pub` caller
    (detail_rows.rs:891/903 + 4 in-file tests) source-compatible.
- **Public API / callers:** `build_detail_rows` signature unchanged;
  both production callers (`carina-cli/src/display/mod.rs:686`,
  `carina-tui/src/app/mod.rs:692`) already pass the registry.
  `compute_map_diff` is `pub` within `carina-core` — option 5a's
  forwarding shim avoids breaking its callers.
- **Tests:** 16 callers of `build_detail_rows`, all in-file tests,
  pass `None` for the registry today → they exercise the
  `semantically_equal` fallback path, so they keep their current
  expectations. New tests cover the schema-present path.
- **Cross-crate:** carina-cli display + carina-tui already pass
  schemas; their output for enum-equal leaves *changes by design*
  (phantom row disappears). Snapshot tests under
  `carina-cli/tests/fixtures/plan_display/` must be reviewed and a new
  fixture added (IAM-policy-shaped `aws.s3.BucketPolicy` with a
  DSL-alias state vs API-canonical desired) so the regression is
  pinned in CI.

## Test plan

1. **Unit (carina-core):** `build_update_rows` / `build_replace_rows`
   with a registry whose schema types a nested `StringEnum`; assert
   `EnumIdentifier("allow")` vs `String("Allow")` produces **no** row,
   while a genuinely changed sibling still produces its row, and the
   unchanged count stays correct. Negative: no registry → identical to
   today (fallback unchanged). List-of-maps and map-diff nested cases.
2. **Snapshot (carina-cli):** new `plan_display` fixture mirroring the
   carina-provider-aws#323 shape; `cargo nextest run -p carina-cli
   plan_snapshot`. Add the Makefile target (CI "Check Plan Fixtures"
   requires it). Review snapshots before accepting (verify the phantom
   row is gone and nothing legitimate was suppressed).

   > **Implementation note (carina#3073, infeasible as specified):**
   > the `plan_display` fixture harness
   > (`carina-cli/src/fixture_plan.rs`) builds its `SchemaRegistry`
   > solely from provider factories, and fixture tests run with
   > `WiringContext::new(vec![])` → an **empty** registry
   > (`provider_mod::collect_schemas(&[])`). `build_detail_rows` then
   > receives `Some(&empty_registry)`, `get_for` returns `None`, and
   > the schema-aware path is **structurally unreachable** from the
   > snapshot harness — a fixture would only exercise the schema-blind
   > fallback (still showing the phantom, i.e. it would pin the *bug*,
   > not the fix). This design step assumed snapshots have provider
   > schemas; they do not. The regression is instead pinned by the
   > carina-core unit tests in step 1 (which construct a real
   > `SchemaRegistry`): they cover all five sites, the no-registry
   > fallback, count/row consistency, the MapDiff shape, and an
   > over-suppression guard. No `plan_display` fixture/Makefile target
   > is added; the existing snapshot suite is confirmed unchanged
   > (the fix is a no-op when the registry is empty).
3. **Repro confirmation:** the carina#3073 reproduction (an
   IAM-policy-shaped resource whose state holds the DSL alias and whose
   desired resolves to API-canonical) plans clean on the second run.

## PR sequence (design-before-implementation)

1. **This design PR** (`notes/specs/...-design.md` only) — merges
   first.
2. **Implementation PR** — the visibility widening + renderer
   threading + tests + snapshot fixture, `Closes #3073`. Then:
   - carina-provider-aws: bump `carina-core` pin past this fix; confirm
     carina-provider-aws#323 reproduction converges; close #323.
   - carina-provider-awscc: no change required to fix the bug; a
     separate follow-up may simplify the read path now that the core
     owns reconciliation.

## Risks / open questions (resolve in implementation)

- **`secret_ctx` at the render site.** `type_aware_equal` takes
  `Option<&SecretHashContext>`; the renderer has none. Need to confirm
  passing `None` is safe here (secrets should already be projected into
  the row model upstream). If not, the secret path must keep using the
  existing comparison for `Value::Deferred(Secret(_))` leaves.
- **`from` is a `State`, `to` is a `Resource`.** `registry.get_for`
  takes the `Resource` (`to`) — same as Create. Confirm
  `get_for(to)` resolves the managed schema for an Update (it should;
  Update only exists for managed resources).
- **Snapshot churn.** Any existing `plan_display` fixture that today
  shows a phantom enum row will legitimately change. Each such change
  must be eyeballed (memory: review snapshots before accepting) to
  prove it removed a phantom, not a real, diff.
- **Count/row consistency (site 5 corollary).** The trailing
  "N unchanged" summary is computed independently
  (`compute_unchanged_count`, diff_helpers.rs:~29, and the
  `effectively_unchanged` accounting in `build_update_rows`). If the
  rows suppress an enum leaf but the count still treats it as changed
  (or vice-versa), the displayed tally won't add up. The
  implementation must apply the same schema-aware equality to the
  count path that it applies to the rows, and a unit test must assert
  `rows + unchanged == total` for the IAM-policy fixture.
- **MapDiff vs ListOfMaps shape coverage.** The IAM policy doc
  reproduces through the list-of-maps shape (`statement` is a
  `List<Struct>`), exercising sites 3/4. Site 5 (the pure Map-diff
  shape) needs its **own** fixture/unit case (a resource whose
  changed attribute is a `Map<String, StringEnum>` or a `Struct`
  rendered via `build_map_diff_row`) — otherwise the MapDiff path's
  fix is untested. Do not assume the IAM fixture covers site 5.
