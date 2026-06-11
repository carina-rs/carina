# Safe anonymous-rename reconciliation and `moved` precedence

<!-- derived-from ./2026-06-10-anonymous-address-stability.md -->
<!-- constrained-by ../../carina-core/src/identifier/mod.rs -->
<!-- constrained-by ../../carina-cli/src/wiring/mod.rs -->
<!-- constrained-by ../../carina-cli/src/commands/shared/state_writeback.rs -->

## Status

Design PR for carina#3454.

This document records the root causes and the intended fix direction. It
deliberately does not implement the fixes. Implementation follows in
separate PRs (see "Implementation plan" at the end).

## Background

Anonymous resources carry a hash-derived state address suffix. Two hash
schemes exist, chosen in `compute_anonymous_identifiers_with_provider_configs`
(`carina-core/src/identifier/mod.rs`):

- **Standard hash** (8 hex chars, u32): used when at least one create-only
  or schema-identity attribute value is available for hashing. A normal
  hash — one changed input bit re-randomizes the whole output.
- **SimHash** (16 hex chars, u64): used when no create-only/identity values
  exist. A locality-sensitive hash — similar inputs produce similar
  outputs, so Hamming distance between two SimHashes approximates input
  similarity.

When a provider lock bump (or a hash-input change such as the carina#3428
enum canonicalization) shifts these hashes, two mechanisms exist to map
old state addresses to new desired addresses:

- `reconcile_anonymous_identifiers` — a heuristic pass that either re-keys
  a state entry to the new desired name (SimHash branch) or renames the
  desired resource to the existing state name (create-only match branch);
- operator-written `moved { from = ... to = ... }` blocks, materialized by
  `materialize_moved_states` (`carina-cli/src/wiring/mod.rs`).

## Problem

carina#3454 (surfaced by carina-rs/infra#138) reports three symptoms after
an awscc/aws lock bump shifted the hashes of five anonymous resources
(2 `awscc.ec2.Route`, 3 `awscc.ec2.SubnetRouteTableAssociation`):

1. With no `moved.crn`, `plan` scrambled the three sibling
   `SubnetRouteTableAssociation`s — two were presented as
   `forces_replacement` (tear down + recreate real AWS resources) even
   though every desired resource had exactly one state entry with
   identical create-only attribute values.
2. A hand-written `moved.crn` with five correct 1:1 blocks was silently
   partly applied: blocks whose `from` the heuristic pass had already
   consumed were dropped without any warning.
3. An incorrect `moved.crn` (whose `from` values were read off the plan's
   rebound names) produced a clean `5 to move` plan, then failed at apply
   with `writeback planned both an upsert and a cleanup for the same
   resource id`. The collision is detectable at plan time but is only
   checked at the end of apply.

All three symptoms were verified against the issue's captured plan
output; the simulated pass behavior reproduces it exactly.

## Root causes

### RC1 — Hamming matching is applied to non-locality-sensitive hashes

`reconcile_anonymous_identifiers` falls back to Hamming-distance matching
whenever the desired resource's create-only values cannot be stringified:

```rust
if create_only_attrs.is_empty() || resource_co_values.is_empty() {
    // SimHash-based Hamming distance matching ...
```

`canonical_create_only_value_string` returns `None` for every
`Value::Deferred` (`_ => None` arm). Create-only attributes written as
`let`-binding references — `route_table_id = private_rtb.id`, the dominant
real-world shape — are still deferred when reconciliation runs (the pass
runs before reference resolution in all four commands), so
`resource_co_values` is empty and **standard-hash (8-hex) resources enter
the SimHash branch**.

`extract_hash_from_identifier` zero-extends 8-hex suffixes to u64. Both
sides being 8-hex, the upper 32 bits contribute zero distance; the
distance of two independent u32 hashes follows B(32, 0.5) — expected 16,
P(distance < 20) ≈ 0.89 — while `SIMHASH_HAMMING_THRESHOLD = 20` was
calibrated for 64-bit SimHashes (independent u64s: expected 32,
P(< 20) ≈ 0.001). Result: nearly every unrelated sibling pair "matches",
and the winner is decided by coincidental hex proximity. That is the
scrambling in symptom 1.

