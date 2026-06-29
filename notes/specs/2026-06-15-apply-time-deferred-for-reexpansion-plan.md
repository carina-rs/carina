# Apply-time deferred-for re-expansion: Implementation plan

<!-- derived-from ./2026-06-15-apply-time-deferred-for-reexpansion-design.md -->

Closes carina-rs/carina#3561.

This plan decomposes the design into concrete TDD tasks, each scoped
to a single file or a tight cluster of files. Every task ships with
its failing test first; no production code lands without a failing
test in the same change.

## Sequencing

The work splits into four cohesive cargo-rounds. Each round is
self-contained: it leaves the build green, the test suite green, and
the public API in a coherent (if incomplete) state. The four rounds
land in a single PR.

```
Round 1   ResolvedResource witness in carina-core
Round 2   Effect::ExpandDeferredFor variant + constructor + accessors
Round 3   Plan-time emission rule (expansion → ExpandDeferredFor when
          iterable is unresolved at plan time)
Round 4   Apply-time scheduler integration: re-expansion runs, emits
          ResolvedResource children, downstream waits/depends_on see
          them complete
```

The design's Open Questions are resolved before implementation begins
(see §Resolutions below). Plan display for `ExpandDeferredFor` lands
as part of Round 4 — silent meta-effect, the children are what the
user sees.

## Resolutions to design open questions

