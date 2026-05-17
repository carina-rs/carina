# `for` over a same-config provider-read attribute: Design

<!-- derived-from #root-cause -->
<!-- constrained-by ./2026-04-19-unify-resource-walk-design.md -->

## Goal

Make a `for` loop whose iterable is a **same-config** provider-read /
provider-computed attribute (e.g. `for opt in cert.domain_validation_options`
where `let cert = aws.acm.Certificate { … }` is declared in the same
configuration) representable in `carina validate` and `carina plan`,
so that:

1. `carina validate` enumerates the loop body (it has a parsed
   template today; it is simply never displayed).
2. `carina plan` expands the loop against the **refreshed** value of
   the same-config iterable, surfaces the loop-generated resource
   addresses, and lets SimHash reconcile the prior `let`-bound
   resource into the loop body instead of emitting destroy-only.

This is the **design document only**. Implementation follows in
separate PRs after this design merges, per the repo's split-PR policy
for large refactors (`CLAUDE.md` → "Design PR must merge before
implementation PR").

This document deliberately does **not** adopt any of the three
open-ended directions enumerated in issue #3121. Per the issue author's
instruction, it targets the underlying structural defect instead of
the symptoms.

## Root cause

<!-- constrained-by ../../CLAUDE.md -->

GitHub issue #3121. Rewriting

```crn
let cert = aws.acm.Certificate { domain_name = d; validation_method = dns; … }
let validation_record = aws.route53.RecordSet {
  name             = cert.domain_validation_options[0].resource_record.name
  resource_records = [cert.domain_validation_options[0].resource_record.value]
  …
}
```

into

```crn
for _, opt in cert.domain_validation_options {
  aws.route53.RecordSet {
    name             = opt.resource_record.name
    resource_records = [opt.resource_record.value]
    …
  }
}
```

makes the loop body invisible to `validate` and `plan`, and `plan`
against existing state emits the prior `validation_record` as
**destroy-only with no replacement**. Applying the rewrite would
destroy the live DNS validation record and not re-create it.

### Defect 1 — `validate` display bypasses the unified walk

`carina-cli/src/commands/validate.rs` reports resources via
`parsed.resources.len()` and `parsed.resources.iter()` directly:

```rust
resource_count: parsed.resources.len(),
resources: parsed.resources.iter().map(|r| r.id.to_string()).collect(),
…
for resource in &parsed.resources {
    println!("  • {}", resource.id);
}
```

`ParsedFile::iter_all_resources()` already exists
(`carina-core/src/parser/ast.rs:742-751`) and already yields the
`template_resource` of every `DeferredForExpression`, tagged with
`ResourceContext::Deferred`. Its own doc-comment says callers "should
prefer this over `self.resources.iter()` so they stay in sync with
for-body code", and the shipped design
`notes/specs/2026-04-19-unify-resource-walk-design.md` made every
*checker* use it. The **validate display layer was never converted**,
so a loop body that exists as a parsed template is silently dropped
from the `✓ N resources validated successfully.` list. This is a
direct violation of the already-shipped single-source-of-truth
design and is independently fixable with no behavior change beyond
the count/list.

### Defect 2 — same-config read iterables are structurally unreachable by loop expansion

At parse time `cert.domain_validation_options` is an unresolved
provider-computed reference, so the for-expression lands in the
deferred branch
(`carina-core/src/parser/expressions/for_expr.rs:229`) and records a
`DeferredForExpression` with `iterable_binding = "cert"`,
`iterable_attr = "domain_validation_options"`, and a
`template_resource`. **Zero resources enter `parsed.resources`.**

The only de-deferral mechanism is
`ParsedFile::expand_deferred_for_expressions`
(`carina-core/src/parser/ast.rs:784-801`), which resolves the iterable
**exclusively** from `remote_bindings`:

```rust
let iterable = remote_bindings
    .get(&deferred.iterable_binding)            // "cert"
    .and_then(|attrs| attrs.get(&deferred.iterable_attr));
let Some(iterable_value) = iterable else { continue; };
```

`remote_bindings: HashMap<String, HashMap<String, Value>>` is
populated **only** by `load_upstream_states`
(`carina-cli/src/commands/plan.rs:653-710`, twin at
`apply/mod.rs:783`), which iterates `parsed.upstream_states` and reads
*other* configs' state backends. A same-config `let cert` is **never**
an `upstream_state`, so `remote_bindings.get("cert")` is always `None`
and the template stays deferred forever.

The refreshed value of `cert.domain_validation_options` *does* exist
at plan time — but in `current_states: HashMap<ResourceId, State>`
(`carina-cli/src/wiring/mod.rs:1150`, populated by the refresh phase
at `:1243`), which is:

- built **after** `expand_deferred_for_expressions` has already run
  (`plan.rs:338` is before `create_plan_from_parsed_with_upstream`,
  inside which refresh happens),
- keyed by `ResourceId`, not by `binding`/`attr`,
- never fed back into deferred-for expansion.

So the data needed to expand the loop is computed, but on the wrong
side of an ordering boundary and in an incompatible shape, with no
bridge between them. There is, today, **no mechanism by which a
same-config provider-read attribute can feed a deferred-for
iterable** (verified by tracing every consumer of
`DeferredForExpression.iterable_binding`/`iterable_attr` and every
writer of `remote_bindings`).

### Defect 2b — expansion runs after SimHash reconcile (ordering)

Even if the iterable were resolvable, `plan.rs` runs
`reconcile_anonymous_identifiers_with_ctx(&wiring, &mut parsed.resources, sf)`
at `:315` — **23 lines before**
`parsed.expand_deferred_for_expressions(&remote_bindings)` at `:338`.
SimHash reconcile is given `parsed.resources` *before* the loop body
is expanded into it, so even a correctly-expanded loop resource would
miss reconciliation and could not absorb the prior `let`-bound state
entry. The prior `validation_record` state entry has no candidate to
SimHash-match against → orphan → destroy-only.

### Net root-cause statement

A `for` over a same-config provider-read attribute is parsed into a
template that **never** becomes a real resource, because the sole
expansion mechanism reads from the upstream-state-only
`remote_bindings` map while the iterable's actual value lives in the
post-refresh, `ResourceId`-keyed `current_states` map produced later
and never bridged back. Combined with (a) the validate display layer
bypassing the unified `iter_all_resources()` walk and (b) loop
expansion running after SimHash reconcile, the loop body is invisible
to `validate`, invisible to `plan`, and the prior named binding is
emitted as silent destroy-only.

## Non-goals

- The three speculative directions in issue #3121 (per-symptom
  diagnostic, placeholder-element expansion, explicit `moved`-to-loop
  syntax). They patch symptoms; this design removes the cause.