The codebase already knows this invariant — in exactly one place.
`detect_anonymous_to_named_renames` filters its SimHash fallback
candidates:

```rust
// Only consider state entries written via the SimHash path
// (16-hex suffix). 8-hex entries come from the create-only
// hash scheme and are meaningless to XOR with a 64-bit SimHash.
.filter(|e| e.name.rsplit('_').next().map(str::len) == Some(16))
```

A sibling pass (`reconcile_anonymous_identifiers`) lacks the filter. A
convention enforced per call site by a comment is exactly the class of
bug the type system should make unrepresentable.

### RC2 — heuristic reconciliation runs before `moved` and steals its `from`

The heuristic pass runs in all four commands (`plan.rs:574`,
`apply/mod.rs:829`, `destroy.rs:162`, `state.rs:916`) and rebinds
addresses in both directions — the SimHash branch re-keys `state_file`
entries in place, the create-only branch renames the desired resource to
the state name; neither branch is moved-safe today. `moved` blocks are materialized by
`plan` (inside `create_plan_from_parsed_with_upstream`,
`wiring/mod.rs:1855` / `1934`), `apply` (directly,
`apply/mod.rs:1010`), and the fixture-plan display path; `destroy` and
`state refresh` have no moved handling at all. Where both run, the order is fixed:

1. `reconcile_anonymous_identifiers_with_ctx` re-keys `state_file`
   entries in place;
2. only later, `materialize_moved_states` resolves each `moved` block's
   `from` via `state_file.find_resource(...)` — against the **already
   re-keyed** state — and silently `continue`s when the entry is gone
   (`wiring/mod.rs:2240-2241`).

Consequences, all observed in the issue:

- a `moved` block whose `from` the heuristic consumed is dropped with no
  diagnostic (symptom 2);
- worse, a surviving `moved` block whose `to` collides with a
  heuristic-renamed entry **overwrites that entry in `current_states`**,
  silently dropping a state row (orphaning live infrastructure from
  state).

Operator-explicit intent must take precedence over heuristics, and an
ineffective `moved` block must be visible.

### RC3 — the upsert/cleanup collision is validated only at writeback

