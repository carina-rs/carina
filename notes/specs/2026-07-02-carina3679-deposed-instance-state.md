# Issue #3679 Deposed Instance State Representation — Design

Issue: https://github.com/carina-rs/carina/issues/3679

## Goal

When a create-before-destroy (CBD) replacement partially fails — the new
instance's Create succeeds but the old instance's Delete is skipped or
fails — the still-live old instance must remain tracked in state until a
Delete for it actually succeeds. Today it silently vanishes from state
and becomes an untracked orphan that `destroy` can never clean up
(observed on real AWS 2026-07-02: old VPC `vpc-0b8150caa42035bcd` had to
be found and deleted manually).

## Root cause

State is one row per `(provider, resource_type, identity)`
(`StateFile.resources: Vec<ResourceState>`; logical key enforced by
`find_resource` / `upsert_resource` / `remove_resource`). During a CBD
replacement two live remote instances legitimately share one
`ResourceId`: the new one just created and the old one awaiting delete.
The single-slot model cannot represent that, so writeback
(`decompose()` in `state_writeback.rs`) lets `applied_states` (the new
Create's result) overwrite the row, and the old instance's identifier
has nowhere to live. Every consumer path (apply writeback, destroy,
refresh, plan) inherits the same gap — it is one bug in the data model,
not N executor bugs.

## Reality the model must represent

- Between a CBD Create success and the old Delete success, **two live
  instances of one ResourceId coexist**. This window is not exceptional;
  it exists on every CBD replacement and becomes persistent whenever the
  Delete fails (dependency violation, permission error, transient API
  failure) or is skipped by dependency-failure propagation.
- Repeated replacement failures can stack **multiple generations** of
  old instances (replace fails, config changes, replace fails again).
  Refusing to represent more than one generation would recreate the same
  orphaning bug one level up, so the representation must hold N.
- A row whose current instance was destroyed can still have pending
  deposed instances (destroy deleted the current instance but a deposed
  delete failed). The model must represent "no current instance, deposed
  pending".

## Design decision (three-lens selection)

Adopt the Terraform "deposed instance" concept, represented as a nested
list on the row: `ResourceState.deposed: Vec<DeposedInstance>`, where
`DeposedInstance` is a distinct type.

Alternatives rejected:

- **Row-level flag** (`deposed: bool` on `ResourceState`, multiple rows
  per identity): breaks the `(provider, type, identity)` uniqueness that
  `find_resource`/`upsert_resource` and every consumer rely on; every
  consumer would need to remember a `!deposed` filter — the
  convention-only seam this project forbids.
- **Separate top-level array** (`StateFile.deposed_resources`):
  preserves row uniqueness but associates deposed instances with their
  row by key re-matching at every consumer — a join every consumer must
  remember, i.e. the same convention leak in a different shape.

The nested-vec form wins on all three lenses: root cause (the row keeps
its 1-per-identity invariant while gaining a place for the coexisting
old instances), type safety (a `DeposedInstance` is not a
`ResourceState`; it cannot flow into current-instance positions, and
consumers that only care about the current instance are unaffected by
construction), and long-term (multiple generations are a `Vec` push;
the model matches Terraform's field-proven shape).

## State schema v9

`StateFile::CURRENT_VERSION` bumps 8 → 9. Changelog entry: "v9: added
`ResourceState.deposed` (pre-replacement instances pending delete,
carrying provider-instance routing); rows with `identifier: None` are
retained while `deposed` is non-empty."

```rust
pub struct DeposedInstance {
    /// System-assigned unique key for this generation (short UUID).
    /// Identifies the entry in plan display, logs, and delete results.
    pub key: DeposedKey,
    /// Provider-side identifier of the still-live old instance.
    /// Required by construction: an instance is only deposed when it
    /// exists remotely under a known identifier.
    pub identifier: String,
    /// Provider-instance routing the generation was created under,
    /// sourced from the delete effect's ResourceId at depose time.
    /// A replacement may change the row's routing; the deposed delete
    /// must route to where the old instance actually lives.
    pub provider_instance: Option<String>,
    /// Last known attributes of the old instance (display and refresh
    /// need them; provider delete calls take identifier + directives
    /// only, so dropping an unserializable attribute is safe).
    pub attributes: HashMap<String, serde_json::Value>,
    /// Dependency bindings inherited from the old row, so deposed
    /// deletes keep correct ordering relative to other deletes.
    pub dependency_bindings: BTreeSet<String>,
}
```

`DeposedKey` is a newtype over the generated key string — not a bare
`String` — so it cannot be confused with identifiers or identities in
keyed positions.

Notes:

- `ResourceState.deposed: Vec<DeposedInstance>` with
  `#[serde(default, skip_serializing_if = "Vec::is_empty")]`. v≤8 files
  migrate implicitly (field defaults to empty); no step migration
  needed.
- `identifier` on `DeposedInstance` is `String`, not `Option<String>`:
  deposing an instance without an identifier is meaningless, so the
  type forbids it.
- No `directives` / `protected` on the deposed entry: the replacement
  that deposed it already implied approval to delete the old instance;
  `prevent_destroy` gating happened at plan time for the replacement.
- The carina#3266 managed-only read invariant ("drop rows with
  `identifier: None`") changes at its single seam (the `retain` in
  `check_and_migrate`) to: drop rows with `identifier: None` **and**
  empty `deposed`. That is the "current destroyed, deposed pending"
  state above.
- Removing a resource whose row still has deposed generations leaves a
  shell row: identity keys and `directives.provider_instance` are kept
  (so the row round-trips to the same `ResourceId`), everything else is
  reset, `deposed` is retained. This lives inside
  `StateFile::remove_resource` itself so destroy, refresh, and any
  future caller inherit it — no caller can choose an unsafe removal.
- Deposed preservation across upserts also lives at the row-write seam:
  `StateFile::upsert_resource` merges the existing row's generations
  into the incoming row (incoming wins per `(identifier,
  provider_instance)`), then drops any generation whose identifier
  equals the incoming current identifier (invariant 4, enforced where
  rows are written rather than at each writer). The writeback's own
  depose application additionally relies on the classification's
  same-identifier guard; Phase 2's deposed-delete writeback should
  route every depose write through one filtered helper.
- Riding along on the shared upsert path: building a row from a
  provider state now falls back to the `ResourceId`'s provider-instance
  routing when the directives carry none, so routed rows built from
  synthetic resources (refresh orphans, deferred-replace children)
  round-trip to the same `ResourceId` they were written under. This
  affects every row write, not just deposed ones, and fixes a latent
  routing loss on the refresh-orphan path.

## Instance-level effect keying (the typed reshape)

Once deposed instances exist, "the Delete for identity X" is ambiguous:
it may target the current instance or a specific deposed generation.
Two conflation hazards fall out of today's identity-keyed types:

1. The scheduler indexes effects by `ResourceIdentity`
   (`add_same_identity_replacement_order_edges`, edge lookup maps). A
   deposed Delete plus a current-instance Create/Update/Delete for the
   same identity would collide in those maps and fabricate replacement
   ordering edges.
2. Executor result collection uses
   `successfully_deleted: HashSet<ResourceId>`. If a deposed Delete for
   X succeeds, writeback Phase 2 would see "X was deleted" and remove
   the **whole row**, current instance included — a new data-loss bug
   of exactly the class being fixed.

Fixing these with "consumers check a deposed flag before lookup" would
be the forbidden per-consumer filter. Instead, extend the key type
itself: introduce an instance key that pairs the identity with the
generation being addressed —

```rust
pub struct EffectInstanceKey {
    identity: ResourceIdentity,
    generation: EffectGeneration,   // Current | Deposed(DeposedKey)
}
```

— and move the scheduler's effect index and same-identity edge
derivation onto it. `Effect::Delete` carries the generation it targets
(current deletes say `Current`; deposed deletes say `Deposed(key)`;
the field is serde-defaulted to `Current`). Because the key type
changes, the compiler surfaces every map, set, and comparison that must
now distinguish generations; no consumer can silently keep conflating
them. Create/Update effects always target `Current` by construction,
and `DeferredReplaceDelete` has no generation slot at all — absorbed
deletes are `Current` by type, so a deposed delete can never be
absorbed into a `DeferredReplace` (it stays top-level, where the
remove-deposed writeback can observe its result).

The executor's delete-result set is a separate, richer key —
`DeletedInstanceKey { id: ResourceId, generation, identifier }`. The
scheduler key deliberately stays identity-scoped (matching the
pre-existing scheduler semantics), but delete results must be at least
as strong as the pre-change `HashSet<ResourceId>`: the full
`ResourceId` prevents identity-string collisions across resource
types/providers/routings, and carrying the deleted identifier lets
writeback answer "was this exact remote object deleted?" as a direct
lookup instead of re-deriving it by matching identifier strings
against plan effects.

The same-identity replacement edge derivation keys on
`EffectInstanceKey`, so a deposed Delete never pairs with a current
Create as a pseudo-replacement. Deposed deletes get delete-ordering
edges from their stored `dependency_bindings`; identity-level delete
ordering fans out across generations (a parent's delete waits for
every generation's delete of a dependent identity), and destroy
wait-alias edges include deposed generations in both directions
(a deposed consumer is deleted before its wait target; a deposed wait
target waits for its consumers).

Serialization note: `Effect` flows through saved plans
(`plan --out` → apply). The generation field round-trips through plan
serialization, but it is deliberately NOT treated as cross-version
additive: the plan file version bumps 9 → 10 and the loader rejects
other versions, because an older binary reading a newer plan would
silently degrade a deposed delete to a current delete via the serde
default — and its writeback would then remove the whole row. Saved-plan
pairing-metadata validation also rejects a `Deposed`-generation delete
as a replacement half.

## Writeback: exhaustive replacement outcomes

`decompose()` currently encodes the CBD interaction as a skip-guard
(`is_replacement_delete_index && upserts.contains_key`), and
`DeferredReplace` bakes the same rule separately inside
`writeback_cleanup_ids` — two sibling sites implementing one invariant.
The redesign replaces both with one exhaustive classification per
replacement pair (plain decomposed pairs and DeferredReplace absorbed
deletes flow through the same code path), producing a typed
`WritebackAction` the writeback plan must destructure:

| Create half | Old-instance Delete half | Action |
| --- | --- | --- |
| succeeded | succeeded | upsert new row, no deposed entry (today's behavior) |
| succeeded | failed or skipped | upsert new row **and record a `DeposedInstance`** carrying the old identifier/routing/attributes/dependency bindings |
| failed / not run | not run (CBD delete waits on create) | keep old row from `current_states` (today's behavior) |
| n/a (DBC) delete succeeded, create failed | — | row cleanup (a behavior fix: the pre-change code kept a stale `current_states` upsert here) |

Recording a generation is a keyed replace, not a blind append: an
existing entry with the same `(identifier, provider_instance)` is
refreshed instead of duplicated, so re-running a replacement (e.g. a
stale saved plan) cannot stack duplicate entries for one remote object.

Deposed attribute sourcing, in order: the existing state row when its
identifier matches the delete (already secret-hashed), else the plan
metadata's `previous_attributes`, else the `current_states` snapshot,
else empty. The fallback sources hold raw provider values, so they are
serialized through the same secret-masking seam the ordinary row upsert
uses, with two authorities: the new desired resource's secret-wrapped
attributes and the schema's `write_only` metadata (covers secrets the
new config removed). Serialization is lossy-safe per attribute — an
unserializable value is skipped, never allowed to abort the state save
after infrastructure was already mutated. The masking step is required
by the function signature (the depose builder takes the desired
resource, not an `Option`), so no caller can skip it.

Guard invariant: if the new instance's identifier equals the old one
(provider returned the same remote object), do **not** depose — there is
only one live instance. Deposing it would schedule a delete of the live
current resource.

Deposed-delete results feed back the same way: a successful
`Deposed(key)` delete removes exactly that `DeposedInstance` from the
row (and the row itself only when `identifier` is `None` and `deposed`
is empty); a failed one leaves the entry for the next run.
`WritebackPlan` gains the corresponding typed entries next to upserts
and cleanups, so `build_state_after_apply` handles them by
destructuring, not by convention. Phase split: the depose entries land
with Phase 1 (writeback recording); the remove-deposed entries land
with Phase 2, which introduces deposed-delete scheduling. Within the
writeback plan, upserts and cleanups stay mutually exclusive per
`ResourceId`; a depose intentionally coexists with the same id's upsert.
Application order is upserts → deposes → remove-deposed → cleanups.

Invariant-4 scoping note: "a deposed entry's identifier differs from
the row's current identifier" is enforced per provider instance — a
generation with the same identifier string under a different
`provider_instance` is a different remote object and is retained.

The displaced-current guard (closing the known residual below) deposes
the identifier an applied create displaced from the row, under four
restrictions: only ids on the plan's create side (an update-only
identifier change is the same remote object and never deposes); skipped
when the pre-apply refresh proved the displaced instance gone
(`exists == false` — the manual-deletion recreate flow must not
phantom-depose a confirmed-dead identifier; a failed refresh stays
conservative and deposes); suppressed when the delete-result set shows
that exact row identifier was successfully deleted (routing-aware via
`DeletedInstanceKey`); and deduplicated against an already-planned
depose of the same `(identifier, provider_instance)`.

Two related plan-construction fixes ride along because they are the
same invariant-1 class: `Plan::retain` now remaps the replacement
pairing metadata's effect indices (and drops a pairing whose half was
removed) instead of leaving them stale, and the moved-block delete
suppression only suppresses orphan deletes — a replacement delete for a
desired moved-to resource keeps flowing into classification. Saved
plans validate the pairing metadata at load (index bounds, effect
kinds, create/delete identity match, no duplicate indices) before
anything mutates.

## Command behavior

**plan / apply.** Every plan sources delete-pending work from state
through one shared seam (used by the plan pipeline, the apply pipeline,
and the display fixture harness): each `DeposedInstance` yields an
`Effect::Delete` targeting `Deposed(key)`, with the identifier, routing,
and delete-ordering dependencies taken from the entry, displayed with a
deposed marker, e.g.

```
- awscc.ec2.Vpc main (deposed vpc-0b8150caa42035bcd)
```

The marker appears in CLI plan output, plan-brief lines, and TUI node
labels; detail rows come from the entry's stored attributes, matched by
key + identifier + routing. Deposed deletes count as deletes in the
plan summary. In the plan tree they render as their own root and never
claim the row binding's node — the binding slot belongs to the current
instance's effect, so dependents keep hanging under the current node.
Plan surgery leaves them alone: removed-block suppression only
suppresses current deletes (a removed block speaks about the current
instance; deposed generations are delete-pending work carina itself
created), and deferred-create absorption never absorbs them.

Apply executes them like any delete; success drops the entry, failure
keeps it — the orphan is retried on every subsequent apply until it is
gone, matching Terraform. A failed deposed delete does not enqueue a
state refresh for the row (the old instance's read must never clobber
the current row's state).

**destroy.** Destroy already builds deletes from state rows; it
additionally emits deposed deletes per entry, ordered by the stored
dependency bindings alongside the current-instance deletes. Destroy
result writeback is per instance (`DestroyedInstance { id, generation }`)
and removes exactly the succeeded generation. The plan count and the
emptiness/short-circuit decisions use the actual delete effects, so
deposed-only work still runs and is counted. Backend-protected rows
(the state bucket) shield their deposed generations too; a
`prevent_destroy` row deliberately does not — that directive speaks
about the current instance, and the replacement that deposed the old
one already carried approval to delete it.

**state refresh** (Phase 3 — not yet implemented). Refresh reads each
deposed instance via the provider (synthetic resource from the stored
identifier/attributes, like the orphan path). Gone remotely (e.g. the
user deleted it manually) → drop the entry; still present → update its
stored attributes. This makes manual cleanup converge without any new
subcommand.

**state list / show / lookup** (Phase 3 — not yet implemented). Display
deposed entries under their row with the deposed marker and key. No new
manipulation subcommands (no `state rm` equivalent) — recovery paths
are apply/destroy (delete it) and refresh (observe it already gone),
which cover both directions.

## Invariants after this change

1. A live remote instance recorded in state stays recorded until a
   Delete for that specific instance succeeds (or refresh observes it
   gone). No code path may overwrite or drop an identifier that still
   points at a live instance.
2. One row per `(provider, resource_type, identity)`; coexisting old
   instances live inside the row as `DeposedInstance`s, each with a
   unique `DeposedKey`.
3. Every effect-keyed map/set distinguishes generations —
   `EffectInstanceKey` in the scheduler, `DeletedInstanceKey` (full id +
   generation + identifier) in delete results; identity-only keying of
   deletes is unrepresentable.
4. A deposed entry's `identifier` always differs from the row's current
   `identifier` under the same provider-instance routing (the same
   identifier string under different routing is a different remote
   object and is retained).

## Known residual (closed in Phase 2)

Re-applying a stale saved plan is allowed past a serial-bump warning
(lineage still matches). If run 1 partially failed (current `new-1`,
deposed `old`) and the same plan file is applied again, the re-run
Create returns `new-2` and the applied upsert overwrites the row's
current identifier; the classification only compares against the
plan's delete identifier (`old`), so the displaced `new-1` — live,
never deleted — would be dropped. Phase 2 closes this with the
displaced-current guard described in the writeback section, enabled by
`DeletedInstanceKey` carrying the deleted identifier (the pre-reshape
`successfully_deleted: HashSet<ResourceId>` could not distinguish
which identifier a delete removed). Both re-run variants are pinned by
tests: delete-succeeds (guard deposes `new-1`) and delete-fails-again
(guard deposes `new-1` next to the re-deposed `old`).

One deliberate over-retention remains in the delete-succeeds variant:
the run-1 deposed entry for `old` survives even though the re-run's
Current-generation delete destroyed that object, because only a
`Deposed(key)` delete result removes a deposed entry. Retaining a
dead identifier is the safe direction under invariant 1; it converges
via Phase 3 refresh (observes it gone) or an idempotent provider
delete.

## Non-goals / boundary notes

- No provider/protocol change: deposed deletes reach providers as
  ordinary delete calls (identifier + attributes). Nothing crosses the
  WIT/plugin boundary in a new shape, so carina-provider-aws / awscc
  need no counterpart PR. (Verified: the deposed concept lives entirely
  in carina-state / carina-core effect types / carina-cli.)
- No DSL change, hence no LSP/grammar work.
- No dedicated deposed-manipulation subcommand.

## Testing plan

TDD: the reproducing test comes first, on the real apply pipeline
(unit-green ≠ apply-green — #3677 lesson). Mock-provider harness reuses
the `ApplyCancellationFixture` + `ProviderFactory` pattern from
`apply/tests.rs` (#3680), with a delete-call recorder and failure
injection added.

1. **Repro (fails before the fix):** CBD replacement, Create succeeds,
   old Delete fails → state must contain the new identifier as current
   and the old identifier as a deposed entry. Assert via
   `run_apply_locked` and reading back the written state file.
2. Same via dependency-failure skip (the `⊘` path observed on AWS):
   old Delete skipped because a dependent effect failed.
3. Next apply plans a deposed delete; on success the entry is removed
   and the current row is untouched.
4. Conflation guard: a plan containing only a deposed delete for X must
   not remove X's current row on success (the `successfully_deleted`
   reshape test).
5. Multi-generation: two consecutive partial failures stack two deposed
   entries with distinct keys; each is deletable independently.
6. Destroy with deposed entries deletes both instances; a failed
   deposed delete survives destroy writeback.
7. Refresh drops a deposed entry whose instance is gone remotely and
   preserves one that still exists.
8. DeferredReplace path: same depose-on-failure behavior as the plain
   decomposed pair (shared seam test).
9. State v8 → v9 migration: v8 file loads with empty deposed;
   round-trip serialization of a row with deposed entries.
10. Plan display fixture + snapshot for the deposed marker; state
    list/show snapshots.
11. Final real-AWS check: awscc `cascade_without_cbd` variant with an
    injected step-2 failure, confirming the old VPC is retained in
    state, then cleaned by the follow-up apply.

## Rollout

Design merges first (this document). Implementation follows on branches
off the updated main, ordered so every merge keeps main consistent:
state model + writeback deposing (with repro tests), then
plan/apply/destroy scheduling of deposed deletes + display, then
refresh + state subcommand surfacing. The instance-key reshape lands
with the first consumer that needs it, not as a deferred follow-up.
