# Deferred for-loop replace as a typed effect: Design

<!-- constrained-by ./2026-06-15-apply-time-deferred-for-reexpansion-design.md -->

## Status

Design proposal for carina-rs/carina#3599.

This document does not implement the change. It records the Effect-model
seam needed so that the "deferred for-loop iteration is a planned
replace" case is represented as one Effect — not as a `Delete` plus an
`ExpandDeferredFor` (which this design also renames to `DeferredCreate`)
that downstream consumers must re-pair themselves.

Implementation lands in a follow-up PR after this design merges.

## Problem statement

<!-- derived-from #status -->

carina#3599 reports that `carina apply` aborts state writeback with

```
writeback planned both an upsert and a cleanup for the same resource id:
aws.route53.RecordSet.registry_publish.validation_records[0]
(likely a moved-block `from` colliding with a desired-side resource)
```

when the same apply destroys a deferred for-loop iteration *and*
successfully creates a fresh iteration of the same logical address.

The reproduction is `carina-rs/infra` PR #172 post-merge: a `+/-`
replace of `validation_records[*]` deletes the pre-apply
`validation_records[0]` CNAME (its underlying cert was deleted
out-of-band) and re-creates `validation_records[0]` from the new cert's
`domain_validation_options`. Both halves finished at the AWS level —
the old record was destroyed at 12:32, the new one was created at
12:35, and ACM validated the new cert at 12:35:26 — but the writeback
collision aborted the state save and rolled the in-memory state back to
the pre-apply baseline, leaving the entire 4-resource successful apply
(LB, cert, alias record, new validation record) absent from
`carina.state.json` and live on AWS.

