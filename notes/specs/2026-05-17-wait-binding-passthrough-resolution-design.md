# `wait`-binding ResourceRef Passthrough Resolution: Design

<!-- derived-from ./2026-05-09-wait-construct-design.md -->
<!-- constrained-by ./2026-05-09-wait-construct-design.md#value-semantics -->

## Goal

Make `<wait-binding>.<attr>` (e.g. `cert_issued.certificate_arn`)
resolve to the **same value** as `<target>.<attr>` (e.g.
`cert.certificate_arn`) during ResourceRef resolution — the passthrough
semantics the wait-construct design already promises but the resolver
never implemented — so a downstream resource that references a wait
binding stops rendering a never-converging phantom diff.

Trigger: carina#3085. `carina plan` on `carina-rs/infra` shows:

```
> r.cert_issued (until cert.status == aws.acm.Certificate.Status.issued)
      └─ ~ awscc.cloudfront.Distribution r.distribution
            distribution_config:
                viewer_certificate:
                  ~ acm_certificate_arn: "arn:aws:acm:us-east-1:...:certificate/3fc2..." → r.cert_issued.certificate_arn
```

State holds the resolved ARN; desired holds the **unresolved
`ResourceRef`** `r.cert_issued.certificate_arn`, every plan, never
converging.

## The real root cause (verified against code)

The wait-construct design `notes/specs/2026-05-09-wait-construct-design.md:112`
specifies passthrough:

> `<wait-binding>.<attr>` resolves to `<target>.<attr>` (passthrough)
> ... `cert_issued.arn` has the same type and content as `cert.arn`;
> the difference is purely the dependency edge in the execution graph.

The resolver does not implement it:

- `ResolvedBindings::from_resources_with_state(resources,
  current_states, remote_bindings)` (`carina-core/src/binding_index.rs:343`)
  builds the resolution map from **three inputs only**: top-level
  resources, last-known state, upstream bindings. It never sees
  `wait_bindings`.
- `BindingNameSet::from_parsed` (`binding_index.rs:215`) registers
  eight declaration forms (`Resource`, `ModuleCall`, `UpstreamState`,
  `Argument`, `Use`, `UserFunction`, `Structural`, `Variable`) — there
  is **no wait form**. `BindingNameKind` (`binding_index.rs:156`) has
  no `Wait` variant.
- `resolve_refs_for_plan(&mut resources, &current_states,
  remote_bindings)` (`carina-cli/src/wiring/mod.rs:1416`) is called
  **without** `wait_bindings`. `parsed.wait_bindings` is threaded only
  later into `create_plan` (`wiring/mod.rs:1449`) for `Effect::Wait`
  lowering; the apply path (`carina-cli/src/commands/apply/mod.rs:1023`)
  does the same. Neither path feeds wait bindings into resolution.
- So `resolve_ref_value` (`carina-core/src/resolver.rs:178`) does
  `bindings.get("cert_issued")` → `None` → falls through to
  `Ok(value.clone())` (`resolver.rs:254`), returning the unresolved
  `ResourceRef`. The non-resolved ref reaches the differ against the
  state's concrete ARN → permanent phantom.

The wait construct's real-infra (infra T6 registry usecase)
verification was explicitly deferred when carina#2825 landed
(handoff `handoff_2026-05-11_wait_acm_complete`); this is that deferred
gap surfacing. It is independent of and orthogonal to carina#3080
(Union scalar/list, merged).

## Why this is carina-core, not a provider concern

Same layering as carina#3073 / #3080: the differ/renderer see an
unresolved value because *resolution upstream did not produce the
value the design promises*. There is no provider read involved at all
here — the value is a local DSL ResourceRef that simply never got
resolved. A per-provider patch would be nonsensical; the fix is in
carina-core ref resolution.

## The two-faced nature of a wait binding (the design crux)

A wait binding is **not** a value-producing declaration. Per the
wait-construct design lines 116–117:

- `cert_issued.arn` has the **same content** as `cert.arn` (the ARN is
  the ARN regardless of validation status).
- The **only** difference is the dependency edge: a downstream
  resource that references `cert_issued.*` must wait for the
  `until` predicate; one that references `cert.*` directly need not.

So the wait binding has two separable concerns:

1. **Value identity** — `cert_issued.X` *is* `cert.X`. This is what
   resolution must restore.
2. **Dependency edge** — referencing `cert_issued` adds an execution
   ordering constraint (the `Effect::Wait` must complete first). This
   is *already* handled, separately, by `create_plan`'s `Effect::Wait`
   lowering (`differ/plan.rs:379-429`) and the dependency graph.

The design must restore (1) **without** collapsing (2): the fix is in
the *value resolution* layer only; it must not remove or duplicate the
dependency-edge machinery that already exists. Resolving
`cert_issued.X` to `cert`'s value does not erase the `Effect::Wait` —
that effect is lowered from `parsed.wait_bindings` on a wholly separate
path and keyed by the wait's own binding/target. This separation is an
invariant the implementation must preserve and test.

## Chosen design

**Register each wait binding in `ResolvedBindings` as a passthrough
alias to its target binding**, threaded through the existing
`resolve_refs_*` entry points (plan and apply), and **typed** so the
"this is a wait alias, not a first-class value source" distinction is
carried in the type system rather than reconstructed by string lookup.

Three coordinated pieces:

### 1. `BindingNameKind::Wait { target: BindingName }` (type-level)

Add a `Wait` variant to `BindingNameKind` (`binding_index.rs:156`)
carrying the target binding name, and populate it in
`BindingNameSet::from_parsed` from `parsed.wait_bindings`.

- This mirrors the existing pattern exactly: every other declaration
  form is a typed `BindingNameKind` variant populated from its
  `parsed.*` source. Wait is currently the lone declaration form
  missing from that enum — adding it closes a gap, it is not a new
  mechanism.
- The variant **carries `target: BindingName`** (not a bare marker)
  so the wait→target relationship is a typed edge, recoverable without
  re-parsing or a parallel string map. `BindingName` is the existing
  newtype (carina#3066); reusing it keeps the alias edge type-safe end
  to end (no `String`→`String` alias dictionary that could point at a
  non-existent or wrong-kind target undetected).
- Precedence: insert wait **after `Resource`** in `from_parsed`'s
  most-specific-first order. A name is a wait binding or a resource
  binding, never both (the parser enforces distinct binding names), so
  ordering is not load-bearing for correctness here — but placing it
  adjacent to `Resource` documents that a wait binding is
  *addressable* like a resource binding, unlike `Structural` (whose
  doc-comment records the "not addressable by ResourceRef" invariant).

### 2. `BindingValueSource::WaitAlias { target }` (resolution-time)

`ResolvedBindings` stores, per name, a `ResolvedBinding { attributes,
source }` where the struct shape already "enforces every name has both
attributes and a source" (`binding_index.rs:311-318`).
`BindingValueSource` is already `#[non_exhaustive]` with a documented
intent to grow (`binding_index.rs:300-309`, "#2301 will introduce
structural sources"). Add a `WaitAlias { target: BindingName }`
variant.

`ResolvedBindings::from_resources_with_state` gains a fourth input,
`wait_bindings: &[WaitBinding]`. For each wait binding it inserts an
entry whose **attributes are the target's attributes** (resolved at
the same point the target's own entry is built) and whose **source is
`WaitAlias { target }`**. The alias is materialised at construction —
not via a resolve-time second lookup — so `resolve_ref_value` needs
**zero new branches**: it already does `bindings.get(name)` and walks
segments; a wait binding now simply *has* an entry, exactly like a
resource binding. (Rejected alternative D below explains why a
resolve-time indirection branch is worse.)

The `WaitAlias` source is retained (not flattened to `Local`) so the
two-faced nature stays observable: code that needs to know "did this
value come *through* a wait?" (diagnostics, future tooling, the
dependency-edge cross-check test) can ask `bindings.source(name)`
instead of losing the fact. This is the same rationale the existing
`Local` vs `Upstream` split already serves (`resolver.rs:194` keys
"missing key is a typo vs not-yet-known" off `source`).

### 3. Thread `wait_bindings` into the resolver entry points

`resolve_refs_for_plan` / `resolve_refs_with_state_and_remote` /
`resolve_refs_inner` (`carina-core/src/resolver.rs`) take an added
`wait_bindings: &[WaitBinding]` and forward it to
`ResolvedBindings::from_resources_with_state`. Call sites
(`wiring/mod.rs:1416`, `apply/mod.rs`, `fixture_plan.rs:140`,
`wiring/mod.rs:1797`) pass `&parsed.wait_bindings` — the same value
they already pass a few lines later to `create_plan`, so the data is
already in scope at every call site.

### Resolution trace for carina#3085

`r.distribution.acm_certificate_arn = r.cert_issued.certificate_arn`.
`from_resources_with_state` builds `cert`'s entry (resource binding,
attributes incl. resolved/state `certificate_arn`), then builds
`cert_issued` as a `WaitAlias { target: cert }` entry **pointing at
the same attribute map**. `resolve_ref_value` does
`bindings.get("cert_issued")` → found → `.get("certificate_arn")` →
the resolved ARN. Desired now equals state; the differ sees no change.
The `Effect::Wait` for `cert_issued` is still lowered independently by
`create_plan` from `parsed.wait_bindings` — the dependency edge is
intact.

