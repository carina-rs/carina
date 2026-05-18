# Same-config read-iterable resolution (deferred-for B/C bridge): Design

<!-- derived-from #root-cause -->
<!-- constrained-by ../../CLAUDE.md -->
<!-- constrained-by ./2026-05-17-module-expansion-merge-surface-design.md -->

## Goal

Make a `for` loop whose iterable is a **same-config** provider-read /
computed attribute — `for _, opt in cert.domain_validation_options`
where `let cert = aws.acm.Certificate { … }` is in the same
configuration — **materialize into concrete resources at `carina
plan` / `apply`**, instead of staying deferred forever.

This is the **design document only**. Implementation follows in
separate PRs after this design merges, per the repo's split-PR policy
for large refactors (`CLAUDE.md` → "Design PR must merge before
implementation PR"). It is carina#3132, the explicitly-out-of-scope
"carina#3121 B/C bridge" axis the carina#3126 design named as a
Non-goal and open Risk; carina#3126 (module-boundary propagation +
instance-prefixing, merged in #3131/#3133) is the prerequisite.

## Root cause

<!-- constrained-by ../../CLAUDE.md -->

`ParsedFile::expand_deferred_for_expressions` (`carina-core/src/parser/ast.rs:839`)
resolves the iterable **only** via
`remote_bindings.get(iterable_binding).and_then(|a| a.get(iterable_attr))`
(ast.rs:854-856) and `continue`s when absent (ast.rs:858-860). At plan
time it is called at `carina-cli/src/commands/plan.rs:338` and at apply
time at `apply/mod.rs:795`. In both, `remote_bindings` is built
**exclusively** by `load_upstream_states(&parsed.upstream_states, …)`
(plan.rs:328 / apply/mod.rs:783) — cross-config `upstream_state` data
only.

A same-config `let cert` is not an `upstream_state`, so
`remote_bindings.get("cert")` is always `None` and the loop never
expands.

### The circular ordering (the crux)

The data the loop needs — `cert`'s live `domain_validation_options`
— first exists in `current_states: HashMap<ResourceId, State>` at
`wiring/mod.rs:1243`, populated by the provider refresh phase
**inside** `create_plan_from_parsed_with_upstream`, which plan.rs
calls at line 343 — **strictly after** the expansion at line 338.
But the refresh loop is built from
`sort_resources_by_dependencies(&parsed.resources)`
(`wiring/mod.rs:1145`, the *first* line of that function), and
`apply/mod.rs:793-794` documents a hard constraint: expansion **must
precede** `sort_resources_by_dependencies` so the loop's generated
children are in the sorted/refreshed/diffed set.

So: expansion needs post-refresh `current_states`; the refresh set
needs the expanded children. A genuine cycle. The escape hatch: the
**iterable's source binding (`cert`) is itself a normal top-level
`let` resource** present *before* expansion — it can be refreshed
independently of its loop children.

### The precedent that already works

The non-loop form
`let validation_record = aws.route53.RecordSet { name = cert.domain_validation_options[0].resource_record.name }`
**works today**. It is a normal `Resource` in `parsed.resources` with
a `Value::ResourceRef`; it resolves at `wiring/mod.rs:1438`
(`resolve_refs_for_plan`) / `apply/mod.rs:1011`
(`resolve_refs_with_state_and_remote`) → `resolver.rs:93`
`ResolvedBindings::from_resources_with_state`, which merges
`resource.resolved_attributes()` with the refreshed
`current_states[cert.id].attributes` (`binding_index.rs:444-451`) —
**after refresh**. The deferred-for cannot reuse it as-is only
because expansion runs *before* refresh and the template is not in
`parsed.resources` at that time. The loop and the working non-loop
ref are temporally disjoint resolutions of the *identical* value.

## Chosen approach: unify expansion into ref-resolution (design Shape 3)

<!-- derived-from #root-cause -->

The two pre-existing axes — `remote_bindings`-only / before-sort for
the deferred-for, vs `ResolvedBindings`-over-refreshed-state /
after-refresh for every non-loop ref — are the same temporal-split
defect the carina#3126 design closed for the `File<E>`/`ExpandedModule`
field lists. Per `CLAUDE.md` / [[feedback_root_cause_over_per_site_patch]]
(3+ same-class instances ⇒ fix the root primitive, not another
per-site carve-out) and the user's "long-term + type-safety"
directive: **a same-config read-iterable resolves at exactly the same
point, against exactly the same `ResolvedBindings` view, as the
working non-loop reference.** There is then *one* resolution timing
for "a same-config provider-read value feeding another resource",
whether the consumer is a `name = cert.dvo[0]…` ref or a
`for … in cert.dvo` loop.

This was chosen over the two narrower shapes precisely because they
*perpetuate* the split:

- **Shape 1 (split refresh)** double-reads the iterable source and
  bakes a new "pre-refresh targeted read" phase whose target set
  must be recomputed every time a new deferred-for kind appears —
  a per-site carve-out, the exact pattern the root-cause rule
  forbids.
- **Shape 2 (two-stage expand)** keeps the line-338 `remote_bindings`
  expansion *and* adds a post-refresh same-config expansion. Two
  permanent expansion timings is a new drift source — the carina#3126
  class recurring (a future maintainer adding a third iterable
  category must again decide "which of the two passes?").

Shape 3 eliminates the question by having one pass.

### Mechanism

1. **`expand_deferred_for_expressions` stops being a pre-refresh
   step.** Delete the `plan.rs:338` / `apply/mod.rs:795` calls.

2. **A single post-refresh expansion+resolution stage** inside
   `create_plan_from_parsed_with_upstream` (and the apply twin),
   placed where the non-loop ref already resolves
   (`wiring/mod.rs:~1438` / `apply/mod.rs:~1011`):
   - The iterable's source resources (`cert`) are normal top-level
     `let`s, already in `sort_resources_by_dependencies(&parsed.resources)`
     and already refreshed by the normal phase-1 pass into
     `current_states` — no special pre-refresh read, no double-read.
   - Build the same-config bindings view from the *same*
     `ResolvedBindings::from_resources_with_state(resources,
     current_states, remote_bindings, wait_aliases)` the non-loop
     resolver builds (`resolver.rs:93` / `binding_index.rs:432`).
     `ResolvedBindings { by_name: HashMap<String, ResolvedBinding> }`
     where `ResolvedBinding { attributes: HashMap<String, Value>,
     source }` (`binding_index.rs:410`), with an `iter() -> (&str,
     &ResolvedBinding)` accessor (`binding_index.rs:~313`).
     `from_resources_with_state` already does the binding↔refreshed-
     state merge (`binding_index.rs:444-451`) **and** the
     upstream-overwrite that keeps the `upstream_state` path working
     (`binding_index.rs:~459`). Projecting to the
     `HashMap<binding → HashMap<attr → Value>>` shape
     `expand_deferred_for_expressions` consumes is a trivial
     `.iter().map(|(k, rb)| (k.to_string(), rb.attributes.clone()))`
     — **no new join logic**; the merge is reused verbatim. (Note:
     `ResolvedBindings` is distinct from the validation-time
     `BindingIndex`/`BindingEntry` (resource+schema) — this design
     uses the runtime value map, not the schema index.)
   - Expand the deferred-for templates against that unified view,
     append the generated resources, **re-run
     `sort_resources_by_dependencies`** over the augmented set, then
     run `resolve_refs_*` so the loop bodies' own refs (the
     instance-prefixed `opt.resource_record.name` etc. from
     carina#3126 PR-B) resolve in the same pass as every other ref.

3. **Type-safety lever.** Today `expand_deferred_for_expressions`
   takes a bare `&HashMap<String, HashMap<String, Value>>` with the
   implicit, undocumented contract "this is upstream-only". Replace
   the parameter with a small typed view — e.g.
   `IterableBindings<'a>` wrapping the merged `ResolvedBindings`
   projection — so the function's input *names* "every binding an
   iterable may reference (upstream + same-config refreshed)", and a
   caller cannot pass the upstream-only map again by mistake (the
   bug class that produced #3132). The exhaustive-destructure /
   single-source-of-truth discipline from the carina#3126 merge
   surface applies: one constructor for that view, fed by the one
   `ResolvedBindings` the resolver already builds.

### Why this is the long-term, type-safe shape

- **One resolution timing.** After this, "a same-config
  provider-read value consumed elsewhere" has a single point of
  truth (`ResolvedBindings` post-refresh) regardless of whether the
  consumer is a scalar ref or a loop iterable. The temporal split
  that *is* #3132 cannot recur because the early pass is deleted, not
  duplicated.
- **No new join / no double-read.** Reuses
  `ResolvedBindings::from_resources_with_state` verbatim; the cert is
  refreshed by the existing phase-1 pass.
- **Typed input closes the bug class.** The
  `&HashMap<…>`→typed-view change makes "iterable resolved against
  the wrong (upstream-only) map" unrepresentable, the same way
  carina#3126's exhaustive destructure made "field dropped at the
  module boundary" a compile error.

## Non-goals

- Changing the deferred-for *parse* or carina#3126's
  module-boundary propagation / instance-prefixing (merged; this
  builds on it — the loop body arrives correctly prefixed, it just
  needs to *materialize*).
- New DSL surface. The `.crn` is unchanged; this is pipeline
  ordering + the resolution-view type.
- Resolving an iterable that is genuinely unknowable at plan time
  (e.g. depends on a not-yet-created resource with no readable
  state). Such loops legitimately stay deferred and surface via the
  carina#3128 validate placeholder — the existing behavior for a
  truly unresolvable iterable is correct and must be preserved.
- The `upstream_state` iterable path. It already works; this design
  must not regress it — it folds into the same unified view, not a
  separate code path.

## Implementation PR breakdown (post-merge, strict order)

1. **PR-1 — introduce the typed iterable-bindings view + move
   expansion after refresh (plan path).** Delete the `plan.rs:338`
   call; add the post-refresh expansion+re-sort+resolve stage in
   `create_plan_from_parsed_with_upstream` fed by the projected
   `ResolvedBindings`. `upstream_state` loops must still expand
   (now via the unified view). Acceptance: existing deferred-for
   tests stay green; a new fixture with a same-config
   `let cert`-read iterable produces concrete resources in the plan.
2. **PR-2 — apply path parity.** The `apply/mod.rs` twin (delete
   line 795, add the post-refresh stage before `resolve_refs_*`),
   honoring the `apply/mod.rs:793-794` sort constraint via the
   re-sort. Plan/apply must not diverge
   ([[feedback_unit_test_path_is_not_apply_path]]).
3. **PR-3 — real-infra acceptance + `moved.crn` removal.** Build the
   binary, run `carina plan` against
   `carina-rs/infra registry/dev/registry`; assert the concrete
   instance-prefixed `aws.route53.RecordSet` per
   `domain_validation_options` entry; remove the now-unnecessary
   `moved.crn`. User-driven per [[feedback_no_real_infra_aws_commands]];
   the named acceptance condition of #3132.

`Closes #3132` on PR-3 only (PR-1/PR-2 `refs #3132`).

## Risks / open questions

- **Re-sort + second diff pass.** Appending loop children after the
  initial `sort_resources_by_dependencies` requires a topological
  re-sort and that the diff/plan consume the augmented set. PR-1
  must prove the re-sort is stable (no reordering of already-planned
  resources that would perturb SimHash / identifier reconcile /
  `moved` matching). This is the highest-risk part and is why PR-1
  is plan-only with the existing-tests-green gate before PR-2.
- **An iterable referencing another loop's output.** `for x in
  a.attr` where `a` is itself produced by a deferred-for. Out of
  scope; must stay deferred (not silently mis-expand) and surface
  via the validate placeholder. PR-1 states this as a known
  limitation and asserts the deferred entry survives rather than
  mis-resolving.
- **`--refresh=false`.** With refresh disabled, `current_states`
  comes from the cached state file
  (`wiring/mod.rs:1359-1364`). A same-config read iterable then
  resolves against cached `domain_validation_options` if present,
  else stays deferred (correct — no live data, same as the non-loop
  ref's behavior under `--refresh=false`). PR-1 must add a fixture
  for this so the behavior is pinned, not incidental.
- **`expand_deferred_for_expressions` is also called from
  carina-core tests** (parser/tests.rs) with hand-built
  `remote_bindings`. The typed-view change is a signature change;
  PR-1 must thread it through those call sites (the carina#3126
  precedent: a generic-but-mechanical signature migration).
- **Plan display of still-deferred entries.** `plan.rs:417` passes
  the residual unexpanded `deferred_for_expressions` to `print_plan`.
  Moving expansion later must keep that residual list correct for
  genuinely-unresolvable loops (the carina#3128 placeholder must
  still render). PR-1 verifies the validate/plan placeholder for an
  unresolvable iterable is unchanged.
- **Not a phantom/renderer change.** This is pipeline ordering +
  resolution typing; no detail-row/diff renderer is touched. Stated
  to scope reviewers away from the phantom-diff class.