1. **State-only vs synthetic-generator variant.** The effect is
   *state-only*: it does not call the provider; it consults
   `applied_states`, derives a fresh expansion, and emits new effects
   into the in-flight scheduler. It rides the existing `Move` /
   `Import` / `Remove` shape — state-only, executor-internal, no
   provider involvement — so the executor seam already has a place
   to dispatch it (`Effect::as_basic()` returns `None` for it, the
   phased executor's `_ => state-only` arm picks it up). The
   "synthetic generator" framing would have implied a new executor
   subsystem; the state-only framing reuses what is there.

2. **Plan display.** `ExpandDeferredFor` does not get its own row in
   the plan tree. The children it produces at apply time are what the
   user sees. At plan time, the tree continues to display whatever
   `expand_same_config_deferred_for` produced — either pre-expanded
   children (when the iterable resolved at plan time, unchanged from
   today) or a single "deferred until apply" marker under the for-loop
   header (new — replaces the silently-stamped `Unknown(ForValuePath)`
   children). The latter case is a known-after-apply display, similar
   to how `wait` blocks render.

3. **Shrinking re-expansion.** When the apply-time re-expansion
   produces fewer children than the plan-time prediction (e.g. the
   plan predicted 2 children but the post-apply upstream has 1), the
   missing children are *not* emitted. They never reach the
   scheduler; no `Create` runs for them; nothing is in state for
   them, so no orphan reconciliation is needed in this PR. The plan
   display already labels these as "deferred until apply" in
   resolution (2), so the user is not promised a fixed count. The
   plan-time pre-expanded path (iterable knowable at plan time) keeps
   today's contract.

## Round 1 — `ResolvedResource` witness

**Goal**: a type that wraps a `Resource` whose attribute tree has
been walked and found free of any `Value::Deferred(*)` arm. Only the
basic executor's `resolve_*` helpers can construct one.

**File targets**:

- `carina-core/src/resource.rs` — new `pub struct ResolvedResource`
  with private inner field and a `pub(crate)` constructor visible to
  `executor::basic` (place the type in `resource.rs` next to
  `Resource`; the constructor lives in a small `mod resolved` whose
  `pub(crate) fn new` is the only path to the witness).
- `carina-core/src/executor/basic.rs` — `resolve_resource` and
  `resolve_resource_with_source` return
  `Result<NormalizedResource, …>` today; both grow a `resolved:
  ResolvedResource` field on `NormalizedResource` (or change
  `NormalizedResource` to wrap `ResolvedResource` directly — choose
  whichever produces the smaller diff at write time; either works).
- `carina-core/src/executor/parallel.rs`,
  `carina-core/src/executor/phased.rs`,
  `carina-core/src/executor/replace.rs` — the call sites that
  currently take `&NormalizedResource` or feed the resolved
  `Resource` to `Provider::create` / `update` switch to
  `&ResolvedResource`. The provider trait itself stays on `Resource`
  at the WIT boundary; the typestate enforces the host-side
  guarantee that we hand `provider.create` only a fully-resolved
  resource.

**Tests (red first)**:

- `resolved_resource_constructor_rejects_value_unknown`: build a
  `Resource` with one `Value::Deferred(DeferredValue::Unknown { …
  ForValuePath … })` attribute; assert the constructor returns
  `Err(SerializationError::UnknownNotAllowed { … })`. Add this test
  to the resolve helper's existing test module
  (`carina-core/src/executor/tests.rs` already covers
  `resolve_resource`; co-locate).
- `resolved_resource_constructor_rejects_resource_ref`: same shape
  with `DeferredValue::ResourceRef`, expects
  `UnresolvedResourceRef`.
- `resolved_resource_constructor_rejects_nested_deferred`: place the
  `Unknown` value inside a `List` / `Map` / `Secret` — the same
  containers `assert_fully_resolved` already walks — assert it is
  still rejected.
- `compile_fail` doctest in `carina-core/src/resource.rs`: a plain
  `Resource` cannot be coerced into `ResolvedResource`; constructing
  one outside `carina-core::executor::basic` does not compile.
  Mirror the pattern carina#3164 used for `BasicEffect`.

**Round-1 verify**: `cargo nextest run -p carina-core` + the
`compile_fail` doctest hits via `cargo test -p carina-core --doc`.

## Round 2 — `Effect::ExpandDeferredFor` variant

**Goal**: a new state-only effect variant that carries the
information needed for apply-time re-expansion.

**File targets**:

- `carina-core/src/effect.rs` — add the variant:

  ```rust
  /// Re-expand a `for opt in <upstream>.<collection> { ... }`
  /// expression against the post-apply upstream state, emitting
  /// fresh `Create` effects for the synthesised children.
  ///
  /// Emitted by the planner when the iterable's plan-time value is
  /// unresolved (upstream is a Create / Replace whose collection
  /// attribute is not yet known). The executor dispatches the
  /// effect after `upstream_binding`'s `applied_states` entry has
  /// been written by an upstream Create/Update/Replace future.
  /// State-only: does not call the provider.
  ExpandDeferredFor {
      /// Synthetic id used for plan-tree display and progress.
      id: ResourceId,
      /// The iterable's binding name (e.g. "cert").
      upstream_binding: String,
      /// The for-expression body, replayed against
      /// `applied_states[upstream_binding].attributes`.
      template: Box<DeferredForExpression>,
  },
  ```

  Update the exhaustive match arms in:

  - `Effect::as_basic()` → returns `None` (state-only, like `Move`).
  - `Effect::resource_id()` → returns `&id`.
  - `Effect::as_resource_ref()` → returns `None`.
  - `Effect::binding_name()` → returns `None` (no single binding
    name; the children carry their own bindings).
  - `Effect::explicit_dependencies()` → returns `HashSet::new()`.
  - `Effect::blocking_bindings()` → returns
    `vec![upstream_binding.clone()]`. This single line is what
    crosses the phase boundary by construction: any phase that
    contains an `ExpandDeferredFor` will look up `upstream_binding`
    in its `binding_to_idx` and find it (because we place the
    effect in the same phase as the upstream's finalization — see
    Round 4).
  - `Effect::type_str()` → `"expand_deferred_for"`.
  - Pattern matches in `state_only_or_blocking_only_effect`,
    `count_actionable_effects`, `Effect::is_wait`,
    `Effect::is_state_only` (if such a method exists; otherwise
    add it now and use it consistently — exhaustive match is the
    enforcement mechanism).

**Tests (red first)**:

- `expand_deferred_for_blocking_bindings_is_upstream_only`:
  construct an `Effect::ExpandDeferredFor { upstream_binding:
  "cert".into(), … }`; assert `blocking_bindings()` returns
  `["cert"]`.
- `expand_deferred_for_as_basic_returns_none`: assert state-only
  classification.
- `expand_deferred_for_resource_id_returns_synthetic_id`: assert
  `resource_id()` returns the synthetic id passed at construction.
- `expand_deferred_for_serde_roundtrip`: bincode / serde_json
  round-trip a plan that contains one of these — `DeferredForExpression`
  already derives the requisite traits per existing tests; the new
  variant follows the same shape. (Saved-plan path coverage; the
  variant is part of the plan file.)

**Round-2 verify**: `cargo nextest run -p carina-core`.

## Round 3 — Plan-time emission rule

**Goal**: switch
`carina-cli/src/wiring/mod.rs::expand_same_config_deferred_for` from
"always pre-expand against `current_states`" to "pre-expand when the
iterable is fully known at plan time, otherwise emit a single
`Effect::ExpandDeferredFor` deferring expansion to apply".

**File targets**:

- `carina-cli/src/wiring/mod.rs` — `expand_same_config_deferred_for`
  inspects the resolved iterable value. If the value is a
  `ConcreteValue::List` / `ConcreteValue::Map` with no `Value::Deferred`
  leaves, pre-expand as today. Otherwise:
  - Skip pre-expansion of the deferred resource's children.
  - Record the deferred expression in the output's
    `residual_deferred_for` (the existing field) the same way
    today's "iterable not found in `bindings`" branch does, *plus*
    emit one `Effect::ExpandDeferredFor` into the plan via the
    plan-construction path that consumes `residual_deferred_for`.
- `carina-cli/src/commands/plan.rs` and
  `carina-cli/src/commands/apply/mod.rs` — the sites that currently
  read `ctx.residual_deferred_for` to print "deferred until apply"
  warnings also feed the new effects into the `Plan` when
  appropriate. Today they live next to `add_state_block_effects`;
  the new emission is a sibling step.
- `carina-core/src/plan.rs` — no change required; `Plan` stores
  `Vec<Effect>` and accepts the new variant transparently.

**Tests (red first)**:

- `expand_same_config_emits_expand_deferred_for_when_iterable_is_unresolved`
  in `carina-cli/src/wiring/tests.rs`: fixture is a Replace cert +
  a `for opt in cert.domain_validation_options` block; the cert's
  current_states value is empty / does not contain a usable DVO.
  Assert the output's effect list contains exactly one
  `Effect::ExpandDeferredFor { upstream_binding: "cert", … }` and
  no pre-expanded `Effect::Create` for the children.
- `expand_same_config_pre_expands_when_iterable_is_concrete`: the
  same fixture but with the cert present in `current_states` and
  its DVO populated; assert the children are pre-expanded as today
  and **no** `Effect::ExpandDeferredFor` is emitted. This is the
  regression boundary that confirms we did not break the
  plan-time-knowable path.
- `expand_same_config_passes_through_pr3558_dependency_bindings_on_pre_expanded_children`:
  cover that the pre-expanded path still carries
  `dependency_bindings.insert(iterable_binding)` (PR #3558's
  invariant) for the same-phase race protection.

**Round-3 verify**: `cargo nextest run -p carina-cli wiring`.

## Round 4 — Apply-time scheduler integration

**Goal**: the phased executor recognises `Effect::ExpandDeferredFor`,
runs it after the upstream's `applied_states` entry is written, and
appends the synthesised children to the in-flight effect list so they
schedule as ordinary `Create`s.

**File targets**:

- `carina-core/src/executor/phased.rs` —
  - The Phase 4 dispatch loop (the legacy replace effect finalization +
    `Effect::Create` for non-CBD replaces) gets a sibling
    `if let Effect::ExpandDeferredFor { … } = effect { … }` arm.
    Dispatch is synchronous: look up
    `applied_states[upstream_binding]`, run the re-expansion
    against the resolved attributes, build a fresh `Resource` per
    collection element (mirroring `parser::ast::build_expanded_child`
    but with `substitute_attrs` consuming the *resolved* iterable
    value), wrap each in `ResolvedResource` via
    `resolve_resource(…)`, append as `Effect::Create` to the
    in-flight phase indices, and mark the `ExpandDeferredFor`
    effect complete.
  - The same arm lives in Phase 1 for first-time `Create` upstreams
    (no Replace, just a fresh Create whose collection becomes
    known once its `applied_states` entry exists). One helper
    (`fn dispatch_expand_deferred_for(...)`) is called from both
    phases; the helper is the single seam where re-expansion
    runs.
- `carina-core/src/executor/parallel.rs` — the non-phased executor
  (used by the simpler test paths) gets the same arm so the
  contract is uniform. If `parallel.rs` is reserved for paths that
  cannot reach `ExpandDeferredFor`, document why and leave the
  dispatch out; otherwise mirror the helper.
- `carina-core/src/executor/wait.rs` — no change needed.
  `Effect::Wait`'s `blocking_bindings` includes its
  `explicit_dependencies`; once the re-expansion has emitted and
  the children's `Effect::Create` completes, the wait fires
  because its dependency entry (`validation_records`) is now in
  `binding_to_idx` and `completed_indices`.
- `carina-core/src/parser/ast.rs::build_expanded_child` — extract
  the loop-variable substitution into a free function callable
  from the executor's re-expansion helper (today it is a private
  helper). The free function takes `(deferred:
  &DeferredForExpression, iterable_value: &Value)` and returns
  `Vec<Resource>`; the parser's existing call site re-uses it.

