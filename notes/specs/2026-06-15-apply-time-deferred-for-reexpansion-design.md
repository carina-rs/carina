# Apply-time deferred-for re-expansion: Design

<!-- constrained-by ./2026-06-13-normalized-resources-typestate-design.md -->

## Status

Design proposal for carina-rs/carina#3561.

This document does not implement the change. It records the seam — both
the type shape and the scheduling shape — needed so that a
`for opt in <upstream>.<computed-collection> { ... }` block no longer
admits an `Unknown(ForValuePath)` placeholder into the apply path, and
so the deferred children are scheduled against the upstream's
create-step completion rather than against an arbitrary later lifecycle
event (terminal status, downstream `wait` satisfaction, etc.).

Implementation lands in a follow-up PR after this design merges.

## Problem statement

<!-- derived-from #status -->

carina#3554 (closed by PR #3558) and carina#3561 (this issue) are
symptoms of the same underlying shape. A DSL fragment of the form

```crn
let cert = aws.acm.Certificate {
  domain_name       = domain_name
  validation_method = dns
}

let validation_records = for opt in cert.domain_validation_options {
  aws.route53.RecordSet {
    hosted_zone_id   = zone_id
    name             = opt.resource_record.name
    type             = cname
    ttl              = 300
    resource_records = [opt.resource_record.value]
  }
}

let cert_issued = wait cert {
  until      = cert.status == aws.acm.Certificate.Status.issued
  depends_on = [validation_records]
  timeout    = 75min
}
```

reaches apply with three intertwined gaps:

1. **Value-resolution gap.** `expand_same_config_deferred_for` runs at
   plan time, once, against `current_states[cert]`. When `cert` is a
   Replace (or a first-time Create), the upstream's
   `domain_validation_options` collection does not yet hold the values
   the children's `opt.resource_record.name` / `opt.resource_record.value`
   references resolve to. The expansion stamps each placeholder as
   `Value::Deferred(DeferredValue::Unknown { reason: UnknownReason::ForValuePath { path } })`
   and the apply path inherits the stamped child verbatim. The Route 53
   `Create` then calls `resolve_resource` → `assert_fully_resolved`
   and reaches the WASM serialization boundary with a still-`Unknown`
   value, producing the error in #3561's reproduction:

   ```
   cannot serialize at WASM provider boundary: value is not yet known
     (deferred for-binding value opt.resource_record.name)
   ```

2. **Scheduling gap (across phase boundaries).** PR #3558 added the
   iterable binding (`cert`) to each expanded child's
   `dependency_bindings` to express "this child depends on `cert`".
   The phased executor (`carina-core/src/executor/phased.rs`) runs in
   five buckets:

   | Phase | Members                                                               |
   | ----- | --------------------------------------------------------------------- |
   | 1     | Non-Replace, non-Read, non-post-replace-wait effects                  |
   | 2     | CBD-Replace creates (sorted)                                          |
   | 3     | Replace deletes (sorted reverse)                                      |
   | 4     | Non-CBD-Replace creates + CBD finalize (sorted)                       |
   | 5     | Waits whose `target_id` is a Replace in this run                      |

   The deferred-for child (`Effect::Create validation_records[0]`) is a
   Phase 1 member. The upstream `cert` is the legacy replace effect, placed in
   phases 2/3/4. `build_phase_dependency_map` builds a `binding_to_idx`
   *limited to the current phase's effects*; cert is not in Phase 1's
   index, so the resolver's `expand("cert", …)` walk drops to the
   "binding not found in this phase" branch and contributes no
   dependency edge. The result is that `dependency_bindings = {"cert"}`
   is silently a no-op in Phase 1 and the child is dispatched
   immediately — exactly the race #3554 set out to close, except the
   close was only effective when both effects landed in the same phase
   bucket. Replace-shaped upstreams (the common case here) slip through.

3. **Wait/depends_on consistency.** #3561 also notes the downstream
   pattern `wait cert { until = cert.status == … depends_on = [validation_records] }`
   only does what the DSL says if the wait does not also wait on
   `cert`'s downstream lifecycle events. PR #3559 expanded
   `Effect::blocking_bindings()` to return `[target_binding, …explicit_dependencies]`
   for waits, which is correct: the wait blocks on `cert`'s
   create-step completion *and* on the explicit `validation_records`
   binding being satisfied. Whatever the deferred-for child waits on
   for value resolution must match what the wait already treats as
   "cert is mutator-complete", so the two paths cannot disagree on
   when the upstream's create step is considered done.

