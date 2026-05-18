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

### Option A — exclude moved targets from the post-expansion refresh

In the `if !new_child_ids.is_empty()` block (plan `:1518-1536`, apply
`:1019-1035`), filter the `children` iterator so any child whose id is a
`moved_pairs` `to` is **not** passed to `refresh_resource_set`.

- Pro: minimal; one-line filter at the two call sites; the migrated
  state placed by step 1 survives untouched, so the differ compares
  desired vs the *moved* state and emits `Move` + (`Change` iff drift)
  — exactly #3141's "Expected".
- Con: the expanded child genuinely *should* be refreshed for its live
  attributes in the non-moved case; this skips refresh for moved
  targets, so if the moved resource drifted in the provider, the differ
  sees the *state-file* values, not live values. This is acceptable:
  carina's moved semantics already diff desired against the moved state
  entry (that is the documented contract of
  `materialize_moved_states` — "the differ compares desired(to) against
  actual(from)"); the moved entry is the same baseline a non-loop
  `moved` already uses. Drift detection for a `moved` target is no worse
  than it is for any other `moved` block today.
- Type-safety note: `moved_pairs` is already `Vec<(ResourceId,
  ResourceId)>` in scope at both call sites; the filter is a
  `HashSet<&ResourceId>` membership test. No new escape hatch.

### Option B — refresh, then re-apply the migrated state on top

Let `refresh_resource_set` run, then after it, for each
`moved_pairs` `to` that is in `new_child_ids`, re-insert the migrated
state (snapshot it before refresh).

- Pro: keeps refresh uniform.
- Con: throws away the value `read_with_retry` returned and re-installs
  the pre-refresh snapshot — i.e. it is Option A with extra steps and a
  redundant provider round-trip. The provider read here is *guaranteed
  useless* (no identifier → `not_found`), so paying for it then
  discarding it is pure waste. Strictly dominated by A.

### Option C — make the refresh use the moved `from` identifier

Thread `moved_pairs` into `refresh_resource_set` so that when a child id
equals a `moved` `to`, the identifier is looked up under the `from`
name (`get_identifier_for_resource` on a synthetic `from`-named
resource), so the provider read targets the *real* live resource.

- Pro: most "correct" — the expanded-and-moved child gets genuine live
  attributes, so post-move drift *is* detected as a `Change`.
- Con: widest blast radius. Changes `refresh_resource_set`'s signature
  and semantics (a `pub(crate)` helper called from 3+ sites including
  phase-1 refresh and the `--refresh=false` path). It also re-introduces
  a question the #3132 series deliberately deferred: whether a moved
  resource's identity should be re-keyed in the state file before
  refresh at all. Higher risk against the "re-sort / SimHash reconcile /
  moved matching must stay stable" invariant the predecessor design
  flagged as the highest-risk area.

## Decision

**Adopt Option A.**

Rationale, weighing long-term maintainability and type safety
([[feedback_long_term_and_type_safety]]):

- A is the *root* fix, not a bandaid: it removes the step-3 overwrite of
  the step-1 migration at the exact point the two mechanisms collide,
  rather than post-processing effects (the rejected #3141 "direction 1")
  or rejecting the plan (#3141 "direction 3", a fallback only).
- A's drift behavior is **identical to every other `moved` block** in
  carina today: the differ already diffs desired against the migrated
  state entry, not a fresh provider read, for non-loop `moved`. A simply
  extends that existing, documented contract to loop-expansion targets.
  C would make loop-`moved` *behave differently* (live-refresh) from
  plain `moved` — an inconsistency, not an improvement.
- B is strictly dominated by A (same outcome, wasted provider call).
- C's extra correctness (post-move drift as `Change`) is not required by
  #3141 ("Expected" is `Move` + optionally `Change` *from the moved
  state*, which A delivers) and costs the most blast radius against the
  highest-risk invariant. If post-move live-drift detection is ever
  wanted, it is a separate, later concern filed on its own — not folded
  in here ([[feedback_scope_discipline]]).

## Implementation plan (separate PR, after this design PR merges)

[[feedback_design_before_implementation_in_pr]] —
[[feedback_no_premature_close]]: this design PR uses `refs #3141`;
`Closes #3141` goes on the implementation PR only.

1. **Plan path** (`carina-cli/src/wiring/mod.rs`, the
   `if !new_child_ids.is_empty()` block ~`:1518`): build a
   `HashSet<&ResourceId>` of `moved_pairs` `to`s; the `children()`
   closure additionally filters `!moved_to.contains(&r.id)`. Resources
   that are both an expanded child *and* a moved target keep the
   migrated state from `materialize_moved_states`.
2. **Apply path** (`carina-cli/src/commands/apply/mod.rs`, the
   `if !new_child_ids.is_empty()` block ~`:1019`): the identical filter.
   Plan/apply must not diverge ([[feedback_unit_test_path_is_not_apply_path]]).
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
