# Design: `moved` into a `for`-loop expansion address must not emit `Move` + same-address `Create`

<!-- derived-from ./2026-05-18-same-config-read-iterable-resolution-design.md -->
<!-- derived-from ./2026-05-18-loop-var-field-access-resolution-design.md -->

Status: proposed (design PR; implementation PR follows in strict order)
Issue: carina#3141
Predecessors (merged): carina#3132 / #3136 / #3137 / #3140

## Problem

When a `moved` block migrates a previously *named-binding* resource into
the address produced by a same-config `for` expansion, `carina plan`
(and `carina apply`) emit **two effects against the same target
address**: a `Move` from the old name *and* a `Create` of the new name.
The `Create`'s attributes are byte-equal to the live state being moved.
This is internally inconsistent — after the `Move` the target address is
occupied by the migrated state, so a same-address `Create` either
no-ops, fails, or clobbers the just-moved resource via the provider API.

Concrete repro (carina-rs/infra registry usecase, after #3137/#3140):

```crn
# usecases/registry/acm.crn
let cert = aws.acm.Certificate { domain_name = domain_name; validation_method = dns; directives { provider = us } }
for opt in cert.domain_validation_options {
  aws.route53.RecordSet {
    hosted_zone_id   = zone.id
    name             = opt.resource_record.name
    type             = cname
    ttl              = 300
    resource_records = [opt.resource_record.value]
  }
}
```

```crn
# registry/dev/registry/moved.crn
moved {
  from = aws.route53.RecordSet 'r.validation_record'
  to   = aws.route53.RecordSet 'r._domain_validation_options[0]'
}
```

Observed: `Plan: 1 to add, 0 to change, 0 to destroy, 1 to move.` with
both lines targeting `r._domain_validation_options[0]`.

Expected: `Plan: 0 to add, 0 to change, 0 to destroy, 1 to move.` (or, if
the moved state has drifted from desired, a single `Move` + `Change` for
the drift — never a `Create`).

## Root cause

Both the plan path (`wiring/mod.rs::create_plan_from_parsed_with_upstream`)
and the apply path (`apply/mod.rs::run_apply_locked`) run the same
ordered sequence:

1. `materialize_moved_states` (`wiring/mod.rs:1392`, apply `:927`) — for
   each `moved` block whose `from` exists in the state file, **moves the
   in-memory `current_states` entry** from `from` to `to`
   (`current_states.remove(from)` → `state.id = to` →
   `current_states.insert(to, state)`), and records the pair in
   `moved_pairs`. It mutates `current_states`/`prev_explicit`/`saved_attrs`
   only — **it does not rewrite the on-disk `state_file`**.
2. `expand_same_config_deferred_for` (`wiring/mod.rs:1509`, apply
   `:1002`) — expands the same-config deferred-for loop, *creating the
   desired resource `r._domain_validation_options[0]`* and adding its id
   to `new_child_ids`. The `to` address of the `moved` block did not
   exist as a desired resource until this step.
3. `refresh_resource_set` (`wiring/mod.rs:1525`, apply `:1023`) — for
   every id in `new_child_ids`, calls
   `read_with_retry(provider, &resource.id, identifier)` and then
   **`current_states.insert(id, state)` unconditionally** (`:1974`).
   `identifier` comes from
   `state_file.get_identifier_for_resource(resource)`
   (`carina-state/src/state/mod.rs:163`), which looks up the **raw
   `resource.id.name_str()`** — i.e. `r._domain_validation_options[0]`.
   The on-disk state file still has the entry under the *old* name
   `r.validation_record` (step 1 only touched in-memory maps), so the
   lookup returns `None`. The provider is then read with no identifier
   and returns `not_found`. That `not_found` is inserted into
   `current_states[r._domain_validation_options[0]]`, **destroying the
   migrated state that step 1 placed there**.
4. Differ: desired `r._domain_validation_options[0]` (from expansion)
   vs `current_states[r._domain_validation_options[0]]` (= `not_found`)
   → **`Create`**.
5. `add_state_block_effects` (`wiring/mod.rs:1634`, apply `:1103`) — from
   `moved_pairs` adds `Effect::Move { from, to }`. It calls
   `suppress_delete.insert(to)` but **never `suppress_create`** for a
   move target (`:1791-1797`).

Net: a `Move` (step 5) and a `Create` (step 4) for the same address.

The bug is the **interaction** between two individually-correct
mechanisms that the #3132 series did not test together:
`materialize_moved_states` migrates state *before* the loop expansion
exists, and `refresh_resource_set` re-reads the expanded child *after*,
with no awareness that the child address is a `moved` target. Both write
the same `current_states` key; last-writer (refresh) wins, and refresh
writes `not_found`.

This is a [[feedback_unit_test_path_is_not_apply_path]] /
[[feedback_root_cause_over_per_site_patch]] case: the fix must address
the ordering interaction, not post-process the wrong effects.

## Options considered

The selection axis is **long-term maintainability × type safety**
([[feedback_long_term_and_type_safety]] /
[[feedback_type_safety_over_runtime_checks]]), not short-term diff size.
"Type safety" here means *the plan/apply parity invariant is enforced by
the type system* — a divergence must be a compile error, not something a
reviewer or a test has to catch. The #3132 series repeatedly paid for
hand-maintained plan/apply parity
([[feedback_unit_test_path_is_not_apply_path]]); a fix that re-creates
that hazard is rejected on those grounds even if it is the fewest lines.

### Option A — exclude moved targets from the post-expansion refresh, via a typed view

The migrated state placed by `materialize_moved_states` (step 1) must
survive the post-expansion refresh (step 3) for any child whose id is a
`moved_pairs` `to`. The decision *which expanded children are
refreshable* is computed **once**, where the expansion and the
`moved_pairs` are both already known, and carried in the typed result
that both the plan and apply paths already consume —
`DeferredForExpansion`:

```rust
pub struct DeferredForExpansion {
    pub sorted_resources: Vec<Resource>,
    pub residual_deferred_for: ...,
    pub new_child_ids: HashSet<ResourceId>,
    /// Expanded children that are safe to re-read from the provider.
    /// Excludes any child that is a `moved` target (its migrated state
    /// must not be overwritten by a `not_found` provider read). Computed
    /// once from `new_child_ids` minus the `moved_pairs` `to`s.
    pub refreshable_child_ids: HashSet<ResourceId>,
}
```

Both call sites then refresh `refreshable_child_ids` — they never see
`moved_pairs` in the refresh decision and cannot diverge:

```rust
refresh_resource_set(
    provider, &multi,
    sorted_resources.iter().filter(|r| exp.refreshable_child_ids.contains(&r.id)),
    ...,
);
```

- **Pro (type safety, primary):** the moved-exclusion lives in exactly
  one place (`expand_same_config_deferred_for`, the function both paths
  already call). The plan and apply paths consume the same typed field;
  if one path is changed to refresh a different set, that is a type
  mismatch against `DeferredForExpansion`, not a silently-passing
  divergence. The parity invariant is enforced by the compiler. This is
  the only option that structurally closes the
  [[feedback_unit_test_path_is_not_apply_path]] hazard rather than
  re-paying it.
- **Pro (long-term):** root fix — it removes the step-3 overwrite of the
  step-1 migration at the point the two mechanisms collide, and it does
  so by *narrowing the type* of "what may be refreshed" rather than
  adding a runtime guard at each consumer. A future change to refresh
  timing inherits the moved-exclusion for free instead of having to
  re-derive it.
- **Con:** a moved target is not live-refreshed, so post-move provider
  drift is diffed against the migrated state, not live values. This is
  **identical to every other `moved` block** in carina today
  (`materialize_moved_states`' documented contract: "the differ compares
  desired(to) against actual(from)"). It is not a regression; it is
  consistency with existing `moved` semantics.
- **Counter-consideration (short-term cost only):** adds one field to
  `DeferredForExpansion` and threads its computation into
  `expand_same_config_deferred_for`. Slightly more than a per-site
  filter, but the cost buys the compiler-enforced parity above.

### Option A′ — hand-written filter at each call site (rejected)

The minimal-diff variant: at the plan call site *and* the apply call
site, build `moved_pairs.to` into a `HashSet` and add
`.filter(|r| !moved_to.contains(&r.id))` to the child iterator.

- Fewer lines than A.
- **Rejected:** the filter is duplicated across the plan and apply call
  sites. If a later change touches one site's filter and not the other,
  **the code still compiles** and the plan/apply effect sets silently
  diverge — the exact [[feedback_unit_test_path_is_not_apply_path]] /
  [[feedback_type_safety_over_runtime_checks]] failure the #3132 series
  kept hitting. Short-term diff size does not outweigh re-introducing a
  type-unsafe parity hazard ([[feedback_long_term_and_type_safety]]).
  A′ is A's payload without A's type safety; documented only to record
  why "just add a filter" was not chosen.

### Option B — refresh, then re-apply the migrated state on top (rejected)

Let `refresh_resource_set` run, then for each `moved_pairs` `to` in
`new_child_ids`, re-insert a pre-refresh snapshot of the migrated state.

- **Rejected:** same outcome as A but issues a provider read that is
  *guaranteed useless* (no identifier → `not_found`) and then discards
  its result. Strictly dominated by A — wasted round-trip, more steps,
  same type-safety story as A′ (still per-site). No reason to choose it.

### Option C — make the refresh use the moved `from` identifier (rejected)

Thread `moved_pairs` into `refresh_resource_set` so a child whose id is
a `moved` `to` is read using the `from` name's identifier, targeting the
real live resource. This is the *most* correct (post-move drift becomes
a real `Change`).

- **Rejected:** widest blast radius. Changes the signature and
  semantics of `refresh_resource_set` — a `pub(crate)` helper called
  from 3+ sites including phase-1 refresh and the `--refresh=false`
  path — and re-opens the question the #3132 series deliberately
  deferred (whether a moved resource's identity is re-keyed in the
  state file before refresh). It pushes hardest against the
  "re-sort / SimHash reconcile / moved matching must stay stable"
  invariant the predecessor design named the highest-risk area. The
  extra correctness (live post-move drift) is **not required by #3141**
  ("Expected" is `Move` + optionally `Change` *from the moved state*,
  which A delivers). Paying the largest risk against the highest-risk
  invariant for correctness #3141 does not ask for is the wrong
  long-term trade ([[feedback_long_term_and_type_safety]]). If live
  post-move drift detection is ever wanted it is a separate, later
  concern on its own issue ([[feedback_scope_discipline]]).

## Decision

**Adopt Option A (typed view).**

Rationale, weighing long-term maintainability and type safety
([[feedback_long_term_and_type_safety]] /
[[feedback_type_safety_over_runtime_checks]]):

- **Type safety is the deciding axis.** A enforces the plan/apply parity
  invariant in the type system (one `DeferredForExpansion.refreshable_child_ids`
  field both paths consume); A′/B leave parity to per-site discipline
  that compiles even when wrong — the recurring
  [[feedback_unit_test_path_is_not_apply_path]] failure. A is the only
  option that *structurally* closes that hazard.
- **A is the root fix, not a bandaid.** It removes the step-3 overwrite
  at the collision point, not via post-processing effects (rejected
  #3141 "direction 1") or rejecting the plan (#3141 "direction 3",
  fallback only).
- **A's drift behavior is consistent with all existing `moved` blocks**;
  C would make loop-`moved` behave differently (live-refresh) from plain
  `moved` — an inconsistency, not an improvement, and at the highest
  blast radius.
- **Short-term cost is a counter-consideration only.** A′ is fewer lines
  but type-unsafe; B is dominated; C is over-scoped. The one extra
  `DeferredForExpansion` field A adds is the price of compiler-enforced
  parity and is worth it long-term.

### Residual structural risk (explicitly not folded in)

Option A stops the *symptom's* collision point but **does not remove the
underlying fragility**: `current_states: HashMap<ResourceId, State>` is
written by multiple pipeline stages (`materialize_moved_states`,
`apply_anonymous_to_named_renames`, `refresh_resource_set`, phase-2
data-source read) with **order-dependent unconditional `insert`** —
last-writer-wins on a shared key. moved×loop is the instance #3141
surfaced; the same class can recur for other stage pairs that contend
the same key (e.g. anonymous→named rename × loop, import × loop). A only
narrows the type of "what refresh may touch"; it does not make the
stage-ordering safe by construction.

This is **deliberately not fixed here** ([[feedback_scope_discipline]]):
#3141 is one instance and A is its root fix. Per
[[feedback_root_cause_over_per_site_patch]], the stage-ordering
last-writer-wins structure is recorded as a **future tracker candidate**
— if a second instance of this class appears, file the tracker rather
than adding another per-pair carve-out. The implementation PR should
leave a code comment at the `DeferredForExpansion` computation pointing
here so the structural debt is discoverable, not silently absorbed.

## Implementation plan (separate PR, after this design PR merges)

[[feedback_design_before_implementation_in_pr]] —
[[feedback_no_premature_close]]: this design PR uses `refs #3141`;
`Closes #3141` goes on the implementation PR only.

1. **Typed view** (`carina-cli/src/wiring/mod.rs`,
   `expand_same_config_deferred_for` + `DeferredForExpansion`): add the
   `refreshable_child_ids: HashSet<ResourceId>` field, computed once as
   `new_child_ids` minus the `moved_pairs` `to`s. `moved_pairs` is
   already materialized before this call on both paths
   (`materialize_moved_states` runs first), so thread it (or just the
   set of `to`s) into `expand_same_config_deferred_for` as an input.
   Leave the code comment pointing at the "Residual structural risk"
   section so the stage-ordering debt is discoverable.
2. **Both call sites consume the typed field, not a hand filter.** Plan
   path (`wiring/mod.rs` ~`:1518`) and apply path
   (`apply/mod.rs` ~`:1019`) both refresh
   `sorted_resources.iter().filter(|r| exp.refreshable_child_ids.contains(&r.id))`.
   Neither site computes the moved-exclusion itself; a divergence is a
   type mismatch against `DeferredForExpansion`, not a silently-passing
   parity bug ([[feedback_unit_test_path_is_not_apply_path]] /
   [[feedback_type_safety_over_runtime_checks]]). Explicitly **do not**
   re-introduce a per-site `moved_to.contains` filter (rejected Option
   A′).
3. **`add_state_block_effects` audit**: confirm that with step 1/2 in
   place the differ no longer produces the `Create` (because
   `current_states[to]` is now the migrated state, not `not_found`), so
   the existing `Move` from `moved_pairs` stands alone. No change to
   `add_state_block_effects` itself is expected; if a residual
   same-address `Create` survives, that is a second defect and gets its
   own issue rather than a `suppress_create` carve-out here
   ([[feedback_root_cause_over_per_site_patch]]).
4. **Tests** (the named #3141 reproduction must be the acceptance
   gate):
   - `wiring/tests.rs`: a directory fixture
     ([[feedback_directory_scoped_features]]) with `acm.crn` (the
     `let cert` + same-config `for opt in cert.domain_validation_options`
     loop) + `moved.crn` (`r.validation_record` →
     `r._domain_validation_options[0]`) + a state file holding
     `r.validation_record` with attributes byte-equal to what the loop
     re-emits. Assert the plan is exactly one `Move` and **zero**
     `Create` for `r._domain_validation_options[0]`
     (`Plan: 0 add, 0 change, 0 destroy, 1 move`).
   - A drift variant: state `r.validation_record` differs from desired in
     one non-create-only attribute → assert `Move` + `Change`, still no
     `Create`.
   - `apply/tests.rs`: the parity twin, asserting the apply effect set
     matches the plan effect set for the same fixture
     ([[feedback_unit_test_path_is_not_apply_path]]).
   - A regression guard that a *non-moved* expanded child is still
     refreshed (the filter must not over-broadly skip refresh).
5. **Verify cycle** per CLAUDE.md: `cargo nextest run -p carina-cli` →
   `cargo test --workspace --doc` → `cargo clippy --workspace
   --all-targets -- -D warnings` → `bash scripts/check-*.sh`
   ([[feedback_local_green_is_not_ci_green]]).

## Risks / open questions

- **`--refresh=false` path.** With refresh disabled the post-expansion
  block restores children from the cached state file
  (plan `:1537-1539`). The same `moved` target then resolves against the
  *cached* (old-name) entry; since `materialize_moved_states` already
  moved the in-memory entry to the new key, the `--refresh=false`
  child-restore must not re-overwrite it with the old-name lookup. The
  implementation PR must add a `--refresh=false` fixture asserting the
  same `Move`-only outcome (predecessor design pinned the analogous
  `--refresh=false` behavior; do the same here, not incidentally).
- **Re-sort stability.** Option A does not touch the topological re-sort
  inside `expand_same_config_deferred_for`; it only filters the refresh
  input. The predecessor design's "re-sort must not perturb
  SimHash/`moved` matching" invariant is untouched, but the
  implementation PR must add an explicit assertion that the `Move`
  pairing is unchanged when the loop also expands other (non-moved)
  children.
- **Multiple `domain_validation_options` entries.** Real ACM certs have
  >1 validation option (apex + SAN). The fixture should cover ≥2 entries
  so the per-index `moved` (`[0]`, `[1]`, …) and the filter set are
  exercised, not just the single-entry happy path.
- **Not a renderer/phantom change.** This is pipeline ordering only; no
  detail-row/diff renderer is touched ([[feedback_state_enum_phantom_diff_is_core_not_provider]]
  scoping note — reviewers should not look for a renderer defect here).
