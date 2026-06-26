# Deferred-replace group: planner split + typed group identity

<!-- constrained-by ./2026-06-25-deferred-for-replace-effect-design.md -->

## Status

Design proposal for carina#3602.

This document does not implement the change. It records the Effect-
model reshape needed so that

1. the scheduler treats absorbed deletes as plain effects (closing the
   carina#3602 deadlock at the root, not by adding per-scheduler
   bookkeeping), and
2. writeback / display / TUI / future consumers continue to recognise
   "this delete and this create belong to the same deferred replace"
   without re-deriving the pairing — the property carina#3599
   established and that this design must preserve.

Implementation lands in a follow-up PR after this design merges.

## Problem statement

<!-- derived-from #status -->

Two related problems land at the same time:

**(a) Scheduler deadlock (carina#3602).** Post-carina#3599, the
parallel scheduler dispatched `Effect::DeferredReplace` by awaiting
`dispatch_deferred_replace` *inline* inside the ready-walk loop.
Provider delete calls inside that await held the loop, so no other
ready effect — including sibling Creates with zero dependencies — was
scheduled until the deletes returned. In one reproducer, an apply
silently stalled for >7 minutes after a successful `Create cert`
because the deferred replace of `validation_records[*]` consumed the
scheduler indefinitely. A repro on `envs/registry/dev/publish` showed
all 16 tokio worker threads parked in `parking_lot::condvar::wait_until_internal`,
zero `carina::` frames on any CPU, and the apply unable to progress
without `SIGKILL`. CPU was 0%.

**(b) Three-site runtime bookkeeping (review of the first fix).** A
direct fix landed on a worktree (commit `c8069ab6`) that moves the
absorbed deletes onto the existing `in_flight: FuturesUnordered`
path, with a `DeferredReplaceParent { pending_deletes, delete_failed,
upstream_binding, template }` tracker and a
`deferred_replace_child_to_parent: HashMap<usize, usize>` plumbed
across three scheduler sites: `executor/parallel.rs`,
`executor/phased.rs` phase-1 (~line 484), and `executor/phased.rs`
phase-4 (~line 1661). The same ~70-line per-completion handler ("decrement
pending_deletes, on zero materialize the create half") is repeated at
each site. Code review classed this as exactly the convention seam
the CLAUDE.md root-cause rule forbids: a new scheduler — or a future
sibling variant such as `Effect::DeferredUpdate` — would need to
remember the same tracker. The pattern is "broken state representable
in the type system", just at the scheduler layer rather than at the
Effect layer.

**The two problems share one root.** `Effect::DeferredReplace` is
shaped as a single fused variant carrying its delete and create
halves together. That shape was the right answer to carina#3599's
*writeback collision* problem — the consumers (writeback, display,
TUI) saw one variant instead of an inferred pair. But the same shape
is the wrong answer to the *scheduler dispatch* problem — the
scheduler wants the deletes to be visible as scheduling units it can
parallelise, gate, fail, and complete independently. Today the
scheduler synthesises that view at runtime via the bookkeeping
above, and a future variant must re-synthesise it. The Effect model
needs to express both views at once.

## Decision

<!-- derived-from #problem-statement -->

Split the fused `Effect::DeferredReplace` variant at the planner
level into `Effect::Delete` instances + a single `Effect::DeferredCreate`,
linked by a typed group identity, so:

- The scheduler sees N+1 plain effects it already knows how to
  dispatch via the existing `in_flight: FuturesUnordered` path. No
  per-variant special case. No `DeferredReplaceParent`. No
  `child_to_parent` HashMap. The deletes share the existing
  parallelism cap, the existing failure-skip semantics, the existing
  per-effect observer events. A new scheduler — phased phase-1,
  phased phase-4, a future phase-N, a hypothetical sequential or
  retry-on-failure scheduler — inherits the dispatch for free.
- writeback / display / TUI / IAM preflight see the relationship
  via a `DeferredReplaceGroupId` newtype carried on every effect
  participating in the replace. The pairing predicate consumers
  used to call (`paired_deferred_for_siblings` in `plan_tree.rs`
  pre-carina#3599, or the implicit "this delete and this DeferredCreate
  share a template binding" check) is replaced by a typed equality
  on the group id. No consumer re-derives the relationship.

### `DeferredReplaceGroupId`

```rust
/// Opaque identity for the set of effects that together represent a
/// single deferred-for replacement: N `Effect::Delete` halves (one
/// per pre-apply iteration that the planner classified as a
/// "replace") plus exactly one `Effect::DeferredCreate` that will
/// re-materialise the iteration set against the post-apply upstream.
///
/// Two effects with the same `DeferredReplaceGroupId` are guaranteed
/// to belong to the same logical replace operation, regardless of
/// whether they are the delete halves or the create half.
///
/// The id is produced by the planner exactly once per replace and
/// is opaque to every consumer: writeback compares ids for equality,
/// display groups effects by id, the dependency graph wires the
/// DeferredCreate's dependency on every Delete by id lookup. No
/// caller may construct an id; only the planner may, via a
/// constructor that takes the source template binding name and an
/// `add_state_block_effects`-pass salt to disambiguate two
/// replaces of the same template name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeferredReplaceGroupId(/* opaque inner: u64 hash or
                                     similar; the field is
                                     `pub(crate)` so external code
                                     cannot mint one */);
```

### Effect shape after the split

`Effect::DeferredReplace` is **removed**. The two surviving variants
gain an optional `deferred_replace_group` field:

```rust
pub enum Effect {
    // ...
    Delete {
        id: ResourceId,
        identifier: String,
        directives: Directives,
        binding: Option<String>,
        dependencies: HashSet<String>,
        explicit_dependencies: HashSet<String>,
        /// Set when this Delete is one half of a deferred-for
        /// replace whose create half is a sibling `Effect::DeferredCreate`
        /// carrying the same id. Plain orphan deletes (the user
        /// removed a resource that has no deferred-for replacement)
        /// leave this `None`.
        #[serde(default)]
        deferred_replace_group: Option<DeferredReplaceGroupId>,
    },
    DeferredCreate {
        id: ResourceId,
        upstream_binding: String,
        template: Box<DeferredForExpression>,
        /// Set when this DeferredCreate is the create half of a
        /// deferred-for replace whose delete halves are sibling
        /// `Effect::Delete` effects carrying the same id. Pure-add
        /// DeferredCreates (no pre-apply iteration existed) leave
        /// this `None`.
        #[serde(default)]
        deferred_replace_group: Option<DeferredReplaceGroupId>,
    },
    // ...
}
```

`DeferredReplaceDelete`, `NonEmptyDeletes`, the variant-count
assertion bump, the writeback `DeferredReplace` arm, the planner
absorption pass — all gone. The planner instead:

1. Walks the orphan-delete set and the apply-time DeferredCreate
   target set together (the same data the absorption pass walks
   today).
2. For each `DeferredCreate` target T, finds matching orphan
   deletes via `binding_matches_deferred_template`.
3. If matches exist: mints one `DeferredReplaceGroupId` (e.g.
   `DeferredReplaceGroupId::from_template(&template.binding_name,
   plan.next_replace_salt())` — salt makes two replaces of the same
   template name distinguishable, important for module-instances).
   Stamps the same id on the `Effect::DeferredCreate` it emits and
   on each matched `Effect::Delete` left in the orphan set (they
   are not removed; they stay in the plan as plain Deletes with the
   group id set).
4. If no matches: emit the `DeferredCreate` with `deferred_replace_group:
   None` (pure-add case, unchanged).

### Scheduler

Zero changes. Deletes with `deferred_replace_group: Some(_)`
dispatch through the existing `Effect::Delete` path. The
`Effect::DeferredCreate` with `deferred_replace_group: Some(_)`
inherits its scheduling order from the existing dependency-edge
mechanism: the planner records each matched Delete's binding in the
DeferredCreate's `dependencies` set so the scheduler already gates
DeferredCreate dispatch on all sibling deletes' completion. No new
dependency primitive.

The `DeferredReplaceParent` struct, the `deferred_replace_child_to_parent`
map, and the per-completion handler that decrements pending_deletes
disappear from `parallel.rs`, `phased.rs` phase-1, and `phased.rs`
phase-4. The three sites collapse to the standard dispatch case for
plain deletes.

### Writeback

The carina#3599 collision detector's invariant — "an upsert and a
cleanup against the same id is structural evidence that a `moved`-
block `from` collided with a desired-side resource" — held when the
planner emitted one `Effect::DeferredReplace`. Under the split, it
breaks the same way the pre-carina#3599 code broke: the planner
emits an `Effect::Delete` for the old iteration AND the runtime-
synthesised create of the same id produces a `runtime_synthesized_resources`
entry, and writeback's Phase 1 + Phase 2 walk would see one upsert
and one cleanup against the same id.

The group id closes that gap typedly. `Effect::writeback_cleanup_ids`
(introduced as the typed exhaustive-match replacement for the
`_ => {}` arm in carina#3601's hardening) gains a group-aware arm
for `Effect::Delete`:

```rust
Effect::Delete {
    id, deferred_replace_group: Some(g), ..
} => {
    if successfully_deleted.contains(id) && !upserts(id) {
        // The matched `DeferredCreate { deferred_replace_group: Some(g), .. }`
        // will produce a fresh upsert under the same id if it
        // succeeded. Skip the cleanup in that case; emit only if
        // no sibling upsert exists.
        vec![id.clone()]
    } else if successfully_deleted.contains(id) {
        // Sibling create succeeded — its upsert wins, no cleanup needed.
        vec![]
    } else {
        vec![]
    }
}
Effect::Delete {
    id, deferred_replace_group: None, ..
} => {
    // Plain orphan: emit cleanup as today.
    if successfully_deleted.contains(id) { vec![id.clone()] } else { vec![] }
}
```

The `!upserts(id)` check is the same suppression that carina#3601's
`Effect::DeferredReplace` arm carried, now keyed on the Delete itself
rather than on the fused parent. A new variant that wants to
participate in a deferred-replace-style collision-free relationship
adds a `deferred_replace_group: Option<DeferredReplaceGroupId>` field
and the exhaustive-match compiler error forces it to answer the
suppression question.

### Display / TUI

The display layer reads the group id directly. Two effects with the
same group id render as one `+/-` row (the delete halves struck
through, the deferred-create row showing "N records after upstream
applies"). Effects with `deferred_replace_group: None` render
independently as today. The `plan_tree.rs` pairing-predicate site
that carina#3599 deleted does NOT come back — the planner has
already paired the effects via the typed id, and display reads the
classification.

The plan-summary tally (`Plan: N to add, N to change, N to replace,
N to destroy`) counts each `Effect::Delete` with
`deferred_replace_group: Some` AS a delete (it really is a delete:
the old state row goes away) AND counts each `Effect::DeferredCreate`
with `deferred_replace_group: Some` as a replace (the carina#3601
hardening's "one replace per DeferredReplace" tally moves to "one
replace per DeferredCreate-with-group" tally). The cardinality user
sees in the summary line is unchanged from carina#3601.

### Dependency graph

`build_dependency_graph` already wires Effect dependencies via the
`dependencies: HashSet<String>` set on each effect. The planner
populates each `Effect::DeferredCreate { deferred_replace_group:
Some(_), .. }`'s `dependencies` with every matched Delete's
`binding`, so the scheduler gates the DeferredCreate behind all of
them naturally. No special edge type. No new graph primitive.

(For DeferredCreate's existing `dependencies` field, this is one
new source of edges; the field continues to also carry the upstream
binding edge. Both flow through `blocking_bindings()` /
`explicit_dependencies()` as today.)

### IAM preflight, debouncer, schema-aware diff

Each existing exhaustive `match effect { ... }` site that previously
included an `Effect::DeferredReplace { ... }` arm collapses back to
the surviving variants (`Effect::Delete` / `Effect::DeferredCreate`)
they already match — those sites *predate* carina#3599 and worked on
the split shape originally. The reshape is closer to the pre-
carina#3599 model than to the post-fix model, modulo the group id.

## Alternatives considered

<!-- derived-from #decision -->

### A. Keep `Effect::DeferredReplace` and centralise the tracker in `deferred_dispatch.rs`

The minimal-diff option from the code review (Option Z in the
post-review discussion). It hides the three-site duplication behind
a `DeferredReplaceTracker` helper consumed at each scheduler site,
but the scheduler still has a per-variant special case. A future
sibling variant (`DeferredUpdate`, batched-create) re-discovers the
need for the same tracker shape. The "broken state representable in
the type system" lens (CLAUDE.md) does not pass: the scheduler must
remember to call the helper.

Rejected because the Effect model — not the scheduler — is the right
place to express "this is N+1 effects with one synthetic edge."

### B. Synthetic dependency edge with no group id

Lift the planner split (Decision), but DO NOT add a group id. The
scheduler dispatch becomes correct (no special case) but writeback
loses the carina#3599 invariant: a plain `Effect::Delete` and a
plain `Effect::DeferredCreate` produce upsert+cleanup against the
same id and `WritebackPlan::add_cleanup` raises
`UpsertCleanupOverlap`. Consumers (writeback, display, TUI) would
need to re-derive the "these two belong to the same replace" pairing
from binding names — the exact regression carina#3599 closed.

Rejected because it trades the carina#3602 deadlock for the
carina#3599 collision class, with no net root-cause progress.

### C. Group id chosen — chosen.

Captured in Decision above. Closes both classes:

- Scheduler: no special case (carina#3602 root cause goes to zero).
- Writeback / display / TUI: the relationship lives in the type
  (`Option<DeferredReplaceGroupId>` on the two variants), not in a
  consumer-side pairing predicate (carina#3599 root cause preserved).

Three lenses (long-term, type-safety, root-cause) agree.

## Implementation outline

<!-- derived-from #decision -->

1. **Add `DeferredReplaceGroupId`** to `carina-core/src/effect.rs`
   with serde + derives. Inner field `pub(crate)`. Constructor on
   `DeferredReplaceGroupId` taking `&str` template binding + a u64
   salt from the planner. `Display`/`Debug` show
   `"deferred-replace:<template>:<salt>"` for diagnostic readability.

2. **Add `deferred_replace_group: Option<DeferredReplaceGroupId>`**
   to `Effect::Delete` and `Effect::DeferredCreate` with
   `#[serde(default)]`. Saved-plan compatibility: pre-rename saved
   plans deserialise with `None`, behaving as plain Delete / pure-add
   DeferredCreate per the existing pre-carina#3599 path.

3. **Remove `Effect::DeferredReplace`** variant. Remove
   `DeferredReplaceDelete` and `NonEmptyDeletes`. The variant-count
   assertion in `effect.rs::every_effect_variant` drops one.

4. **Planner**: replace the absorption pass in `add_deferred_create_effects`
   (`carina-cli/src/wiring/mod.rs`) with the new flow:
   - For each `DeferredCreateTarget` T, find matching orphan
     `Effect::Delete`s by template-binding match.
   - If matches: mint a group id, stamp it on each matched Delete
     (in place — do NOT remove them from the plan), stamp the same
     id on the emitted DeferredCreate, and add each matched Delete's
     `binding` to the DeferredCreate's `dependencies` set.
   - If no matches: emit the DeferredCreate with
     `deferred_replace_group: None` as today.

5. **Scheduler**: remove `DeferredReplaceParent`,
   `deferred_replace_parents`, `deferred_replace_child_to_parent`,
   the special DeferredReplace dispatch arm, and the per-completion
   parent-trigger logic from `executor/parallel.rs`,
   `executor/phased.rs` phase-1, and `executor/phased.rs` phase-4.
   The dispatch loop now treats every Delete identically.

6. **Writeback**: extend `Effect::writeback_cleanup_ids` so the
   `Effect::Delete` arm branches on `deferred_replace_group` and
   does the suppression-vs-cleanup decision typedly. Remove the
   former `Effect::DeferredReplace` arm.

7. **Display / TUI**: read the group id directly. The plan tree
   renders all effects with the same group id as one `+/-` group;
   the existing display-pairing tests update to construct effects
   with the same group id rather than the fused variant.

8. **`dispatch_deferred_replace` is already gone** (the c8069ab6
   commit removed it). The split makes the removal permanent —
   the function and its sibling tracker logic do not need to come
   back in any form.

## Tests

<!-- derived-from #implementation-outline -->

1. **Group-id planner classification**: a deferred-for whose pre-
   apply state has matching iterations produces an `Effect::DeferredCreate`
   with `deferred_replace_group: Some(g)` AND a sibling
   `Effect::Delete` with the same `g`. Their bindings overlap (the
   DeferredCreate's `dependencies` contains the Delete's binding).
2. **Pure-add planner classification**: a deferred-for with no
   matching pre-apply iterations produces an `Effect::DeferredCreate`
   with `deferred_replace_group: None` and no companion Delete.
3. **Plain orphan delete**: an unrelated `Effect::Delete` (not
   matching any template) has `deferred_replace_group: None` and is
   not paired with any DeferredCreate.
4. **Writeback group suppression**: a paired (Delete, DeferredCreate)
   group whose create half produced a successful Create at the same
   id produces a writeback plan with one upsert and zero cleanups
   for that id. Without the group, the same shape produces
   `UpsertCleanupOverlap` (regression guard for carina#3599).
5. **Writeback partial-failure**: a paired group whose Delete
   succeeded but whose DeferredCreate failed produces one cleanup
   for the deleted id (matches carina#3599's partial-failure test).
6. **Scheduler concurrency**: a plan with a paired group + sibling
   no-dep Create dispatches both halves of the group AND the sibling
   concurrently. Exercises the carina#3602 deadlock regression
   guard (the inline-await pattern is now structurally
   impossible because the scheduler has no special-case for the
   group).
7. **Real `moved`-block collision still fatal**: a `Move { from, to }`
   whose `from` is still in the desired set continues to produce
   `UpsertCleanupOverlap`. The writeback collision detector for
   non-Delete variants is unchanged.
8. **Saved-plan round-trip**: a paired group serdes through JSON
   and back without losing the group id.
9. **End-to-end repro of carina#3602 (deadlock)**: a fixture with
   the carina-rs/infra `publish` shape — a cert Create, a paired
   DeferredCreate+Delete on `validation_records`, a sibling no-dep
   alb Create — drives `run_apply_from_plan_locked` (or the
   smallest equivalent) and asserts apply completes without hang
   and the post-apply state file persists every successful resource.
   Pre-fix (c8069ab6's executor-side tracker) this test would
   require harness changes to expose the deadlock; post-fix the
   test is the natural shape.

## Out of scope

- The cleanup-after-SIGINT deadlock observed during the
  carina#3602 repro (carina#3542's neighbour; cleanup is itself a
  victim of the scheduler deadlock and likely resolves once the
  scheduler does, but the SIGINT-handler/lock-release story is
  separate).
- Heartbeat scheduler instrumentation (the original carina#3602
  body's "What would help" section). The reshape removes the
  failure mode the heartbeat was meant to diagnose; the
  instrumentation is still useful in general but is a separate
  feature.
- Generalising the group id to other "N effects + 1 synthetic
  effect" relationships beyond deferred-for replace. A future
  `Effect::DeferredUpdate` or batched-create might want the same
  pattern; if so, the group id is the obvious primitive to extend
  or specialise. Today only deferred-for replace needs it.