- Converting **all** plan/apply `parsed.resources` sites to the
  unified walk in one move. The 2026-04-19 design intentionally left
  the plan/apply half on direct access with `// allow: direct`
  annotations and a CI lint; a blanket conversion is a separate,
  larger refactor and is **out of scope** here. This design converts
  only the sites on the critical path for deferred-for expansion and
  reconcile, and states the invariant that keeps the rest correct.
- Cross-config (`upstream_state`) for-loops — already work; this
  design must not regress them.
- `for` over a same-config iterable that is *not* provider-computed
  (already resolved at parse time → already works).

## Chosen approach

### Principle: one expansion source, ordered before reconcile

Make same-config refreshed values a **first-class iterable source**
for deferred-for expansion, expressed in the *same* type the existing
expansion already consumes, and move expansion to run **before**
anonymous-identifier reconcile on both the plan and apply paths.

### A. `validate` display → `iter_all_resources()` (Defect 1)

`carina-cli/src/commands/validate.rs` reports via
`parsed.iter_all_resources()` instead of `parsed.resources`. Deferred
templates are tagged `ResourceContext::Deferred`; render their address
as the loop placeholder form already used internally
(`for_expr.rs:274` → `"{binding_name}[?]"`) so the user sees, e.g.:

```
✓ 11 resources validated successfully.
  • aws.acm.Certificate.r.cert
  • aws.route53.RecordSet.r.<for-body>[?]      (deferred: for _, opt in cert.domain_validation_options)
  …
```