The original carina#3554 fix targeted (2) at the parser level
(`build_expanded_child` inserts `cert` into the child's
`dependency_bindings`). PR #3558 was correct in direction but
incomplete: it landed only the scheduling-graph edge, did not touch
(1) at all, and could not reach across the phase boundary on its own.
The follow-up issue (#3561) names this directly: the scheduling edge
"appears to gate the deferred items on `X` reaching a *terminal-success*
state" — which is one valid reading of the symptom, but the deeper
question is why the scheduling edge can even exist in a shape that
permits this misreading.

### What the AWS contract actually says

The issue body asserts that ACM's `RequestCertificate` returns
`DomainValidationOptions` populated. The AWS API reference is more
restrictive:

> After successful completion of the `RequestCertificate` action,
> there is a delay of several seconds before you can retrieve
> information about the new certificate.

`RequestCertificate`'s response payload is `{ "CertificateArn": "…" }`
— `DomainValidationOptions[].ResourceRecord` is **not** in the
response. It is populated on `DescribeCertificate` after a
several-second delay. The provider's `Create` implementation is the
right boundary to decide when "create step completed" means
"`DomainValidationOptions[].ResourceRecord` is readable" — not the
DSL, not the scheduler. Carina's contract should be "the upstream
provider's `Create` returned a `State` whose attributes are
readable"; the per-resource semantics of *when that is true* live
inside the provider.

This bounds the scope: carina-core does not encode "wait for
`DomainValidationOptions` to be populated" as a special case. It
encodes "the deferred-for child re-expands and re-resolves once the
upstream's `Create` step has returned a `State`, whatever the
provider chose to put in that `State`". If the AWS provider chooses
to block its `Create` until `DescribeCertificate` returns populated
DVOs, that is a provider-side correctness decision; if it returns
sooner and the deferred-for re-expansion still gets an empty
collection, the failure mode is a clear empty-collection result, not
a stale `Unknown(ForValuePath)` placeholder.

## Goals and non-goals

<!-- derived-from #problem-statement -->

Goals:

- Make `Value::Deferred(DeferredValue::Unknown { reason: ForValuePath { … } })`
  **unrepresentable on resources that reach the apply path**. The
  `resolve_resource` / `assert_fully_resolved` seam is the last line
  of defence today; the design should move it from "the last defense
  catches the bug" to "the bug cannot be constructed".
- Express the deferred-for child's dependency on its upstream's
  **create-step completion** as a scheduling edge that crosses phase
  boundaries by construction, not by remembering to thread the
  binding through each phase's `binding_to_idx`.
- Keep #3559's contract intact: `wait <upstream> { depends_on = […] }`
  blocks on its target's create-step completion *and* on its explicit
  dependencies — the deferred-for child landing as one of those
  explicit dependencies remains coherent.

Non-goals:

- Reworking the four-phase pipeline. The phases are load-bearing for
  CBD Replace semantics and are out of scope here. The deferred-for
  fix slots into the existing pipeline.
- Provider-side semantics of "create completed". Whether a provider
  blocks its `Create` until a follow-up read returns populated
  attributes is a per-provider decision. carina-core's contract is
  "the `Create` future resolved with a `State`".
- Generalizing to `for_each` over arbitrary maps, datasource-deferred
  attributes, or upstream-stack-deferred attributes. Those produce
  `Unknown` values from different sources (`UpstreamRef`,
  `UpstreamBareRef`) and have their own resolution model already
  (`wait` blocks, `upstream_state` reads). The design here addresses
  only `ForValuePath` placeholders produced by the deferred-for
  expansion against a *local managed resource*'s collection.

## Design

<!-- derived-from #goals-and-non-goals -->

The fix has two components that must land together: a typed seam that
keeps `Unknown(ForValuePath)` out of resources the apply executor sees,
and an explicit re-expansion effect that produces the resolved
children once the upstream's `Create` step completes.

### 1. Typed seam: a resolved-resource witness

`carina-core::executor::basic::resolve_resource` already rejects
`Value::Deferred(Unknown)` via `assert_fully_resolved` and returns a
`NormalizedResource` on success. That return type is the existing
seam; the design widens it slightly so the witness travels with the
value instead of being re-checked at every site that consumes a
resource.

Today the resource graph carries `Resource` everywhere — both before
and after `resolve_resource`. The scheduler holds
`UnresolvedResource(Resource)` (a newtype) but the executor's
intermediate types still degrade to bare `Resource` once resolution
ran. The deferred-for expansion runs on a `Resource` and produces a
`Resource`; nothing in the type system records "this child's
attributes still contain `Unknown(ForValuePath)` placeholders" vs
"this child's attributes are fully concrete".

Proposal: introduce a `ResolvedResource` witness type that wraps a
`Resource` whose attribute tree has been walked and found free of any
`Value::Deferred` arm. Only `resolve_resource` /
`resolve_resource_with_source` (and a new
`reresolve_after_expansion`, below) can construct one. The basic
executor's `Provider::create` / `update` / `delete` call sites
already take normalized resources; they shift to take
`ResolvedResource` so that no path reaches the WASM boundary with a
raw `Resource`.

This composes with the existing `BasicEffect` typestate (introduced
in carina#3164): `BasicEffect::Create { resource: &'a Resource }`
becomes `BasicEffect::Create { resource: &'a ResolvedResource }` once
the executor has resolved it, and the apply-time `provider.create`
takes that witness. A deferred-for child cannot reach this seam until
it has been re-expanded; the type system enforces it.

Concretely, the new shape sits in `carina-core/src/resource.rs`
alongside `Resource`:

```rust
/// A resource whose attribute tree has been walked and is free of
/// any `Value::Deferred` placeholder. Constructed only by the
/// `resolve_*` family in `carina-core::executor::basic`, so a raw
/// `Resource` cannot be coerced into one.
#[derive(Debug, Clone)]
pub struct ResolvedResource(Resource);
```

with a `pub(crate)` constructor in the executor's resolve helpers
(`fn from_resolved(r: Resource) -> ResolvedResource` lives next to
`assert_fully_resolved`, so the only path that produces the witness
runs the check). `NormalizedResource` (the existing wrapper around
post-normalization output) gains a `ResolvedResource` accessor; the
provider trait stays on `Resource` at the WIT boundary, but the
host-side trampoline (`provider.create`/`update`) takes a
`&ResolvedResource` so the host code path proves the invariant.

This is the same shape as carina#3164's `BasicEffect` and
carina#3349's opaque `AttributeType`: a typestate that makes the
broken state unrepresentable at the call sites that previously had to
remember to filter.

### 2. Apply-time re-expansion: a new effect variant

Add `Effect::ExpandDeferredFor` (working name) carrying the
information needed to re-run the same expansion that
`expand_same_config_deferred_for` performed at plan time, but against
the freshly-applied upstream `State` rather than the plan-time
snapshot.

```rust
Effect::ExpandDeferredFor {
    /// The iterable binding whose attributes drive the expansion
    /// (e.g. "cert").
    upstream_binding: String,
    /// Synthetic id used for plan-tree display and progress.
    id: ResourceId,
    /// The template carried verbatim from the plan-time deferred
    /// expression; replayed against the post-apply upstream state.
    template: DeferredForExpression,
}
```

`ExpandDeferredFor` is a state-only effect, like `Move` / `Import` /
`Remove`: it does not call the provider. Its action is to walk
`applied_states[upstream_binding]`, re-run the `for_expr` body
substitution against the resolved upstream attributes, and emit a
fresh `Effect::Create(ResolvedResource)` (or a sequence of them, one
per collection element) into the in-flight plan.

The scheduler's contract:

- `ExpandDeferredFor.blocking_bindings()` returns
  `[upstream_binding]`.
- The synthesised child `Effect::Create` calls inherit the original
  child's `dependency_bindings` minus the iterable binding (which
  is now satisfied by construction), plus any explicit
  `depends_on`.
- Because `ExpandDeferredFor` consults `applied_states`, the
  scheduler can place it in **the same phase as the upstream's
  finalization**: Phase 4 for a Replace, Phase 1 for a first-time
  Create, but always *after* the upstream's create step has
  resolved within that phase. The "create-step completion" edge
  becomes structural: the expand effect cannot fire until the
  upstream's `applied_states` entry exists, and the upstream's
  `applied_states` entry is only written after its `Create` future
  resolves.
- Children synthesised by `ExpandDeferredFor` are appended to the
  Phase 4 (or relevant later phase's) ready set; they are not part
  of Phase 1 at all, removing #3561's phase-1-cannot-see-cert
  problem from the picture.

The plan-time path no longer emits the pre-expanded children
directly. Instead, when a deferred-for expression references an
attribute whose current-state value is missing or marked as deferred,
`expand_same_config_deferred_for` emits a single
`Effect::ExpandDeferredFor` for that iterable. When the iterable
*is* fully knowable at plan time (the upstream is a no-op or an
Update whose `domain_validation_options` survives), expansion stays
at plan time as today — the new effect only appears when re-expansion
is actually needed.

A `wait cert { until = … depends_on = [validation_records] }` block
sees `validation_records` resolved (by the time the ExpandDeferredFor
effect has produced concrete children and those children have run)
exactly when #3559 expects: the wait's
`explicit_dependencies = {"validation_records"}` is satisfied when
every synthesised child completes. No DSL change is needed for the
downstream `wait`/`depends_on` pattern to keep working.

### 3. Interaction with PR #3558's `dependency_bindings.insert`

PR #3558's `dependency_bindings.insert(iterable_binding)` was a
direction-correct fix for a same-phase case (the upstream and the
deferred child both being `Effect::Create` in Phase 1). With the new
effect, the pre-expanded child path no longer exists when the
upstream is unresolved at plan time — the `ExpandDeferredFor` effect
takes over and emits children whose dependency edge is implicit in
the scheduling order. PR #3558's insertion remains valid for the
fully-knowable-at-plan-time path (children that *were* expanded at
plan time still need the binding edge so the scheduler waits for
upstream `Update`s in Phase 1), so the line stays — it just no
longer carries the load it could not reach.

## Alternatives considered

<!-- derived-from #design -->

### A. Scheduling-only fix (issue body's suggestion (1))

Refine PR #3558's edge so the deferred child waits for the upstream's
`Create`-step completion specifically, by adding an
`upstream_create_completion` lookup that crosses phase boundaries
without re-expansion.

Rejected. The `Unknown(ForValuePath)` placeholder still reaches the
apply path; `resolve_resource` still has to catch it; a future
caller adding another deferred-pattern (datasource-deferred attribute,
nested for-binding) re-introduces the same class of bug because the
type system still permits the broken state. This is the per-site
runtime filter pattern that this repo's root-cause rule explicitly
forbids.

### B. Honour `depends_on = [validation_records]` on the wait as the
authoritative edge (issue body's suggestion (2))

This is the second framing of the same gap as A; PR #3559 already
landed the wait-side half of the edge. By itself it does not address
the deferred-for value-resolution problem — the children still need
their `opt.X` placeholders resolved.

### C. Phase consolidation

Merge the four phases into a single scheduler so cross-phase
dependencies fall out naturally. Mentioned only to be set aside: the
phases encode CBD Replace lifecycle ordering that is independently
load-bearing, and the deferred-for fix does not require touching
them.

## Test plan

<!-- derived-from #design -->

- Unit: a `Resource` containing
  `Value::Deferred(DeferredValue::Unknown { reason: ForValuePath { … } })`
  cannot be wrapped in `ResolvedResource` — the constructor is
  `pub(crate)` in `executor::basic`; a `compile_fail` doctest pins
  it.
- Unit: `Effect::ExpandDeferredFor::blocking_bindings()` returns
  `[upstream_binding]` and is the only blocker.
- Unit: the planner emits `Effect::ExpandDeferredFor` when the
  iterable's current-state value is missing or marked deferred, and
  emits pre-expanded `Effect::Create` children otherwise. A single
  fixture covers both branches.
- Integration: a `for opt in cert.domain_validation_options { … }`
  fixture where `cert` is the legacy replace effect. Assert the apply event
  trace is `Replace cert → ExpandDeferredFor → Create validation_records[N]`
  and no `Create validation_records[N]` event is emitted before the
  `Replace cert` completes (the #3554 race).
- Integration: the same fixture with
  `wait cert_issued { depends_on = [validation_records] }` reads the
  wait fires *after* every synthesised child's `Create` completes —
  the #3561 deadlock cannot recur because the wait does not block on
  `cert` reaching its `until` predicate before the children land.
- Regression: PR #3558's
  `expand_deferred_for_children_depend_on_iterable_binding_for_all_binding_shapes`
  stays green for the plan-time-knowable branch (the
  `dependency_bindings` insertion still applies there).

## Open questions

- Is `ExpandDeferredFor` better modelled as a state-only effect
  (alongside `Move` / `Import`) or as a new "synthetic generator"
  variant? The former matches the existing executor seam (the CLI's
  `execute_state_only_effects` step) but the latter may be clearer
  in plan display. Choose at implementation time.
- Plan display: how is `ExpandDeferredFor` rendered? Likely as a
  silent step (no row) when the children are also shown — the user
  cares about the children, not the meta-effect. Confirm against
  the existing plan tree's display of `for` blocks before
  implementation.
- Does the design need to handle the case where re-expansion at
  apply time *shrinks* the collection compared to the plan-time
  prediction (e.g. plan expected 2 children, runtime sees 1)? The
  current plan-time pre-expansion handles this by stamping each
  predicted child; the apply-time re-expansion is more honest about
  it because it sees the real count. The orphan reconciliation
  behaviour for "predicted-but-not-realised" children needs a brief
  note in the implementation PR.