**Tests (red first)**:

- Repro test, `carina-core/src/executor/tests.rs`: build a plan
  with `[Replace cert, ExpandDeferredFor cert→validation_records,
  Wait cert_issued{depends_on=[validation_records]}]`. Run through
  the phased executor with a mock provider whose `cert` create
  returns a `State` whose `domain_validation_options` is a list of
  one element with `resource_record.name = "_dns_record_name_"` and
  `resource_record.value = "_dns_record_value_"`. Assert the event
  trace is:
  ```
  Replace cert  → SUCCESS
  ExpandDeferredFor → emits Effect::Create validation_records[0]
  Create validation_records[0] → SUCCESS (uses the resolved
    "_dns_record_name_" value)
  Wait cert_issued → SUCCESS (sees validation_records satisfied)
  ```
  No `Create validation_records[0]` may appear before `Replace cert`
  completes.
- Negative test:
  `expand_deferred_for_emits_zero_children_when_collection_is_empty`.
  Mock provider returns an empty `domain_validation_options`; the
  effect completes, emits zero children, and the wait either fires
  (if `validation_records`'s dependency is vacuous) or hits its
  timeout cleanly.
- Cancellation test: the apply is cancelled mid-flight after
  `cert` create completes but before `ExpandDeferredFor`
  dispatches. Assert the effect is reported as skipped, not
  failed.
- Plan-tree display test: `carina-cli/src/commands/plan.rs`
  snapshot fixture for the new "deferred until apply" rendering of
  the for-loop body. Snapshot updates via `cargo insta accept`
  belong in this PR.

**Round-4 verify**: `cargo nextest run -p carina-core -p carina-cli`,
`cargo test --workspace --doc`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`bash scripts/check-*.sh`.

## Cross-cutting concerns

- **Saved plan compatibility.** `Effect::ExpandDeferredFor` is part
  of the `Plan` serialization. The "no backwards compatibility"
  rule applies — saved plans from older binaries that did not have
  this variant are not portable across the change. No migration
  shim.
- **Provider boundary.** The variant never crosses the WIT
  boundary; it is dispatched host-side. No `carina-provider-aws` /
  `carina-provider-awscc` work is in scope. After this PR merges,
  no provider rev bump is needed unless an unrelated reason calls
  for one.
- **PR #3558's `dependency_bindings.insert(iterable_binding)`
  remains** for the plan-time-knowable path. The new variant
  handles the plan-time-unknowable path. Together they cover both
  branches of the design.
- **`feedback_codegen_verification`** does not apply — this PR
  touches no schema codegen. The `scripts/check-*.sh` invariants
  still run.

## Out of scope

- `for_each` over arbitrary maps that are not derived from a
  managed resource attribute.
- Datasource-deferred attributes (`for opt in
  datasource.collection`). The fix shape is similar in spirit
  (re-expand after the datasource refresh), but the `applied_states`
  hook is not the right seam — datasources go through `Effect::Read`
  with its own state seam. A follow-up issue can pick this up if it
  surfaces.
- Upstream-stack-deferred attributes (`for opt in
  upstream.collection`). These already have a resolution model via
  `upstream_state` reads and `wait` blocks against the upstream
  stack's apply.

## Test-plan summary

By the time Round 4 lands, the regression coverage is:

- 4 typestate tests in `carina-core` (Round 1).
- 4 effect-shape tests in `carina-core` (Round 2).
- 3 plan-time emission tests in `carina-cli/wiring` (Round 3).
- 3 apply-time dispatch tests in `carina-core/executor` (Round 4).
- 1 plan-tree snapshot in `carina-cli` (Round 4).
- PR #3558's existing tests stay green untouched.