That failure mode is exactly what carina#3551 / awscc#456 were meant to
close. We regressed against it because the new apply-time deferred-for
re-expansion path (carina#3561, merged as #3566) introduced a second
source of writebacks against the same resource id that the existing
collision detector cannot disambiguate from a misuse.

### Why the collision fires

The plan for a deferred for-loop whose iteration `validation_records[0]`
existed in pre-apply state but whose iterable will change at apply time
looks like this at the Effect level (today):

```text
Effect::Delete { id: validation_records[0], identifier: "Z08...|_<old-hash>...|CNAME", ... }
Effect::ExpandDeferredFor { id: <synthetic>, upstream_binding: "cert",
                            template: <DeferredForExpression> }
```

The `Delete` comes from `carina-core::differ::plan::create_plan`'s
orphan detector (`carina-core/src/differ/plan.rs:417-471`): because the
deferred-for body has not been materialized yet, the pre-existing
`validation_records[0]` is not in `desired_ids` and is treated as an
orphan. The `ExpandDeferredFor` is emitted separately by
`add_apply_time_reexpansion_effects` in
`carina-cli/src/wiring/mod.rs:2704-2708`.

At apply time, the executor expands `ExpandDeferredFor` synchronously
(`carina-core/src/executor/parallel.rs:695-749`,
`expand_deferred_for_effects` in
`carina-core/src/executor/expand.rs:52-79`) into one
`Effect::Create(resource)` per materialized child and appends them to
`runtime_synthesized_resources`. A successful Create then leaves an
applied state under the same `validation_records[0]` id.

`build_state_after_apply` (`state_writeback.rs:706`) calls
`decompose` (`state_writeback.rs:649-704`). `decompose` runs Phase 1
(`sorted_resources` + `runtime_synthesized_resources`) which registers
`validation_records[0]` as an *upsert*, then Phase 2 (`plan.effects()`)
which registers `validation_records[0]` as a *cleanup* because the
plan contains a `Delete` for that id. `WritebackPlan::add_cleanup`
(`state_writeback.rs:631-637`) refuses the second write and returns
`UpsertCleanupOverlap`, which `finalize_apply` propagates as a fatal
`AppError::Validation` *before* persisting any of the run's successful
state.

The collision detector is doing exactly what it was built for. The
detector's premise is that "an upsert and a cleanup against the same
id is structural evidence that a `moved` block's `from` collided with a
desired-side resource" — true for a hand-written
`moved { from = X to = Y }` where `X` is still in desired, false for a
deferred for-loop iteration whose `[i]` index is reused across applies
intentionally. Both shapes look identical at the Effect level today;
the writeback path cannot distinguish them.

### Why three other consumers have already had to pair the same Effects

`carina-core/src/plan_tree.rs:58-89` (`paired_deferred_for_siblings`)
walks the plan to pair each `Effect::ExpandDeferredFor` with sibling
`Effect::Delete`s whose `binding` matches a numeric-index of the
template binding (`validation_records[0]` against template binding
`validation_records`). The pairing exists so plan display can render
`+/-` for a deferred replace instead of separate "destroy" and "add"
rows.

`deferred_summary_for_plan` (`plan_tree.rs:97-148`) re-runs the same
pairing to classify each `ExpandDeferredFor` as `Replace` or `Add` for
the post-plan summary line.

`child_render_items` (`plan_tree.rs:28-50`) consumes the pairing to
suppress the `Delete` row from the visible tree once it has been
absorbed into a `PairedDeferredFor`.

That is three consumers re-deriving the same pairing today. The
writeback collision detector is the fourth consumer of the same
implicit relationship — and the one that turns a missing pairing into
a fatal abort instead of a redundant display row. Every new consumer
(TUI snapshots, IAM preflight reasoning, progress event labeling)
will need to re-implement the same pairing or risk the same class of
bug. The pairing is not consumer-side display polish; it is the
logical identity of a deferred replace, and it belongs in the
`Effect` model.

## Decision

<!-- derived-from #problem-statement -->

Two changes, taken together as one design step:

1. **Rename `Effect::ExpandDeferredFor` → `Effect::DeferredCreate`.**
2. **Introduce `Effect::DeferredReplace`** that fuses what the planner
   currently emits as `Effect::Delete { id: <pre-apply iteration> }`
   (orphan-detected) plus `Effect::ExpandDeferredFor { ... }`
   (apply-time re-expansion target) into a single typed effect.

The planner merges the two during plan construction; downstream
consumers (executor, writeback, display, TUI, progress) match exactly
one variant for the "deferred replace" shape.

### Naming convention: `Deferred<X>`

The rename establishes a convention for every effect emitted by a
deferred for-loop whose iterable is not knowable at plan time. Each
such effect is named `Deferred<existing-effect-name>` and parallels
its eager counterpart:

| Eager effect       | Deferred counterpart   | When emitted                                                |
| ------------------ | ---------------------- | ----------------------------------------------------------- |
| `Effect::Create`   | `Effect::DeferredCreate` | iterable unresolved at plan time, no pre-apply iterations    |
| `Effect::Replace`  | `Effect::DeferredReplace` | iterable unresolved at plan time AND pre-apply iterations exist that the planner cannot prove will be reused |

This is preferable to the original `ExpandDeferredFor` name on three
counts. First, `Expand` is implementation vocabulary (the executor
"expands" the template); the eager-effect names describe what the
effect does to the world, and the deferred names should match. Second,
it makes the relationship between `DeferredReplace` and the existing
`Replace` self-explanatory — both are "destroy + create against the
same logical address", and `DeferredReplace` is the version that
defers half of the work until apply-time upstream resolution. Third,
if a future feature needs `Effect::DeferredUpdate` (a deferred-for
iteration that is in-place updated against a new upstream value) the
naming slot is already reserved; we are not picking a name today, but
we are not closing the door on one either. `DeferredDelete` is *not*
needed: a deferred for-loop whose body is removed from the desired
config (or whose iterable resolves to empty) is covered by the
existing orphan-delete path emitting plain `Effect::Delete`s.

### `Effect::DeferredReplace`

```rust
/// A `Effect::Delete` (against a pre-apply iteration of a deferred
/// for-loop) paired with the `Effect::DeferredCreate` that will
/// re-create iterations of the same template at apply time. Emitted
/// by the planner when a deferred-for's iterable is not knowable at
/// plan time AND the previous apply materialized iteration(s) that
/// are not in the desired set today.
///
/// At the AWS / provider level this is a destroy of the old physical
/// resource and a create of the new physical resource against the
/// same logical address. The two halves intentionally share a
/// `ResourceId` (e.g. `validation_records[0]`), so writeback must
/// recognise it as one replace, not two writes that collide. The
/// relationship to `Effect::Replace` is the same as the relationship
/// of `Effect::DeferredCreate` to `Effect::Create`: same intent at
/// the resource-level, but the planner cannot enumerate the
/// `to`-side cardinality until apply time, so the eager
/// single-resource shape (`to: Resource` + `from: Box<State>`) does
/// not fit.
DeferredReplace {
    /// Pre-apply iteration(s) being destroyed. Keyed by the same
    /// `ResourceId` that will be re-used by the synthesised children.
    /// Carries the same payload as the `Delete` it absorbed
    /// (identifier, directives, binding, dependencies,
    /// explicit_dependencies) so the executor's delete path is
    /// unchanged.
    deletes: Vec<DeferredReplaceDelete>,
    /// The deferred-for expression to replay against post-apply
    /// upstream state. Same payload as `DeferredCreate`.
    id: ResourceId,
    upstream_binding: String,
    template: Box<DeferredForExpression>,
},
```

`DeferredReplaceDelete` carries `(id, identifier, directives, binding,
dependencies, explicit_dependencies)` — the exact fields the
`Effect::Delete` variant has today, lifted into a struct so the
deletes slot has a name and can grow new fields without re-shaping
`DeferredReplace`.

`Effect::DeferredCreate` (the renamed variant) is retained for the
pure-add case: a deferred-for whose iterable is unresolved and that
has no pre-apply iterations in state. `Effect::Delete` continues to
handle every non-deferred-for orphan. The new variant exists only to
fuse the two into one effect when both are present for the same
template.

### Why not extend `Effect::Replace` instead?

`Effect::Replace { id, from: Box<State>, to: Resource, ... }` carries
a single `from` state and a single `to` resource. The deferred
for-loop case is N-to-M at the iteration level (N pre-apply
iterations destroyed, M new iterations created where M is not known
until apply time), so neither `from` nor `to` fits its eager shape.
The two effects also take different paths through the executor —
`Replace` dispatches the delete and create halves through the basic
executor's existing provider arms, while a deferred replace's create
half must go through the same synchronous template-expansion path
that `DeferredCreate` uses. The distinction is necessary, not
incidental.

### Apply semantics

At apply time the executor:

1. Schedules each `DeferredReplaceDelete` like an `Effect::Delete`:
   the provider runs the delete, success populates
   `successfully_deleted`, the freed binding-set is updated.
2. After the deletes succeed (and after `upstream_binding`'s effect
   completes), materialises the `template` against the post-apply
   upstream value via the renamed `materialize_deferred_create`
   helper and pushes the synthesised `Effect::Create` children into
   the same `runtime_synthesized_resources` list `DeferredCreate`
   uses today.

Both halves use the existing execution mechanisms — no new provider
contract, no new scheduling primitive. The seam is in the planner and
in the writeback decomposer; the executor's per-half code paths are
preserved.

If a delete half fails, the create half does not run (same ordering
as today between an `Effect::Delete` and a downstream
`Effect::DeferredCreate` that depends on it via the upstream
binding). If the create half's upstream-binding effect fails, the
deletes still run — that matches today's behavior where
`materialize_deferred_create` returns `UpstreamBindingMissing` and
the deferred children never materialize but the orphan deletes are
unchanged. Either way the writeback sees a coherent state.

### Writeback semantics

`decompose` adds a third arm to the Phase 2 walk:

```rust
Effect::DeferredReplace { deletes, .. } => {
    for delete in deletes {
        if successfully_deleted.contains(&delete.id) {
            // Same id will appear in `runtime_synthesized_resources`
            // (and thus in Phase 1's upserts) if the create half
            // succeeded. The deferred replace is one operation, so
            // either:
            //   - the upsert wins (create succeeded → record the
            //     new state under the same address), or
            //   - no upsert exists (create failed) → emit the
            //     cleanup as today.
            if !wb.upserts.contains_key(&delete.id) {
                wb.add_cleanup(delete.id.clone())?;
            }
        }
    }
}
```

The "skip cleanup when an upsert exists" branch is the only behavior
change vs. the current code, and it fires only inside the
`DeferredReplace` arm — i.e. only when the planner has explicitly
classified the destroy+create pair as one replace. The existing
`Effect::Delete` arm is untouched and continues to error on a real
`moved`-block collision (where `from != to` so the destination upsert
is under a different id and no suppression applies).

The collision detector's premise — "an upsert against the same id as
a queued cleanup is structurally suspicious" — stays correct for the
two variants where it was always correct (`Effect::Delete`,
`Effect::Move { from, .. }`). It just no longer mis-fires on the
deferred-replace shape, because that shape is now a different variant.

### Display / TUI semantics

`plan_tree.rs`'s `paired_deferred_for_siblings`,
`deferred_summary_for_plan`'s pairing pass, and `child_render_items`'s
deferred-pair branch all collapse to "render a `DeferredReplace` as
`+/-`, render a plain `DeferredCreate` as `+`". The pairing
heuristic (resource_type match + binding base-name match against
template) is deleted. The display layer reads the planner's
classification rather than re-deriving it.

If the planner correctly classifies, the display gets it right; if
the planner mis-classifies (false positive — pairing a `Delete` that
is genuinely unrelated to the template), the user sees a wrong
display *and* the writeback skips a cleanup it should not skip. So
the planner's classification rule must be conservative and tested
end-to-end against the same fixtures that drive the existing pairing.

### Planner classification rule

In `expand_same_config_deferred_for` (or a successor pass that owns
the planner's Effect emission), after the orphan-delete loop and the
apply-time reexpansion-target loop have both populated their
working sets, walk the apply-time targets and for each target T:

1. Find every orphan `Delete` whose `id.resource_type` equals
   T.template's resource type *and* whose `id.name`'s binding base
   matches T.template's binding name with a single bracketed index
   suffix (the exact predicate `binding_matches_deferred_template`
   uses today in `plan_tree.rs`).
2. If at least one such delete exists, remove it from the orphan-
   delete set and absorb it into a `DeferredReplace` along with
   the apply-time target.
3. Otherwise emit the apply-time target as a plain
   `DeferredCreate` (today's pure-add case).

The matching predicate is moved from `plan_tree.rs` to the planner
unchanged. Both consumers can share the same `pub(crate)` helper in
`carina-core::differ` so a future change has one site to touch.

## Alternatives considered

<!-- derived-from #decision -->

### A. Suppress the cleanup inside `WritebackPlan::add_upsert`

Make `add_upsert` overwrite an existing cleanup (new state wins).
Minimum diff (~10 lines), but it removes the collision detector's
ability to flag a real `moved`-block misuse: a hand-written
`moved { from = X to = Y }` where `X` is still in desired would
silently succeed instead of erroring with the current message. The
collision detector's correctness on the two cases where it matters
today (`Effect::Delete`, `Effect::Move`) would be lost in exchange
for fixing the one case where it mis-fires.

Rejected: trades a correctness invariant for a smaller diff.

### B. Branch inside `WritebackPlan::add_upsert` on a known marker

Tag `runtime_synthesized_resources` upserts as "expected to absorb a
prior cleanup" and have `add_upsert` consult the marker before
treating the cleanup as a collision. The marker is convention rather
than type — adding a fifth consumer (e.g. a future
`Effect::ImportThenCreate`) would need to remember to set the same
marker or re-introduce the collision.

Also leaves three other consumers (`plan_tree.rs` × 2, future TUI)
re-deriving the deferred-replace pairing from raw Effects on every
walk.

Rejected: convention seam where the type can carry the relationship.

### C. Effect-model fusion — chosen

Captured above. One typed variant the planner sets and every
consumer matches exhaustively. The "deferred replace is one
operation" invariant is encoded in the variant, not in three sibling
pairing passes spread across plan, display, and writeback.

## Implementation outline

<!-- derived-from #decision -->

1. **Rename `Effect::ExpandDeferredFor` → `Effect::DeferredCreate`**
   in `carina-core/src/effect.rs`, and rename the surrounding
   `Expand` / `ApplyTimeReexpansion` vocabulary to the
   `DeferredCreate` family so the variant and its supporting symbols
   read consistently:

   | Before                              | After                              | Location                                  |
   | ----------------------------------- | ---------------------------------- | ----------------------------------------- |
   | `Effect::ExpandDeferredFor`         | `Effect::DeferredCreate`           | `carina-core/src/effect.rs`               |
   | `expand_deferred_for_effects`       | `materialize_deferred_create`      | `carina-core/src/executor/expand.rs`      |
   | module `executor/expand.rs`         | `executor/deferred_create.rs`      | `carina-core/src/executor/`               |
   | `ExpansionFailure`                  | `DeferredCreateFailure`            | same module                               |
   | `ApplyTimeReexpansionTarget`        | `DeferredCreateTarget`             | `carina-cli/src/wiring/mod.rs`            |
   | `add_apply_time_reexpansion_effects`| `add_deferred_create_effects`      | `carina-cli/src/wiring/mod.rs`            |

   `materialize_deferred_create` mirrors the existing
   `materialize_moved_states` ([`state_writeback.rs`'s pre-writeback
   helper for `Effect::Move`'s `from`→`to` state transfer]) so the
   "materialize the state-side consequence of a non-provider effect"
   shape stays consistent across the codebase. Sweep every consumer
   that exhaustively matches on `Effect` (planner, executor parallel /
   phased schedulers, writeback decomposer, display, plan tree, TUI,
   IAM preflight) — about 30 match sites identified by
   `grep -rn "ExpandDeferredFor" --include="*.rs"`. No semantic
   change in this step; it lands as the first commit of the
   implementation PR so the rest of the diff reads cleanly against
   the new names.
2. **Add `Effect::DeferredReplace` + `DeferredReplaceDelete` to
   `carina-core/src/effect.rs`** with serde derives matching the
   sibling variants. Wire it into `Effect::is_mutating`,
   `Effect::as_resource_ref` (returns `None` — there is no single
   target resource; the deletes' ids are exposed via a dedicated
   accessor), `Effect::binding_name`, `Effect::blocking_bindings`,
   `Effect::display_glyph`, and any other `Effect`-method that
   exhaustively matches today.
3. **Update the planner**: replace the current "emit `Delete` for
   orphan + emit `DeferredCreate` for re-expansion target" sequence
   with a fused pass that absorbs matching orphans into the new
   variant. The matching predicate moves from `plan_tree.rs` to the
   planner.
4. **Update the executor**: schedule the deletes half exactly like
   the current `Effect::Delete` dispatch (sharing
   `execute_basic_effect`'s delete arm via a small adapter), then
   run the create half exactly like the current `DeferredCreate`
   dispatch. Reuse `BasicEffect::Delete` for the delete half so the
   basic executor's typestate stays the only delete path.
5. **Update writeback** as sketched above — Phase 2 gets a
   `DeferredReplace` arm that emits cleanups only for deletes whose
   id is not also an upsert. The `add_upsert` /
   `add_cleanup` collision detectors are unchanged.
6. **Collapse the display pairing**: delete
   `paired_deferred_for_siblings`,
   `collect_paired_deferred_summary_for_siblings`,
   `binding_matches_deferred_template` (move to planner), and the
   `paired_delete_indices` machinery; render `DeferredReplace`
   directly as `+/-` with `delete_indices` derived from the variant.
7. **Update TUI** in `carina-tui/src/app/mod.rs` (effect match site)
   to handle the new variant — should follow the same display
   classification as the CLI tree.
8. **Saved-plan compatibility**: `Plan` serdes Effect into JSON
   today (saved-plan file format). The new variant gets the same
   default-handling treatment as `Effect::Wait` /
   `Effect::DeferredCreate`. No special compatibility shim — saved
   plans across the boundary are not load-bearing per
   `feedback_no_backward_compat`. The variant rename also changes
   the serde tag from `"ExpandDeferredFor"` to `"DeferredCreate"`;
   any pre-rename saved plan in the wild fails to deserialize, which
   matches the project policy.

## Tests

<!-- derived-from #implementation-outline -->

1. **Planner classification unit tests** in `carina-core::differ` or
   `carina-cli::wiring::tests`: given a desired set that drops a
   deferred-for body and a pre-apply state with `validation_records[0]`,
   the resulting plan must contain exactly one `DeferredReplace`
   carrying the absorbed delete *and* the apply-time
   `DeferredCreate` target — no separate `Delete` or
   `DeferredCreate` for the same template.
2. **Planner negative test**: an orphan `Delete` whose binding does
   not match any deferred-for template is emitted as a plain
   `Effect::Delete` (no false absorption).
3. **Writeback collision regression** in
   `carina-cli::commands::shared::state_writeback` tests: a
   `DeferredReplace` whose delete half succeeds *and* whose create
   half produced a successful Create at the same id must produce a
   writeback plan with one upsert and zero cleanups for that id.
   The same shape under `Effect::Delete + DeferredCreate` (i.e. the
   pre-fix Effect layout, constructed by hand in the test) is *not*
   a regression target — the planner is the only producer and the
   planner will never emit that layout once the change lands.
4. **Writeback partial-failure**: a `DeferredReplace` whose delete
   half succeeded but whose create half failed (no upsert) must emit
   one cleanup for the deleted id. State after writeback drops the
   pre-apply iteration; the operator re-runs `carina apply` to
   re-materialise the create half.
5. **End-to-end repro coverage**: a `cargo nextest` test under
   `carina-cli/tests/` that drives `run_apply_from_plan_locked`
   (or the smallest equivalent integration surface) over a fixture
   reproducing carina#3599's shape and asserts the post-apply state
   file persists *both* the successful sibling resources (the LB,
   the new cert, the alias record) *and* the new validation
   record's identifier. The pre-fix code would abort writeback on
   the test fixture; the post-fix code must not.
6. **Saved-plan round-trip**: serialize a `DeferredReplace` to JSON
   and parse it back; the resulting `Effect` is `Eq` to the original.
7. **Display snapshot**: an insta snapshot at the existing plan-
   display test set in `carina-cli/src/display/tests.rs` covers the
   `+/-` rendering before and after; the snapshot output is
   unchanged because the display still resolves the variant to the
   same `+/-` row.
8. **Collision detector still fires on real `moved` misuse**: a unit
   test that constructs `Effect::Move { from: X, to: Y }` against a
   desired set still containing `X` continues to produce
   `UpsertCleanupOverlap`. The fix must not weaken that path.

## Out of scope

- The wait/binding race that caused the sibling Listener failure in
  the same carina#3599 apply (separate sibling issue).
- The orphan AWS-side resources from the regression run (infra-side
  cleanup).
- Generalisation to non-deferred-for "logical replace at same id"
  shapes. The planner only fuses `Delete + DeferredCreate` pairs
  whose binding-base matches the template. If a future feature
  needs the same fusion for a different reason, it adds a sibling
  variant or extends `DeferredReplace`; it does not silently
  inherit the writeback suppression rule.
- `Effect::DeferredUpdate` and `Effect::DeferredDelete`. The
  `Deferred<X>` naming convention reserves the slots — see the
  Decision section — but the actual variants are not needed today.
  `DeferredUpdate` would only be justified if a future feature
  asked for "deferred-for iteration in-place updated against a new
  upstream value", which has no caller today. `DeferredDelete` is
  unnecessary: a deferred-for whose body leaves the desired config
  (or whose iterable resolves to empty) is already covered by the
  existing orphan-delete path emitting plain `Effect::Delete`s.