Exact rendering string is an implementation detail; the **invariant**
is: validate's resource count and list MUST be
`iter_all_resources()`-derived, never `parsed.resources`-derived.
This is a strict subset of the already-shipped 2026-04-19 design and
carries no risk to the plan/apply path. It is the first implementation
PR and can land independently of B/C.

### B. Bridge same-config refreshed values into deferred-for expansion (Defect 2)

After the refresh phase populates `current_states`
(`wiring/mod.rs:~1243`) and *before* the plan diff is built, project
the deferred-for iterables out of `current_states` into the **exact
shape `expand_deferred_for_expressions` already accepts**
(`remote_bindings: HashMap<String, HashMap<String, Value>>`), then run
expansion against the union of upstream + same-config bindings.

Concretely, for each `DeferredForExpression d` still unresolved:

1. Resolve `d.iterable_binding` (e.g. `"cert"`) to the same-config
   resource's `ResourceId` via the existing binding→id resolution
   already used by `resolve_refs_for_plan`
   (`carina-cli/src/wiring/mod.rs:1438`, `carina-core` resolver).
2. Look up that `ResourceId` in `current_states`; read
   `state.attributes[d.iterable_attr]` (the refreshed
   `domain_validation_options` list value).
3. Insert it into a `HashMap<String, HashMap<String, Value>>` keyed
   `{ binding → { attr → value } }` — structurally identical to a
   `remote_bindings` entry.
4. Call the **existing** `expand_deferred_for_expressions` with the
   merged map (upstream entries unchanged + the new same-config
   entries). All shape-mismatch / list-vs-map handling is reused
   unchanged.

No new expansion code path, no new `Value` variant, no WIT change.
The single source of truth for "how a deferred-for expands" stays
`expand_deferred_for_expressions`; we only widen *where its iterable
values come from*, in its already-existing input type. This is the
type-safety lever: the bridge produces the same `HashMap` type the
function already takes, so there is no second expansion implementation
to drift.

#### Where the bridge lives

`create_plan_from_parsed_with_upstream`
(`carina-cli/src/wiring/mod.rs`) already owns `current_states` and the
binding resolver. The bridge is a private helper there:

```rust
fn same_config_for_iterables(
    deferred: &[DeferredForExpression],
    current_states: &HashMap<ResourceId, State>,
    resolve_binding: &dyn Fn(&str) -> Option<ResourceId>,
) -> HashMap<String, HashMap<String, Value>>;
```

Plan path: call it after refresh phase 1/2, merge into the
`remote_bindings` clone, run `expand_deferred_for_expressions`, *then*
the diff. Apply path: the symmetric site in `apply/mod.rs` (the twin
of every step listed in the root cause) gets the identical treatment —
plan and apply must not diverge on this axis
([[feedback_unit_test_path_is_not_apply_path]]).

#### `--refresh=false`

When refresh is disabled, the same-config iterable value is taken from
the **cached state file** (`wiring/mod.rs:1359-1363` already inserts
cached state into `current_states` in that mode), so the bridge reads
the cached value uniformly — no special case. If the cached state has
no entry for the iterable resource (never applied), the deferred
entry stays deferred and validate/B-display still surfaces the
template (Defect 1 fix), so the user is not blind.

### C. Order expansion before SimHash reconcile (Defect 2b)