## Alternatives considered

| Approach | Verdict |
|---|---|
| **A: typed `Wait` kind + `WaitAlias` source, alias materialised at `ResolvedBindings` construction (chosen)** | Restores the design's passthrough; one ranking of "what kind is this name" in the type system; zero new `resolve_ref_value` branches; dependency edge untouched; mirrors the existing per-declaration-form `BindingNameKind` + `BindingValueSource` patterns. |
| B: `String`→`String` wait-alias dictionary consulted in `resolve_ref_value` | **Rejected** — untyped parallel map; a wait whose target is missing/renamed/wrong-kind drifts silently (the split-source hazard carina#3080 was burned by). No compile-time or construction-time guard that the alias target exists. |
| C: rewrite `cert_issued.*` → `cert.*` in the AST before resolution (forward-ref rewrite) | **Rejected** — erases the wait binding from the resolved graph, so the dependency-edge concern (the *whole point* of `wait`) would have to be reconstructed elsewhere; couples value identity and dependency edge that the design deliberately separates (lines 116–117). |
| D: keep three inputs; add a resolve-time indirection branch in `resolve_ref_value` (if `bindings.get(name)` is None, look up wait target and retry) | **Rejected** — adds a second lookup path and a new branch to the hottest resolution function; the "materialise the alias at construction" approach makes a wait binding indistinguishable-by-handling from a resource binding (one code path), which is simpler and less error-prone long-term. |
| E: resolve in the provider read path | **Rejected** — there is no provider read here; the value is a local ResourceRef. Per-provider carve-out anti-pattern (carina#3073/#3080 layering). |

## Type safety

Load-bearing, per the project's standing guidance to prove invariants
in the type system rather than reconstruct them with runtime lookups.

1. **The wait→target edge is a typed value, not a string convention.**
   `BindingNameKind::Wait { target: BindingName }` and
   `BindingValueSource::WaitAlias { target: BindingName }` carry the
   target as the existing `BindingName` newtype (carina#3066). A wait
   alias cannot exist without naming its target; "wait binding with no
   target" is unrepresentable. Rejected alternative B's `String→String`
   map has no such guard — it can point anywhere, including a deleted
   or wrong-kind name, and only fail (silently, as a phantom) at diff
   time.

2. **`#[non_exhaustive]` is already the sanctioned growth path.**
   `BindingValueSource` is explicitly `#[non_exhaustive]` with a
   doc-comment anticipating new sources (#2301). Adding `WaitAlias`
   uses the mechanism the codebase already designed for this, and the
   compiler forces every `match` on `BindingValueSource` to consider
   it (no silent fallthrough). `BindingNameKind` is a plain enum;
   adding `Wait` makes every exhaustive match on it a compile error
   until updated — a desired tripwire, not a burden.

3. **No `unreachable!`/`panic!`, no runtime kind-guessing.** Selection
   of "is this name a wait binding" is the typed `BindingNameKind`
   lookup, not a heuristic. The `target`-missing case (a wait whose
   target binding does not exist) is *already* reported by
   `create_plan` as a `PlanError` (`differ/plan.rs:392-400`); this
   design does not add a second, divergent check — it reuses that
   single source of "target resolvable?" truth and, when the target is
   absent, simply does not create the alias entry (the ref stays
   unresolved and the existing `PlanError` fires loudly — no phantom,
   no panic).

4. **Value identity and dependency edge stay type-distinct.** The
   alias only ever contributes to `ResolvedBindings` (value layer).
   The `Effect::Wait` is lowered on a separate path from
   `parsed.wait_bindings` (effect layer). Nothing in this change lets
   the value layer suppress or duplicate the effect layer; an
   invariant test asserts the `Effect::Wait` still appears when a
   resource references `cert_issued.*` *and* the phantom is gone — the
   two concerns are verified together so a future change cannot fix one
   by breaking the other (the carina#3073 lesson: a downstream layer
   silently re-introducing the bug).

## Blast radius

- **Signature change:** `ResolvedBindings::from_resources_with_state`
  and the `resolve_refs_*` trio gain a `wait_bindings: &[WaitBinding]`
  parameter. ~4 call sites (`wiring/mod.rs` ×2, `apply/mod.rs`,
  `fixture_plan.rs`), each already has `parsed.wait_bindings` in scope.
- **Two enum variant additions:** `BindingNameKind::Wait`,
  `BindingValueSource::WaitAlias`. The latter's enum is
  `#[non_exhaustive]`; `match` sites within the crate must add an arm
  (compiler-enforced — a feature here, it forces every consumer to
  decide how a wait-sourced value behaves).
- **No comparator/renderer/provider/state-format change.** The differ,
  the wait-construct grammar, `Effect::Wait` lowering, and the
  dependency graph are all untouched. The fix makes resolution produce
  the value the rest of the pipeline already expects.
- **Behavioural surface:** any `<wait-binding>.<attr>` ResourceRef.
  Previously unresolved (phantom); now resolves to the target's value.
  A *genuine* downstream change (target attr actually differs) still
  shows — the value is resolved, then compared normally.
- **Risk:** a wait binding and its target sharing an attribute map
  must not let a downstream `cert_issued.*` write-back corrupt
  `cert.*`. `ResolvedBindings` entries are per-name `HashMap` clones
  built at construction (`from_resources_with_state` already clones
  into `merged`), so the alias entry is a *copy* of the target's
  attributes at build time, not a shared mutable reference — the
  `set()` write-back path (post-Create/Update) keys by name and does
  not alias. The implementation must add a test pinning that a
  `set("cert", ...)` does not mutate `cert_issued`'s entry and vice
  versa (independent copies, intentional — the wait alias is a
  read-time passthrough snapshot, matching the design's "snapshot of
  the target captured by the read() that satisfied until", line 118).

## Test plan

1. **Unit (`binding_index.rs`):**
   `ResolvedBindings::from_resources_with_state` with a wait binding →
   `get("cert_issued")` returns the target's attribute map;
   `source("cert_issued") == WaitAlias { target: cert }`. Negative:
   wait whose target binding is absent → no `cert_issued` entry (ref
   stays unresolved; the existing `PlanError` path, not a panic, is
   what surfaces it).
2. **Unit (`binding_index.rs`):** `BindingNameSet::from_parsed`
   registers a wait binding as `BindingNameKind::Wait { target }`;
   `contains("cert_issued")` is true (it is addressable, unlike
   `Structural`).
3. **Resolver (`resolver.rs`):** `resolve_ref_value` on
   `cert_issued.certificate_arn` against a `ResolvedBindings`
   containing the alias → resolves to the target's `certificate_arn`
   value (not the unchanged ResourceRef).
4. **Differ parity (the carina#3085 repro), real pipeline:** a
   directory fixture mirroring the infra registry shape (cert + wait +
   distribution across sibling `.crn` files) run through the real
   `resolve_refs_for_plan` → `create_plan`; assert (a) the
   `distribution.acm_certificate_arn` diff is `NoChange` (phantom
   gone), **and** (b) an `Effect::Wait` for `cert_issued` is still in
   the plan (dependency edge intact). Both asserted in one test so the
   two-faced invariant cannot regress half-way
   (`feedback_unit_test_path_is_not_apply_path`,
   `feedback_state_enum_phantom_diff_is_core_not_provider`).
5. **Write-back isolation:** `ResolvedBindings::set("cert", new_attrs,
   Local)` does not change `get("cert_issued")`, and vice versa
   (independent snapshots, per design line 118).
6. **Real-infra smoke (user-driven):** `carina plan` on the
   `carina-rs/infra` registry usecase that produced the issue output
   shows the `acm_certificate_arn` phantom gone. Per
   `feedback_no_real_infra_aws_commands` this is user-run, not
   initiated here; the directory fixture (item 4) is the
   CI-enforceable proxy.

## PR sequence (design-before-implementation)

1. **This design PR** (`notes/specs/…` only) — merges first.
2. **Implementation PR** — `BindingNameKind::Wait`,
   `BindingValueSource::WaitAlias`, the `wait_bindings` parameter
   threading, alias materialisation in `from_resources_with_state`,
   tests 1–5, `Closes #3085`.
3. No provider repo change (no provider involved).

## Risks / open questions (resolve in implementation)

- **`#[non_exhaustive]` match sites.** Enumerate every in-crate `match`
  on `BindingValueSource` (currently the `Local`/`Upstream` split at
  `resolver.rs:194` and tests) and decide each `WaitAlias` arm
  deliberately — most will treat it like `Local` (the value is a local
  passthrough), but the choice must be explicit per site, not a
  catch-all.
- **Upstream-sourced target.** If a wait's target is itself an
  upstream-state binding, the alias must mirror the target's *resolved*
  attributes regardless of the target's source. Confirm
  `from_resources_with_state` builds the target entry before the wait
  entry (insertion order) or resolves the alias against the
  already-built `by_name` map, so an upstream target's attributes are
  visible to the alias.
- **`depends_on` interaction.** A wait may declare `depends_on`
  bindings. That is an effect-layer ordering concern already handled by
  `Effect::Wait` lowering; confirm the value-layer alias change does
  not need to consider `depends_on` (it should not — value identity is
  independent of polling order). Document the conclusion.
- **Target rename / state-block moved.** If a `moved` state block
  renames the target, confirm the wait alias resolves against the
  post-rename binding (it should, since the alias is built from
  `parsed.wait_bindings`' `target` after forward-ref resolution; verify
  the ordering).
- **Multiple downstream consumers.** Several resources referencing
  `cert_issued.*` must all resolve and all inherit the single
  `Effect::Wait`; confirm the dependency graph already fans the one
  wait effect out to all consumers (it does via the existing dependency
  machinery — the value-layer change does not alter this, but the
  test plan item 4 fixture should include two consumers to lock it).
