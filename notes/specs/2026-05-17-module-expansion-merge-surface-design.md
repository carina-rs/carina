# Module-expansion merge surface: Design

<!-- derived-from #root-cause -->
<!-- constrained-by ../../CLAUDE.md -->

## Goal

Make it **structurally impossible** for a `ParsedFile` field that
survives the sibling-`.crn`-file merge to be silently dropped at the
import-module-expansion boundary. carina#3126 (`deferred_for_expressions`
dropped) and the already-shipped carina#3061 (`wait_bindings` was
dropped) are the *same* defect recurring; this design removes the
class, not the instance.

This is the **design document only**. Implementation follows in
separate PRs after this design merges, per the repo's split-PR policy
for large refactors (`CLAUDE.md` → "Design PR must merge before
implementation PR").

## Root cause

<!-- constrained-by ../../CLAUDE.md -->

carina#3126: a `for` loop over an unresolved iterable
(`for _, opt in cert.domain_validation_options { … }`) declared
**inside an imported module directory** (e.g.
`carina-rs/infra usecases/registry/acm.crn`, consumed by
`registry/dev/registry/main.crn` via `let r = registry { use { … } }`)
never reaches the caller. It is invisible to `validate`, `plan`, and
`apply` — the loop body is silently never managed. The existing
`moved.crn` in that stack is a hand-written state-migration workaround
for exactly this limitation.

### Two merge paths, one drifting field list

There are two paths that take content from a parsed `.crn` and fold
it into a caller's `ParsedFile`. They do not share a field list:

1. **Sibling-file merge** —
   `config_loader.rs::merge_parsed_file`
   (`pub(crate) fn merge_parsed_file(target: &mut ParsedFile, source:
   ParsedFile)` — note: **pinned to `ParsedFile`, not generic
   `File<E>`**; ~7 caller sites in config_loader / loader / resolver).
   The complete reference: merges **all 17** `File<E>` content fields
   — 15 collection `.extend()`, `structural_bindings` set-extend,
   `backend` last-wins. Nothing is dropped.

2. **Import-module expansion** — `ExpandedModule`
   (`module_resolver/expander.rs`) carries **2** fields
   (`resources`, `wait_bindings`); the two resolver merge sites
   (`module_resolver/resolver.rs` ~`:200` nested, ~`:249` top-level)
   `.extend()` only those two. `expand_module_call` reads
   `module.resources` and `module.wait_bindings` from the module's
   already-loaded `ParsedFile` (which *does* contain
   `deferred_for_expressions` — the loss is at expansion, not load)
   and writes only those two into `ExpandedModule`.

There is **no type, trait, or helper** connecting the two field
lists. Adding a `ParsedFile` field updates the struct literal in
`merge_parsed_file` (or fails to compile there) but **silently does
not** flow through module expansion. That is precisely the failure
mode, and it is structurally identical to carina#3061: `wait_bindings`
was the previously-missing field, fixed by adding it to
`ExpandedModule` + a `prefix_wait_binding` helper + both resolver
`.extend()` sites — a per-field bandaid. carina#3126 is the next field
in the same lineage. A third will follow unless the *class* is closed.

### Why a bandaid is wrong here

The issue's "Suggested fix" (add `deferred_for_expressions` to
`ExpandedModule`, add a prefix helper, `.extend()` at both sites) is
exactly the carina#3061 shape repeated. It fixes carina#3126 and
leaves the drift mechanism intact. `CLAUDE.md` /
[[feedback_root_cause_over_per_site_patch]]: 2+ same-class instances
(#3061, #3126) means fix the root primitive, not another per-site
patch. The user explicitly requested the root fix.

## Non-goals

- Propagating fields that module expansion **intentionally** does not
  surface as collections: `variables`, `user_functions` (module-local,
  inlined during expansion), `export_params` / `attribute_params`
  (surfaced via the synthesized virtual attribute resource),
  `arguments` / `requires` / `module_calls` / `uses` (consumed
  *inside* `expand_module_call` / nested resolution), `providers`
  (modules inherit caller providers by design), `state_blocks` /
  `structural_bindings` / `warnings` / `backend`. The design must let
  these be *explicitly classified as not-propagated*, not silently
  dropped — see "Chosen approach".
- carina#3121 fixes B/C (plan/apply same-config read-iterable bridge).
  Independent axis; this design is the prerequisite for the real
  `carina-rs/infra` registry case but does not itself implement B/C.
- The `upstream_states` coupling (a module-internal deferred-for whose
  iterable is a module-internal `upstream_state`) is **in scope to
  classify** but its propagation semantics are an open question — see
  "Risks / open questions".

## Chosen approach

### Principle: one classified merge surface, compile-forced completeness

Replace the two divergent ad-hoc field lists with **one place that
classifies every `File<E>` content field**, and route *both* the
sibling-file merge and the module-expansion merge through it. The
classification must be **exhaustive-destructure-guarded** so a new
`ParsedFile` field cannot compile until someone explicitly classifies
it for *both* paths — the same compile-time forcing function
`prefix_wait_binding` already uses for `WaitBinding`
(`expander.rs` doc: "if a future field is added, this stops compiling
until someone decides…", the carina#3061 guard).

### Shape: `ExpandedModule` becomes a `ParsedFile`-shaped contribution

The robust shape (the one the investigation recommends and that
matches the existing single-spelling doctrine of `apply_instance_prefix`):

1. **`ExpandedModule` is replaced by a `ParsedFile`-shaped delta.**
   `expand_module_call` produces a `File<E>` (or a thin newtype
   wrapping one) representing the module's contribution, already
   instance-prefixed. The two resolver sites stop hand-listing
   `.resources` / `.wait_bindings` and instead call the **same merge
   function** the sibling path uses (`merge_parsed_file`, generalized
   if needed). Result: adding a `ParsedFile` field automatically
   flows through module expansion via the shared merge — it cannot be
   dropped because there is only one merge.

2. **A single `prefix_module_contribution(&mut File<E>, instance_prefix)`**
   function instance-prefixes the contribution. It **destructures
   `File<E>` exhaustively** (every field bound by name, no `..`), so a
   new field forces a compile error until classified as one of:
   - *binding-prefix-sensitive* → routed through `apply_instance_prefix`
     (the single spelling) / the resource-prefix path,
   - *value/provenance* → passed through unchanged,
   - *not-propagated-from-modules* → explicitly dropped **with a
     comment stating why**, not silently.
   This subsumes `prefix_wait_binding` (it becomes one arm) and the
   per-resource prefixing already in `expand_module_call`.

3. **`merge_parsed_file` keeps its exhaustive struct-literal-free
   form** but gains the same guard: a `#[deny]`-style exhaustiveness
   (destructure the source `File<E>` so an unmerged field fails to
   compile). Today it is a hand-maintained `.extend()` list with no
   compiler check that it is complete; the design adds that check so
   the *sibling* path also can't silently drop a future field.

The net invariant: **every `File<E>` field is classified exactly once,
in one place, for propagation + prefixing; both merge paths consume
that classification; the compiler rejects an unclassified field.**

### `DeferredForExpression` prefixing (the carina#3126 payload)

When the contribution is prefixed, `DeferredForExpression` is
destructured exhaustively. Per-field classification (verified against
`ast.rs:19-44` and the `prefix_wait_binding` model):

| Field | Treatment |
| --- | --- |
| `binding_name` | **prefix** — string-level `apply_instance_prefix` (generated-resource binding prefix; same reason `Resource.binding` is prefixed) |
| `iterable_binding` | **prefix** — string-level `apply_instance_prefix` (mirrors `prefix_wait_binding`'s `lhs_segments[0]` head) |
| `attributes` | **rewrite** — `rewrite_intra_module_refs` + `substitute_arguments` + `canonicalize_in_place`, exactly as `module.resources` attributes |
| `template_resource` | **full resource treatment** — `id.name`/`.binding` prefix + ref-rewrite + arg-substitute + canonicalize, identical to a `module.resources` entry |
| `file`, `line` | pass through (provenance) |
| `header` | pass through (verbatim user-surface display text, like `WaitBinding.until_raw`) |
| `resource_type`, `iterable_attr` | pass through (not bindings) |
| `binding` (`ForBinding`) | pass through (loop-local var pattern, not a module binding) |

This reuses the machinery already applied to `module.resources`
(`intra_module_bindings`, `rewrite_intra_module_refs`,
`substitute_arguments`, `canonicalize_in_place`) — no new ref-rewrite
logic, just routed to one more carrier.

## Implementation PR breakdown (post-merge, strict order)

1. **PR-A — introduce the single merge surface (no behavior change).**
   Generalize `merge_parsed_file` to be exhaustive-destructure-guarded;
   make `expand_module_call` emit a `File<E>`-shaped contribution and
   the two resolver sites consume the shared merge. `wait_bindings`
   continues to work (now via the unified path, `prefix_wait_binding`
   folded into `prefix_module_contribution`). **`deferred_for_expressions`
   still not prefixed yet** — but now its absence is a *compile error*
   in `prefix_module_contribution`, forcing PR-B. Pure refactor;
   `cargo nextest` + the existing module_resolver suite must stay
   green; no `.crn`-observable change.
2. **PR-B — classify + prefix `deferred_for_expressions` (the
   carina#3126 fix).** Fill the compile-forced arm with the table
   above. Acceptance: a multi-file fixture where the deferred-for
   lives in an **imported module directory** (mirroring
   `usecases/registry/acm.crn` consumed by a wrapper) shows the loop
   body in `carina validate` (now correct because carina#3128 routed
   validate display through `iter_all_resources()` — PR-B depends on
   #3128, already merged) and expands it in `plan` with
   instance-prefixed `binding_name`/`iterable_binding`/`template_resource`.
   Real-infra smoke: build the binary, run `carina validate` /
   `carina plan` against `carina-rs/infra registry/dev/registry`
   (user-driven per [[feedback_no_real_infra_aws_commands]]) — the
   named acceptance condition.

`Closes #3126` on PR-B only (PR-A `refs #3126`).

## Risks / open questions

- **`upstream_states` coupling.** A module-internal deferred-for's
  iterable is typically a module-internal `upstream_state`.
  Propagating the deferred-for without its `upstream_state` may leave
  it unresolvable at plan time. The single classified surface forces a
  decision (the field can't be silently skipped), but *what* the
  correct semantics are (propagate prefixed? resolve inside the
  module? error?) is genuinely open. PR-A surfaces it as a
  compile-forced classification; the answer is decided in PR-B with a
  fixture proving the chosen behavior — do not guess silently.
- **`merge_parsed_file` is pinned to `ParsedFile`, not generic, and
  has ~7 callers.** Its signature is
  `(target: &mut ParsedFile, source: ParsedFile)`
  (callers in config_loader / loader / resolver). Module expansion
  operates on `File<E>` generically (the carina#3128 generic-helper
  precedent / [[feedback_directory_scoped_features]] loader-phase
  path). Making it the *shared* surface therefore requires
  **generalizing its signature to `File<E>`** — a real PR-A task and
  a behavior-preservation risk: all callers must keep byte-identical
  sibling-merge output. PR-A must assert byte-identical parse output
  on existing multi-file fixtures (the directory-scoped acceptance
  rule) before the structural change is considered safe.
- **Cost of exhaustive destructure on `File<E>` (17 fields).** Verbose
  but intentional — the verbosity *is* the guard. Mitigate with a
  single well-commented destructure, not scattered field access.
- **Not a phantom/plan-display change.** This is a parse/merge
  structural fix; no detail-row/renderer surface is touched. Stated to
  scope reviewers away from the phantom-diff class.

## Why this is the long-term, type-safe shape

- **The drift mechanism is removed, not patched.** After PR-A there is
  one classified merge surface; carina#3061-class bugs become compile
  errors, not runtime data loss. A hypothetical carina#3127-equivalent
  ("field N also dropped at module boundary") cannot reach `main` —
  it won't compile.
- **No new ref-rewrite logic.** Reuses `apply_instance_prefix` (the
  single spelling), the existing resource-prefix path, and the
  exhaustive-destructure forcing function already proven by
  `prefix_wait_binding`.
- **Split PRs keep blast radius reviewable.** PR-A is a behavior-
  preserving refactor with a green-suite gate; PR-B is the small
  classified payload with a real-infra acceptance. Neither mixes the
  structural change with the user-visible fix.