Move deferred-for expansion (now fed by B's merged map) to run
**before** `reconcile_anonymous_identifiers_with_ctx` on both paths:

- Plan: today `reconcile` at `plan.rs:315`, expansion at `:338`.
  Reorder so expansion (against upstream-only bindings *and*, once
  inside `create_plan_…` where `current_states` exists, the
  same-config bridge) precedes reconcile. The cleanest shape: expand
  upstream-resolvable loops where they are today; for same-config
  loops, expansion necessarily happens inside `create_plan_…` after
  refresh, so SimHash reconcile for the loop-vs-prior-named-binding
  case must also move inside `create_plan_…`, after the bridge
  expansion and before diff. The implementation PR will pick the
  precise call-graph cut; the **invariant** is: *no anonymous-id
  reconcile observes a `parsed.resources` that is still missing an
  expandable deferred-for body.*
- Apply: same invariant at the `apply/mod.rs` twin sites.

After C, the expanded loop body (e.g.
`aws.route53.RecordSet.r.<binding>[0]`) is a normal anonymous
resource present before reconcile. SimHash compares it against the
prior `let validation_record` state entry; with the body attributes
(`name`, `resource_records`, `type`, `ttl`) substituted from the
refreshed `domain_validation_options[0]`, the Hamming distance is
within `SIMHASH_HAMMING_THRESHOLD` (the only structural change is the
address form), so reconcile re-keys the state entry onto the loop
address → **update/no-op, not destroy+recreate**. The ACM
re-validation hazard is eliminated for the 1-element-today case and
the loop scales to N elements (SANs) without further surgery.

### Why this is the type-safe, long-term shape

- **One expansion function, one input type.** B feeds the *existing*
  `expand_deferred_for_expressions` through its *existing*
  `HashMap<String, HashMap<String, Value>>` parameter. No parallel
  expander, so plan and apply cannot diverge on loop semantics.
- **The unified-walk invariant is extended, not re-litigated.** A
  builds on the already-shipped 2026-04-19 design and its CI lint
  (`scripts/check-no-direct-resources-access.sh`) rather than
  inventing a new mechanism. The plan/apply sites that stay on direct
  `parsed.resources` remain correct *because* expansion now completes
  before they run (invariant in C) — the design states this
  precondition explicitly so future changes can be checked against
  it.
- **No new DSL surface, no WIT/protocol change, no new `Value`
  variant.** The fix is data-flow ordering + a projection helper,
  which is far less surface area to maintain than an explicit
  `moved`-to-loop syntax (issue direction 2) and does not bake a
  symptom workaround into the language.

## Implementation PR breakdown (post-merge)

Strict order; each verified independently:

1. **PR-1 (Defect 1, A):** `validate` display → `iter_all_resources()`.
   Multi-file `.crn` directory fixture (per `CLAUDE.md`
   directory-scoped rule) with a deferred-for over a same-config
   read; assert the body appears in `validate` count + list and in
   `--json` output. Snapshot/fixture for plan-display unaffected.
   Lands independently; no plan-path risk.
2. **PR-2 (Defect 2, B + Defect 2b, C):** the same-config iterable
   bridge + reorder, plan path and apply path together (they must not
   split — apply-path divergence is the recurring trap
   [[feedback_unit_test_path_is_not_apply_path]]). Acceptance test
   reproduces issue #3121: prior state with `let validation_record`,
   rewrite to `for opt in cert.domain_validation_options`, assert
   plan shows *no destroy* of the prior record and the loop body as
   update/no-op after SimHash re-key. Real-infra smoke
   (`carina-rs/infra` registry/dev ACM rewrite) is the named
   acceptance condition in #3121 — run the built binary against it
   (user-driven per [[feedback_no_real_infra_aws_commands]]).

Both PRs `Closes #3121` only on PR-2 (PR-1 uses `refs #3121`), since
#3121's headline symptom (destroy-only) is not resolved until PR-2.

## Risks / open questions

- **Binding→ResourceId resolution for the bridge.** B step 1 must
  reuse the *exact* resolver `resolve_refs_for_plan` uses, not a
  reimplementation, or the bridge could key on a stale/renamed id.
  Implementation PR must thread the existing resolver, not duplicate
  it. (Type-safety follow-through.)
- **Loop body attribute shape vs SimHash threshold.** The claim that
  the rewritten body lands within `SIMHASH_HAMMING_THRESHOLD` of the
  prior `let` binding must be proven by a real fixture test, not
  assumed ([[feedback_unit_test_path_is_not_apply_path]] — assert on
  the actual plan output, not a unit call to `compute_simhash`).
- **Multiple same-config loops referencing each other.** Out of scope;
  a `for` whose iterable is itself a loop-generated resource's
  computed attribute is not addressed here. State it as a known
  limitation; do not silently mis-expand — if encountered, the entry
  stays deferred and surfaces via the Defect-1 display.
- **Ordering cut for C.** Whether SimHash reconcile for the
  same-config case moves wholesale into `create_plan_…` or only the
  loop-relevant subset is a call-graph decision left to PR-2; the
  invariant ("no reconcile sees an unexpanded expandable deferred
  body") is the contract, the cut is the implementation's.