`decompose` in `state_writeback.rs` errors when the same `ResourceId` is
both upserted (desired side) and cleaned up (a `Move` effect's `from`).
That check is structural (`WritebackPlan::add_upsert`/`add_cleanup`) and
interwoven with apply-only inputs (`applied_states`, refresh/delete
outcomes), so it cannot run verbatim at plan time — but its triggering
shape, a move `from` that is also a desired resource id, is fully
determined by the desired id set and the move pairs, both of which exist
at plan time. Plan never checks it. An invalid `moved.crn` therefore
produces a green plan and a guaranteed-broken apply ("green plan, broken
apply" footgun; once merged, CI is stuck failing).

A second collision shape is today caught nowhere: two moves resolving to
the same `to` are not an upsert/cleanup overlap (cleanups are an
idempotent set of `from` ids), so the second transfer silently overwrites
the first inside `materialize_moved_states` — a state row is dropped
without any error, even at writeback.

## Design

Both lenses are applied to each fix: restore the invariant at the one
upstream seam, and make the broken state unrepresentable for future
callers.

### RC1 — typed hash kinds; meaning-based matching for standard hashes

**Typed hash suffix.** Replace the type-erasing extraction

```rust
pub(crate) fn extract_hash_from_identifier(identifier: &str) -> Option<u64>
```

with a kind-preserving one built on an **opaque SimHash payload**:

```rust
/// A 64-bit SimHash value. The field is private: a `SimHash` can only be
/// produced by the constructors below, so a u32 standard hash can never
/// be smuggled into Hamming-distance comparison.
pub(crate) struct SimHash(u64);

pub(crate) enum AnonymousHashSuffix {
    /// 8-hex create-only/identity hash. Not locality-sensitive;
    /// Hamming distance between two of these is meaningless.
    Standard(u32),
    /// 16-hex SimHash. Hamming distance approximates input similarity.
    SimHash(SimHash),
}

pub(crate) fn extract_hash_from_identifier(identifier: &str) -> Option<AnonymousHashSuffix>
```

`SimHash` has no public `From<u64>` and no public field; its only
producers are `compute_simhash` and a single shared parsing constructor,
`SimHash::parse_16_hex(&str) -> Option<SimHash>`, used by both the
16-hex branch of `extract_hash_from_identifier` and
`parse_synthetic_instance_prefix` (the module expander's prefix parser,
which already enforces 16-hex). The constructor shape matters: the
expander lives in a different module, and giving it a raw
`from_u64`-style escape hatch would reduce the guarantee back to
convention. Parsing hex is the only way in. The Hamming
distance computation is a method on `SimHash` itself, and
`closest_unique_simhash_match` accepts only `SimHash` values — for the
target as well as for the candidates. A caller holding a
`Standard(u32)` has no way to reach Hamming comparison: there is no
conversion to write, not just no filter to forget. The hand-rolled
16-hex string-length filter in `detect_anonymous_to_named_renames` is
replaced by the same typed seam.

**Reconcile branch condition.** The Hamming fallback in
`reconcile_anonymous_identifiers` engages only when the desired resource's
own computed name carries a `SimHash` suffix and the state candidate does
too. Standard-hash resources never enter it. Routing the branch through
`closest_unique_simhash_match` also unifies tie handling: today the
inline loop keeps the first-found minimum, while the helper refuses ties
as ambiguous. The tie-refusing semantics are the deliberate choice —
committing to one of two equally-distant candidates is exactly the silent
corruption this design removes.

**Meaning-based matching for deferred create-only values.** For
standard-hash resources whose create-only values are deferred, add a
resolution step before comparison instead of giving up:

- Exactly one form is resolved: a single-hop
  `ResourceRef(<binding>.<attr>)`. Find the state entry whose `binding`
  field equals `<binding>`, and read `<attr>` from its `attributes`,
  converted through the same canonical string used for state-side
  create-only values
  (`canonical_create_only_state_json_string`).
  Scope is decided from the binding string itself: a dotted binding
  (`inst.x`) is already instance-qualified by construction, so equality
  suffices; a bare binding is a root binding and must match a dot-free
  state entry name. Referrer-based scoping was rejected because
  argument-passed root refs keep the bare root binding and would be excluded.
- Everything else stays unresolved by design: `Interpolation`,
  `BindingRef`, multi-hop paths (`binding.attr.sub`), function calls.
  Unresolved means no value for that attribute, exactly like an
  unresolvable binding or a missing attribute — never an error.
- If more than one state entry carries the same `binding` value, the ref
  is ambiguous and yields no value.

The resolved values then flow through the existing full/partial match
logic verbatim (unique full match preferred, ambiguity refuses to
match). One property of that logic is worth restating for partially
resolved resources: "full match" means every *resolved* value matches
and none mismatches, so a resource with only one of two refs resolved
can full-match on that one value alone; the existing
used/claimed-exclusion and unique-match requirements are what prevent a
wrong pairing, and a tie across two candidates refuses as today. For the
issue's reproducer every desired RTA resolves both refs to exactly one
state entry with identical `(route_table_id, subnet_id)`, so all three
become pure renames with no `moved.crn` at all.

State already contains everything this needs: `ResourceState.binding`,
`ResourceState.attributes`, and `dependency_bindings` are persisted
(carina-state `state/mod.rs`).

One adjacent gap closes in the same PR:
`canonical_create_only_value_string` returns `None` not only for deferred
values but also for concrete `Int` / `Float` / `Bool` / `Duration` /
`StringList`. Left as is, a resource whose create-only attributes are
e.g. numeric would fall out of value comparison entirely — gated off
Hamming (correctly) but with no meaning-based match either, degrading to
add+destroy churn. PR 1 extends the canonical string to those concrete
variants.

### RC2 — operator-claimed addresses are excluded from heuristics, by signature

The invariant is broader than `moved`: **every operator-written state
block names addresses the heuristics must not touch.** `removed { from }`
resolves through the same post-heuristic `state_file.find_resource(...)`
and silently produces no `Remove` effect when the heuristic re-keyed the
entry; `import { to }` silently no-ops when a heuristic re-key lands a
different entry's state under the `to` name. Claiming only `moved` would
be the per-symptom carve-out this repository forbids, so the claims type
covers all state-block kinds:

```rust
/// Addresses claimed by operator-written state blocks. Heuristic rename
/// passes must not consume a claimed `from` state entry nor synthesize a
/// rename onto a claimed `to` name — operator intent always wins over
/// similarity heuristics.
///
/// `from` claims: `moved.from`, `removed.from`.
/// `to` claims: `moved.to`, `import.to`.
pub struct StateBlockClaims {
    from: HashSet<StateBlockAddress>,
    to: HashSet<StateBlockAddress>,
}

impl StateBlockClaims {
    pub fn claims_from(&self, provider: &str, rt: &str, name: &str) -> bool { ... }
    pub fn claims_to(&self, provider: &str, rt: &str, name: &str) -> bool { ... }
}
```

The type lives in `carina-core` (the heuristic passes consume it), but
its resolving constructor cannot: effectiveness evaluation needs the
state file, and `StateFile` is a `carina-state` type that depends on
`carina-core` — core cannot name it. The constructor is therefore a
wiring-layer function, mirroring how the passes already receive state
through wiring-built callbacks:

```rust
// carina-cli/src/wiring/
pub fn resolve_state_block_claims(
    blocks: &[StateBlock],
    state_file: &Option<StateFile>,
    desired: &[Resource],
    registry: &SchemaRegistry,   // import name_attribute resolution
) -> StateBlockClaims { ... }
```

**Only effective blocks claim.** A claim exists to reserve a state entry
(or a desired name) for the operator's block. A block that cannot do
anything must not pin addresses — otherwise a stale `moved` block whose
`from` no longer exists would still claim its `to`, excluding that
desired resource from RC1's meaning-based rescue and degrading a
would-be no-op plan into add+destroy of live resources. Effectiveness is
decided against the state file as read, before any heuristic pass:

- a `moved` block claims `(from, to)` iff its `from` resolves in state;
- a `removed` block claims `from` iff it resolves in state (absent =
  already removed, silent no-op as today, no claim);
- an `import` block claims its target's desired id iff the target is not
  already in state.

The set element is the existing `StateBlockAddress` (routing-agnostic by
construction, carina#3324) rather than a stringly tuple. Routing note:
the heuristic passes' own candidate space is keyed without
`provider_instance` (`used_names` in `identifier/mod.rs`), so
address-granularity claims match it exactly; if two state entries share
an address triple across provider instances, both are excluded —
conservative over-exclusion (rename churn at worst), never corruption.

**Import targets resolve like the import itself does.** Users commonly
write `import { to = awscc.s3.Bucket 'carina-rs-state' }` against the
resource's `name_attribute`, not the anonymous hash name
(`resolve_import_target`'s documented style). A literal address match
would therefore miss the desired resource the import targets. Claims
construction reuses the same desired-side matching the import path uses —
by name, then by `name_attribute` value against `desired` — and claims
the *resolved desired resource's address* (its id projected onto the
routing-agnostic `StateBlockAddress`, matching the field type). An
unresolvable import target claims nothing.

One import trade-off is accepted deliberately: if the physical resource
already sits in state under an old hash name and the operator wrote
`import` where `moved` was meant, today's buggy heuristic can
accidentally self-heal (re-key the old entry onto the import target,
which then no-ops). Under claims exclusion the plan instead shows an
`import` plus an orphan delete of the same physical resource — wrong
operator input now produces a *visible* wrong plan instead of a silent
accidental rescue. The `moved` block is the right tool for that shape,
and the plan output makes the mistake reviewable before apply.

`reconcile_anonymous_identifiers`,
`reconcile_anonymous_module_instances`, and
`detect_anonymous_to_named_renames` take `&StateBlockClaims` as a
required parameter. The parameter is not `Option`: a future caller cannot
run a heuristic rename pass without stating what the operator has already
claimed. Call-site inventory (per pass, verified): the per-resource
reconcile runs in all four commands and `fixture_plan.rs`; the
module-instance pass runs in `plan`, `apply`, `destroy`, and
`fixture_plan.rs` (not `state refresh`); the anonymous→named detector
runs only inside `create_plan_from_parsed_with_upstream` and `apply`.
`fixture_plan.rs` also materializes `moved` blocks for display fixtures,
so it constructs claims the same way the commands do. An empty claims
value is still an explicit `resolve_state_block_claims(&[], ...)` call
at every call site.

Two evaluation-time notes, pinned so implementers do not improvise:

- Effectiveness is evaluated once, at claims construction, against the
  state file as read. `materialize_moved_states` keeps resolving its
  `from` independently at its own (later) time, unchanged. The only shape
  where the two evaluations could diverge — a SimHash-branch re-key
  landing exactly on a claimed-ineffective `from` name — necessarily
  lands on a desired name and is therefore stopped by the RC3
  `from`-is-desired error before any transfer happens. (In the
  PR2-only window before PR 3 lands, that shape behaves exactly as
  today: the transfer happens and apply fails at writeback — no state
  corruption, the same failure point as now.)
- Chained `moved` blocks (`A→B`, `B→C`) are not supported today: `from`
  resolves only against the state file, which materialization never
  mutates, so the second block has always been a silent no-op. Under
  this design the second block emits the ineffective-block warning
  instead. That change from silent to warned is intended — the chain
  never did anything, and now it says so. Swap/rotation shapes (both
  `from`s live in state, e.g. `A→B` while `B→C`) have never completed
  either — depending on block order they corrupt at materialize or fail
  at writeback — and now error at plan time via the RC3 occupied-`to`
  and `from`-is-desired rules.

Inside the per-resource passes:

- a state entry matching a claimed `from` is excluded from match
  candidates (it belongs to the operator's block);
- a desired resource whose name matches a claimed `to` skips heuristic
  reconciliation (its state will arrive via the operator's block).

`reconcile_anonymous_module_instances` matches at module-instance
*prefix* granularity, so the rules lift to groups:

- a state prefix is excluded from the orphan candidates when **any**
  state entry under it is claimed as a `from`;
- a current (desired) prefix is not remapped when **any** desired name
  under it is claimed as a `to`.

Consequence, stated deliberately: one claimed resource pins its entire
module instance out of prefix heuristics, so an operator moving one
resource of an instance must write `moved` blocks for the instance's
other renamed siblings too. That is the conservative direction — the
alternative (heuristically remapping a prefix that rewrites a claimed
name mid-flight) re-opens the silent-steal hole at group scope.

`materialize_moved_states` keeps its position in the pipeline — with
claims excluded from the heuristics, the entries it needs are guaranteed
untouched.

**Claimed entries in `destroy` and `state refresh`.** These commands run
the heuristic passes but never materialize `moved` blocks. Under claims
exclusion a claimed state entry stays under its old name there: `destroy`
handles it through the orphan path it already uses for state entries with
no matching desired resource, and `state refresh` refreshes it in place
under its current address. Both are strictly safer than today's behavior,
where the heuristic may rebind the entry to an arbitrary sibling before a
destroy plan is computed.

**The third heuristic pass is covered too.**
`apply_anonymous_to_named_renames` (driven by
`detect_anonymous_to_named_renames`) synthesizes anonymous→`let`-name
rename pairs and transfers state the same way `moved` does. Although it
runs after `materialize_moved_states`, it matches against the **state
file**, which materialization never mutates (it transfers entries only
in `current_states` / `prev_explicit` / `saved_attrs`) — so the detector
can absolutely still select a claimed `from` as its rename source, and
its synthesized `to` (a binding name) can collide with a moved `to` and
overwrite the moved row in `current_states` — the same silent-row-drop
class. Its `&StateBlockClaims` parameter is therefore essential, not
belt-and-braces: it must not consume a claimed `from` state entry as its
rename source. The RC3 duplicate-`to` check runs over the combined
move-pair vector — operator `moved` pairs and synthesized rename pairs
together — so any cross-source `to` collision is an error, not an
overwrite.

**Ineffective `moved` blocks warn.** When `from` is not found in state:

- if `to` already exists in state, the block is an already-applied move —
  keep today's silent no-op (idempotent re-runs stay quiet);
- otherwise print a warning to stderr:
  `warning: moved block from <X> to <Y> was not applied: <X> not found in state`.

This gives operators the missing signal from symptom 2 without breaking
the documented idempotency of committed `moved` blocks. An ineffective
block also claims nothing (see "Only effective blocks claim"), so a stale
`from` cannot pin the block's `to` out of heuristic reconciliation
either — the warning and the claim release act together.

`removed` blocks have no `to` to disambiguate against, and with claims
exclusion in place a missing `from` can no longer mean "stolen by the
heuristic" — it legitimately means "already removed". Their silent no-op
behavior is unchanged.

### RC3 — a plan-time collision predicate over the move pairs

Introduce a collision predicate over plan-time data — the desired
resource id set, the combined `(from, to)` move-pair vector (operator
`moved` pairs plus synthesized anonymous→named rename pairs), and the
resolved `removed` targets. `plan` and `apply` call it right after the
pairs are produced and **fail** (hard error, not a warning) on:

- a move `from` that is also a desired resource id (the infra#138 shape:
  upsert and cleanup of the same id at writeback);
- a resolved `removed` `from` that is also a desired resource id (the
  same upsert/cleanup overlap reached through `Effect::Remove` — leaving
  it out would re-create the green-plan-broken-apply footgun through the
  sibling block kind);
- two pairs resolving to the same `to` (the second transfer would
  silently overwrite the first — today undetected even at writeback);
- two pairs sharing the same `from` (the second transfer silently no-ops
  yet still emits a Move effect — same silent-nonsense class, same
  vector scan to detect);
- a pair whose `from` resolves in state while its `to` is *also* already
  occupied in state (the transfer would overwrite a live, refreshed row —
  today an unwarned state-row drop; distinct from the from-absent /
  to-present shape, which stays the idempotent already-applied no-op).

This is a new early check, not an extraction: writeback's structural
`WritebackPlan` enforcement is interwoven with apply-only inputs
(`applied_states`, refresh/delete outcomes) and stays exactly as it is,
as the apply-time superset and defense in depth. The plan error message
mirrors the writeback one so existing operator knowledge transfers.

Two wiring details, pinned: the predicate obtains its resolved `removed`
targets from the same claims-construction resolution that decides
`removed` effectiveness — not a third `find_resource` site — and the
fixture-plan display path runs the predicate too, with the same hard
error, since it materializes the same pairs.

`apply` recomputes its plan under the lock, so the same check fails apply
early — before any provider mutation — instead of at writeback after
resources were already touched.

## Alternatives considered

### Restrict Hamming matching only, no meaning-based matching

Smallest fix for RC1: gate the fallback on `SimHash`-kind names and stop.
The scrambling disappears, but every hash shift on create-only resources
with deferred refs (the dominant shape) degrades to add+destroy churn the
operator must absorb with hand-written `moved` blocks. Rejected: the
1:1 mapping is mechanically derivable from data already in state, and the
operator workflow it forces is exactly the one symptoms 2 and 3 show to
be error-prone.

### Reorder the pipeline so `materialize_moved_states` runs first

Moving moved-materialization before reconciliation would also fix the
precedence, but it materializes into maps built during refresh, so the
reorder ripples through the refresh phase of four commands. Excluding
claims from the heuristic passes achieves the same precedence with a
narrower, type-enforced seam. Rejected as higher-risk for equal effect.

### Report heuristic matches in plan output instead of fixing precedence

The issue floats surfacing what SimHash matched so operators can write a
correct `moved.crn`. Useful as observability, but it keeps the silent
drop and makes the operator compensate for the tool's ordering. Rejected
as the primary fix; the plan-display improvement can ride along later if
still wanted once `moved` precedence makes it mostly moot.

### Warn (not fail) on plan-time collision

Symmetric with the state-drift warning philosophy, but unlike drift —
where apply re-checks under the lock and stays correct — this collision
makes apply fail unconditionally. A plan that is guaranteed to fail at
apply is not a valid prediction; printing it as green-with-a-warning
repeats the footgun. Rejected.

## Migration

No state format change. Behavioral changes visible to users:

- plans that previously scrambled sibling anonymous resources now
  reconcile silently — the desired resource adopts the existing state
  name (create-only branch), so the plan is a no-op and resources stay
  at their old hash addresses — or degrade to clean add+destroy pairs
  when meaning-based matching cannot resolve;
- previously silent ineffective `moved` blocks now warn;
- previously green plans carrying an upsert/cleanup collision now fail at
  plan time with the writeback error's wording.

The stuck-CI shape from infra#138 self-resolves, in either direction.
The *committed* `moved.crn` there is the symptom-3 file whose `from`
values were read off the plan's rebound names — they do not exist in the
on-disk state, so under this design every such block warns and claims
nothing, and RC1's meaning-based matching reconciles all five resources
to a no-op plan (with warnings prompting the operator to delete or fix
the stale file). A *corrected* 1:1 `moved.crn` (on-disk state hash →
desired hash, the symptom-2 file) is effective, claims its addresses,
applies fully as five moves, and becomes an idempotent no-op after one
successful apply.

## Planned reproducing tests

RC1 (`carina-core/src/identifier/tests.rs`):

- `test_reconcile_does_not_hamming_match_standard_hash_names` — three
  sibling create-only resources with deferred refs and disjoint
  state/desired 8-hex names; assert no cross-pairing (the issue's
  scrambling shape, captured from the infra#138 hash values).
- `test_reconcile_resolves_deferred_create_only_via_state_bindings` —
  the same shape with `binding`-carrying state entries; assert each
  desired resource reconciles to the state entry with equal resolved
  create-only values.
- `test_reconcile_deferred_create_only_ambiguous_refuses` — two state
  entries with identical create-only values; assert no match is made.
- `test_reconcile_simhash_tie_refused` — two SimHash state candidates at
  equal distance; assert no match (pins the deliberate first-found-min →
  refuse-tie change).
- `test_canonical_create_only_string_covers_concrete_scalars` — the
  extended canonical string handles `Int` / `Float` / `Bool` /
  `Duration` / `StringList`.
- A compile-time guarantee that `closest_unique_simhash_match` cannot be
  fed a `Standard` hash: the opaque `SimHash` newtype is the only
  accepted input type, and its only producers are `compute_simhash` and
  `SimHash::parse_16_hex`.

RC2 (`carina-core/src/identifier/tests.rs`,
`carina-cli/src/wiring/tests.rs`):

- `test_reconcile_skips_state_entry_claimed_by_moved_from` — heuristic
  pass must not consume a claimed `from` even when it would match.
- `test_reconcile_skips_desired_name_claimed_by_moved_to`.
- `test_detect_anonymous_to_named_skips_claimed_from` — the detector must
  not use a claimed `from` state entry as its rename source.
- `test_module_instance_reconcile_skips_claimed_prefix` — a claimed
  resource under a state prefix excludes the whole prefix from orphan
  candidates; a claimed desired name pins its current prefix.
- `test_materialize_moved_states_warns_on_missing_from` — `from` absent,
  `to` absent from state → warning emitted; `from` absent, `to` present →
  silent no-op.
- `test_stale_moved_block_releases_claims` — a `moved` block with a
  nonexistent `from` does not pin its `to`: the desired resource still
  reconciles via meaning-based matching and the plan stays a no-op
  (the infra#138 committed-file shape).
- `test_reconcile_skips_state_entry_claimed_by_removed_from` — an
  effective `removed` block's `from` entry is not consumed by the
  heuristics.
- Import claims: a `name_attribute`-style import target resolves to the
  desired resource's id and is excluded from heuristic reconciliation;
  an already-in-state or unresolvable target claims nothing; the
  accepted trade-off shape (import target claimed while the physical
  resource sits in state under an old hash) plans a visible import plus
  orphan delete.
- Claimed entries under commands without moved materialization: `destroy`
  routes a claimed state entry through the orphan path under its old
  name; `state refresh` refreshes it in place.
- An end-to-end fixture mirroring the issue: five disjoint renames, five
  1:1 `moved` blocks, assert a `0 add / 0 destroy / 5 move` plan.

RC3 (`carina-cli/src/commands/`):

- `test_plan_fails_on_moved_from_colliding_with_desired` — a fixture
  whose state genuinely contains an entry under a desired resource's id
  while a `moved` block names that id as `from`; assert plan errors with
  the collision message instead of printing a green move plan. (The
  literal infra#138 `moved.crn` no longer reaches the collision once PR 2
  lands — its `from` names are absent from state and produce the RC2
  warning instead — so the fixture constructs the colliding state
  directly.)
- `test_plan_fails_on_two_moves_to_same_target`.
- `test_plan_fails_on_two_moves_from_same_source`.
- `test_plan_fails_on_removed_from_colliding_with_desired`.
- `test_plan_fails_on_move_onto_occupied_state_entry`.
- `test_plan_fails_on_synthesized_rename_colliding_with_moved_to` — a
  synthesized anonymous→named rename pair and an operator `moved` pair
  sharing a `to`; assert the cross-source collision errors.
- Writeback's existing collision tests stay green — `WritebackPlan`'s
  structural enforcement is untouched.

## Implementation plan

Three implementation PRs, in order, each carrying its reproducing tests:

1. **PR 1 (RC1)** — opaque `SimHash` newtype + `AnonymousHashSuffix`
   typed extraction; restrict the Hamming fallback; meaning-based
   matching for deferred create-only values; canonical string coverage
   for concrete non-string values. carina-core + carina-cli: the pass's
   state access is the `find_state_by_type` callback built in the wiring
   layer, and binding-based resolution needs a cross-resource-type lookup
   that callback cannot express today, so its interface (and the call
   sites constructing it) changes too. The third Hamming consumer — the
   module expander's prefix matching
   (`module_resolver/expander.rs`, via `parse_synthetic_instance_prefix`)
   — moves onto the `SimHash`-only helper signature in the same PR.
2. **PR 2 (RC2)** — `StateBlockClaims` required parameter on all three
   heuristic passes (`reconcile_anonymous_identifiers`,
   `reconcile_anonymous_module_instances`,
   `detect_anonymous_to_named_renames`); candidate exclusion;
   ineffective-`moved` warning. carina-core + carina-cli (the four
   commands plus `fixture_plan.rs`).
3. **PR 3 (RC3)** — the plan-time collision predicate over the combined
   move-pair vector, wired into plan and apply as a hard error.
   carina-cli only.

PR 1 and PR 2 are independent; either order works, but RC1 first removes
the scrambling that makes RC2's symptoms hardest to reason about. PR 3
depends on neither but is most valuable last, when the remaining failure
shape is operator error rather than tool error.
