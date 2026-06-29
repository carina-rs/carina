# Bindings: unified pre-apply `ResolvedBindings` constructor

<!-- constrained-by ./2026-05-21-resource-kind-typestate-design.md -->

Refs #3246. Closes #3248.

## Background

`ResolvedBindings` is the view that reference resolution consults to
turn a `ResourceRef { binding: "X", attribute: "attr", ... }` into a
concrete `Value`. It is a map `binding_name → attributes` whose
attribute maps may themselves contain unresolved `ResourceRef`s — the
recursive resolver chains through.

The current public API is asymmetric:

- `ResolvedBindings::from_managed_with_state(managed, current_states, remote_bindings, wait_aliases)`
  builds the view from the **managed** resource slice only.
- `ResolvedBindings::add_virtual_resources(&[VirtualResource]) -> Result<(), String>`
  layers virtuals **on top** as a separate, opt-in step.
- `ResolvedBindings::add_data_sources(&[DataSource])` likewise layers
  data sources as a separate, opt-in step.

This shape was introduced by #3176 / #3181 ("resource kind typestate
split") and is correct for the **post-apply** path: state writeback
re-resolves virtuals against post-apply state (`resolve_virtual_refs_post_apply`)
*then* layers them, so the increments must be explicit and ordered
(`carina-cli/src/commands/shared/state_writeback.rs:210-230`).

It is **incorrect** for the **pre-apply** path. From the perspective of
reference resolution, `let X = ...` is a name → attribute mapping
regardless of whether X is Managed, Virtual, or DataSource — a sibling
attribute reading `X.attr` does not care which kind X is. The
typestate split's separation reflects a different axis (Effect
generation operates on Managed only, virtuals carry no provider-side
state, data sources only produce `Effect::Read`); collapsing that axis
into "the bindings view sees Managed by default" is a category error.

The asymmetry has produced a recurring class of bug: every pre-apply
binding-construction site that forgets to layer virtuals / data
sources produces unresolved references at the differ, which then
reports spurious changes against the literals already in state. The
headline instance is #3246: post-#3243, a managed-resource attribute
referencing `<module_instance>.<attr>` (a virtual-rooted ref)
survives plan-path resolution and the differ flags `must be replaced`
against the state's literal. Review of a prototype symptom-level fix
(threading `virtuals: &[VirtualResource]` into every pre-apply
resolver entry point and re-calling `add_virtual_resources` inside)
identified at least three more pre-apply sites with the same blind
spot:

- `carina-cli/src/wiring/mod.rs:1273` — `expand_same_config_deferred_for`
  builds an `iterable_bindings` view from the managed slice only; a
  `for ... in module_instance.attr` would have the same gap.
- `carina-cli/src/commands/apply/mod.rs:1670` — saved-plan apply
  (`apply --plan`) builds bindings from the managed slice only and
  explicitly passes `&[]` virtuals into `execute_effects`, with the
  comment "Saved plan files do not persist virtual resources".
- `carina-core/src/resolver.rs:resolve_data_source_refs` — data-source
  input refs go through their own resolver that builds bindings from
  the managed slice only with no virtuals layer.

The prototype patch makes every callsite re-layer virtuals manually.
That keeps the asymmetry in the API, treats every new pre-apply call
site as needing a careful audit, and silently regresses if anyone
adds a new pre-apply binding-construction site that forgets the
layering call. The fix is structural: make "include virtuals + data
sources" the only way to build a pre-apply `ResolvedBindings`, and
make forgetting them a compile error.

## Goals

1. **Type-level guarantee:** "forgot to include virtuals on the
   pre-apply path" is a compile error, not a runtime symptom.
2. **Preserve the typestate split where it is real:** Effect generation,
   state writeback, destroy ordering, and the executor's typed slice
   inputs all continue to distinguish Managed / Virtual / DataSource.
   None of those change.
3. **Make the resolver a pure transform:** `resolve_refs_for_plan` /
   `resolve_refs_with_state_and_remote` consume a pre-built
   `ResolvedBindings`. Binding construction is the caller's
   responsibility — concentrated in one place per pre-apply path.
4. **Close the headline bug (#3246) structurally:** no managed-resource
   attribute that references a virtual binding can survive plan-path
   resolution as an unresolved ref.

## Non-goals

- Changing what the executor / differ / state writeback consider
  Managed vs Virtual vs DataSource. The typestate split there stays.
- Surfacing a user-facing diagnostic that flags `module_instance.attr`
  references as suspect. Module attributes are first-class published
  API for a module instance; `bootstrap.role_name` is the intended
  way to consume them. The earlier "wait / upstream-state passthrough"
  guidance in `resolver.rs:138-148` refers to a different scenario
  (cross-stack or asynchronous completion-bound consumption); it is
  not a substitute for same-stack module attribute references and
  the doc-comment will be amended to say so.
- A trait-generic resolver (`trait BindingProvider` over Managed /
  Virtual / DataSource). That is a further refactor; the typed-slice
  shape proposed here is the minimum needed to fix the headline
  class.
- Cross-repo coordination (`carina-provider-aws`, `carina-provider-awscc`).
  Provider repos consume `carina-core` only through the `Provider`
  trait and the `*Resource` types — they do not call `ResolvedBindings`
  constructors. A `gh search code` over `carina-rs/*` confirms zero
  hits for `from_managed_with_state` / `from_resources_with_state`
  outside this repo. The implementation PR ships as a
  carina-only change with no coordinated provider-repo bump.

## Proposed shape

### `ResolvedBindings::pre_apply` — single typed pre-apply constructor

The constructor takes a `PreApplyInputs` struct with named fields
rather than 6 positional arguments. The struct is required-fields-only
(no `Option`, no `Default`) so "forgot virtuals" is still a compile
error; the struct shape is purely for call-site readability when a
caller has typed `ParsedFile` slices in hand. (Builder pattern is
unnecessary — all fields are mandatory by design.)

```rust
/// Required inputs for `ResolvedBindings::pre_apply`. Every field is
/// mandatory; the struct exists only to give the 6 inputs named slots
/// at call sites (`apply/mod.rs:1097`, `plan.rs:644`,
/// `wiring/mod.rs:1762`). Forgetting any kind is a compile error
/// because the struct has no `Default` and no `Option` fields
/// (carina#3246).
pub struct PreApplyInputs<'a> {
    pub managed: &'a [ManagedResource],
    pub virtuals: &'a [VirtualResource],
    pub data_sources: &'a [DataSource],
    pub current_states: &'a HashMap<ResourceId, State>,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub wait_aliases: &'a [WaitAliasSpec],
}

impl ResolvedBindings {
    /// Pre-apply constructor. Takes every kind of binding that
    /// reference resolution can name. Forgetting virtuals or data
    /// sources at a new call site is a compile error rather than a
    /// runtime divergence (carina#3246).
    ///
    /// The `pre_apply` name breaks the existing `from_<source>_with_state`
    /// convention deliberately: it describes the **lifecycle phase**,
    /// pairing visibly with the kept post-apply increment APIs
    /// (`layer_virtuals_post_apply` / `layer_data_sources_post_apply` —
    /// see below). At a glance "pre_apply vs post_apply" tells the
    /// reader which side of the apply boundary the call belongs to,
    /// which is the central distinction the redesign establishes.
    ///
    /// Order of layering (matches the post-apply layering in
    /// `state_writeback.rs:210-230`, so same-name collisions resolve
    /// consistently): managed first (merged with `current_states`),
    /// then data sources, then virtuals, then wait aliases.
    /// (Same-stack collisions are rejected by the parser's
    /// `DuplicateBinding` check, so the order is observable only in
    /// test code that constructs colliding inputs by hand.)
    pub fn pre_apply(inputs: PreApplyInputs<'_>) -> Self { ... }
}
```

### Resolver entry points become pure transforms

`resolve_refs_for_plan` and `resolve_refs_with_state_and_remote` no
longer build the bindings view internally. They accept a pre-built
`ResolvedBindings` and the resources slice:

```rust
pub fn resolve_refs_for_plan(
    resources: &mut [ManagedResource],
    bindings: &ResolvedBindings,
) -> Result<(), String> { ... }

pub fn resolve_refs_with_state_and_remote(
    resources: &mut [ManagedResource],
    bindings: &ResolvedBindings,
) -> Result<(), String> { ... }
```

The two functions stay separate (rather than collapsing into a
single function with a `bool`/enum mode flag). The plan-variant's
extra behaviour — stamping any surviving upstream `ResourceRef` as
`Value::Unknown(UpstreamRef)` for display — is encoded in the function
name, which keeps call sites grep-able and avoids the
`resolve_refs_for_plan(&mut rs, &b, true)` smell where the trailing
boolean is opaque without jumping to the signature. The shared
implementation lives in a private `resolve_refs_inner` helper as
today; only the public entry-point split changes.

This is the carina#3175 / #3176 design direction made concrete: the
resolver is no longer responsible for choosing which binding sources
to consult. The caller — which has the typed `ParsedFile` slices in
hand — assembles the view once and reuses it for every resolver call
on the same plan/apply path.

### `add_virtual_resources` / `add_data_sources` — renamed and scope-narrowed

These remain, but are **renamed** to make the post-apply scope visible
at every call site:

- `add_virtual_resources` → `layer_virtuals_post_apply`
- `add_data_sources` → `layer_data_sources_post_apply`

The doc-comments are rewritten to name the `state_writeback.rs:210-230`
use case explicitly (post-apply re-resolution followed by layering on
top of a pre-apply view) and to point new readers at `pre_apply` for
the pre-apply case. The rename pairs visibly with `pre_apply` so the
phase distinction is obvious in any grep / code-review context. New
pre-apply call sites cannot land on the layer helpers by accident
because the names now say "post_apply".

### `from_managed_with_state` / `from_resources_with_state` deprecation

`from_resources_with_state` (the pre-#3176 legacy mixed-slice
constructor) and `from_managed_with_state` (the #3176 typed wrapper)
are deprecated for pre-apply use. The implementation PR migrates
every pre-apply call site to `pre_apply`, then removes both legacy
entries. Test code that hand-constructs a minimal view continues to
use `pre_apply` with empty slices for the kinds it does not need —
the convenience is "default-empty slices" via a constructor pattern,
not a separate API.

## Migration table

Every existing pre-apply call site, classified by category and the
migration target. Two row classes appear: **constructor sites** that
build a `ResolvedBindings` today (the `from_*_with_state` rows) and
**resolver-entry sites** that today pass raw input slices into a
resolver that builds the bindings internally (`resolve_refs_for_plan`,
`resolve_refs_with_state_and_remote`). After migration both classes
construct a `ResolvedBindings` via `pre_apply` and pass it explicitly
into the resolver — the resolver-entry sites *become* constructor
sites.

| Site | Category | Current call | After migration |
| ---- | -------- | ------------ | --------------- |
| `carina-core/src/resolver.rs:93` (`resolve_refs_inner`, plan path) | Pre-apply | `from_resources_with_state(managed, ...)` | Caller passes pre-built `ResolvedBindings` |
| `carina-core/src/resolver.rs:227` (`resolve_data_source_refs_inner`) | Pre-apply | `from_resources_with_state(managed, ...)` | Caller passes pre-built `ResolvedBindings` |
| `carina-cli/src/wiring/mod.rs:1273` (`expand_same_config_deferred_for`) | Pre-apply | `from_resources_with_state(managed, ...).project_iterable_bindings()` | `pre_apply(managed, virtuals, data_sources, ...).project_iterable_bindings()` |
| `carina-cli/src/wiring/mod.rs:1762` (`create_plan_from_parsed_with_upstream`) | Pre-apply | `resolve_refs_for_plan(managed, ...)` | Build `pre_apply` once, pass into `resolve_refs_for_plan` |
| `carina-cli/src/commands/apply/mod.rs:1097` (live apply, head-of-pipeline) | Pre-apply | `from_resources_with_state(managed, ...)` | `pre_apply(managed, virtuals, data_sources, ...)` |
| `carina-cli/src/commands/apply/mod.rs:1670` (`run_apply_from_plan`, saved-plan apply) | Pre-apply | `from_resources_with_state(managed, ...)` + `&[]` virtuals into executor | **See "Saved-plan path" below.** |
| `carina-cli/src/commands/plan.rs:644` (`resolve_export_values_for_display`) | Pre-apply | `from_resources_with_state(managed, ...)` | `pre_apply(managed, virtuals, data_sources, ...)` |
| `carina-cli/src/fixture_plan.rs:159` (fixture-based plan) | Pre-apply | `resolve_refs_for_plan(managed, ...)` | Same as wiring/mod.rs:1762 |
| `carina-cli/tests/wait_apply_module_path.rs:384` (e2e test) | Pre-apply | `resolve_refs_with_state_and_remote(managed, ...)` | Update to typed slice constructor |
| `carina-cli/src/commands/shared/state_writeback.rs:210` (export resolution) | Post-apply | `from_managed_with_state(managed, ...)` + `add_data_sources` + `add_virtual_resources` (correct increment layering) | **Unchanged.** This is the post-apply layering case the increment API is for. |

## Saved-plan path (`apply --plan`)

The saved-plan apply path needs an explicit design decision. Currently
`PlanFile` (`carina-cli/src/commands/plan.rs:35-86`) persists
`sorted_resources` (managed), `current_states`, `upstream_snapshot`,
and `wait_bindings`, but **not** `virtual_resources`. The
`run_apply_from_plan` body at `apply/mod.rs:1698-1700` documents
this:

```rust
// Saved plan files do not persist virtual resources; the
// `apply --plan` path has no post-expansion virtual slice.
&[],
```

Today this is benign only because the plan file's `sorted_resources`
have already gone through `resolve_refs_for_plan` once (at plan time)
and the resolved literals are baked in — *if* virtuals resolved
correctly at plan time. Post-#3243 they don't, so the literals are
not baked in, and the saved-plan path runs the executor with
attributes still holding `ResourceRef`s into a binding map that has
no entry for them. That fail-fasts inside `executor::basic::resolve_resource`
(`carina-core/src/executor/basic.rs:125-137`) via
`assert_fully_resolved` rather than producing a spurious diff, but
the saved-plan path is broken for the same shape regardless.

Two viable options. Both are within the scope of this design issue
because the unified `pre_apply` constructor cannot be applied to
`apply/mod.rs:1670` without picking one.

### Option A: persist `virtual_resources` in `PlanFile`

Add `virtual_resources: Vec<VirtualResource>` to `PlanFile`. Bump
`PlanFile::version` (currently `3`, bump to `4`). The
saved-plan apply path deserializes and passes them through `pre_apply`
identically to the live-apply path.

**Pro:** symmetry with the live-apply path. The saved-plan apply path
ends up structurally identical to the live-apply path. No re-derivation
logic.

**Con:** plan file size grows (modest — virtuals are small). Older
plan files are unreadable after the version bump.

### Option B: re-derive virtuals from the saved managed slice + module info

Persist enough to reconstruct virtuals at apply time — most cheaply,
also persist the `module_calls: Vec<ModuleCall>` and `uses: Vec<UseStatement>`
that produced the virtuals. At apply time, re-run module expansion
(`module_resolver::resolve_modules_with_config`) to regenerate the
virtual slice.

**Pro:** smaller plan file (no virtual rows). Re-expansion is the
canonical source of truth.

**Con:** re-running module expansion at apply time means the saved
plan is no longer a frozen snapshot — a module file edited between
plan and apply changes the apply behaviour. The whole point of saved
plans is to lock the apply input.

### Decision

**Choose Option A** — persist `virtual_resources` in `PlanFile`. This
matches the design intent of saved plans ("apply exactly what the plan
showed") and makes the saved-plan apply path structurally identical
to the live-apply path (one less divergence to audit). The plan file
size cost is small and bounded.

The "frozen snapshot" argument against Option B is stronger than just
preference. Saved-plan apply already enforces a frozen-state invariant
via `state_lineage` / `state_serial` drift detection
(`carina-cli/src/commands/plan.rs` `StateSnapshot`, CLAUDE.md "Plan
Concurrency Contract"). Re-running module expansion at apply time
under Option B would introduce a **second** drift vector — module
source files edited between plan and apply — that the existing drift
machinery does not cover. Closing that would require either
fingerprinting every module-source file or accepting silent
divergence. Option A's "persist the expansion result" sidesteps
that contract surface.

`PlanFile::version` bumps to `4`. Older saved plans (version `3`) are
rejected with a clear error pointing the user at re-running `plan`.
No on-disk migration and no "v3-without-virtuals works if the config
has no virtuals" graceful-fallback branch — that branch would
re-introduce the asymmetric construction path that the entire design
exists to delete (the type-level guarantee "forgot virtuals = compile
error" is preserved only by the hard reject). Saved plans are
short-lived workflow artifacts (`build_plan_file` writes a
`timestamp` per run; the no-backward-compat project policy
[`feedback_no_backward_compat`] applies); a `version` bump rejecting
older plans is the established pattern.

**Secret redaction.** `build_plan_file` (`carina-cli/src/commands/plan.rs:138-153`)
runs per-kind redaction on every persisted shape:
`redact_secrets_in_plan`, `redact_secrets_in_resource` over each
managed entry, `redact_secrets_in_state` over each current-state
entry. The redaction surface in `carina-core/src/value.rs` provides
typed helpers for `ManagedResource`, `DataSource`, `State`, `Effect`,
`Plan` — but **no `redact_secrets_in_virtual` exists today** because
nothing persisted virtuals. A virtual's attribute map can hold
`ResourceRef`s into managed siblings whose values are secrets, plus
literal values copied through from the inner module's `attributes {
... }` block. Option A requires:

- A new `redact_secrets_in_virtual` in `carina-core/src/value.rs`
  mirroring the existing per-kind helpers (recursively walks the
  attribute map, replaces `Value::Secret(_)` with the redaction
  marker, recurses into `Map` / `List` / `Interpolation`).
- A call in `build_plan_file` that runs it over every entry of the
  new `virtual_resources` field, parallel to the existing
  `redact_secrets_in_resource` call over `sorted_resources`.
- Tests pinning the redaction (a secret value reachable through a
  virtual attribute is redacted in the serialized plan; deserializing
  the plan and resolving through the virtual chain still gives the
  redacted form on the display path).

Without this, a saved plan would leak secret values through the
virtual attribute map — a real regression. The implementation plan
below names this as Step 2a.

## Implementation plan

Implementation lands in a **single follow-up PR** after this design
is merged (`feedback_design_before_implementation_in_pr`). The
migration is atomic — every call site flips in one PR — because the
production blast radius is bounded (8 production sites across 5
files) and a partial migration would leave the codebase with both
APIs visible, defeating the type-level guarantee. Test-only call
sites (~40 in `binding_index.rs` test modules,
`binding_index_split_tests.rs`, `resolver_split_tests.rs`) are
mechanical rewrites and ride in the same PR.

1. Add `ResolvedBindings::pre_apply` (with `PreApplyInputs`) per the
   signature above. Implementation reuses `from_managed_with_state`'s
   body plus the existing layer helpers — no behaviour change, only
   API surface change. **Attach `#[deprecated(note = "use
   ResolvedBindings::pre_apply (carina#3248)")]` to both
   `from_resources_with_state` and `from_managed_with_state` at the
   same time.** Verify protocol's `cargo clippy --workspace
   --all-targets -- -D warnings` then flags any in-PR-window callsite
   that lands on the legacy constructors, closing the
   coexistence-window gap.
2. Rename `add_virtual_resources` → `layer_virtuals_post_apply` and
   `add_data_sources` → `layer_data_sources_post_apply`. Update the
   one production caller (`state_writeback.rs:219`, `:230`) and the
   test callers. No behaviour change.
3. Bump `PlanFile::version` to `4`, add `virtual_resources:
   Vec<VirtualResource>` field. Older saved plans (version `3`)
   error out with a message pointing the user at re-running `plan`.
   `carina_version` field (`carina-cli/src/commands/plan.rs:39`) is
   the informational `env!("CARGO_PKG_VERSION")` string and updates
   automatically per build — no workspace version bump needed; the
   `version: u32` field is the compatibility gate.
3a. Add `redact_secrets_in_virtual` to `carina-core/src/value.rs`
   and call it in `build_plan_file` over every persisted virtual
   entry, parallel to the existing `redact_secrets_in_resource`
   over managed entries. Tests pin the redaction over the virtual
   chain.
4. Migrate every pre-apply call site in the migration table to
   `pre_apply`. The resolver entry points (`resolve_refs_for_plan`,
   `resolve_refs_with_state_and_remote`) change shape to accept
   `&ResolvedBindings`.
5. Update `resolver.rs:138-148`'s doc-comment to reflect the new
   contract: virtuals and data sources are first-class binding
   sources on the pre-apply path; the "wait / upstream-state
   passthrough" guidance applies only to cross-stack / async-completion
   scenarios, not to same-stack module attribute references.
6. Delete `from_resources_with_state` and `from_managed_with_state`.
   All test code now goes through `pre_apply`.

Test coverage:

- Existing tests for `from_resources_with_state` / `from_managed_with_state`
  port to `pre_apply` directly.
- New unit test: `resolve_refs_for_plan` chains through a virtual
  binding to a managed sibling literal (the #3246 reproduction at
  the resolver level).
- New e2e test: directory fixture mirroring `envs/registry/dev/bootstrap/`,
  drives the full plan path, asserts no the legacy replace effect against a
  state holding the resolved literal.
- New regression: saved-plan apply against a fixture whose plan was
  generated with virtuals — the saved plan deserializes correctly,
  the apply path resolves the virtual chain identically to live apply.

## Out-of-scope follow-ups

Filed separately after this design lands (not blocking implementation):

1. `resolve_ref_value`'s lack of a cycle guard (`carina-core/src/resolver.rs:379-453`).
   No visited-set, no depth cap. Safe today because the as-authored
   virtual attribute graph is acyclic (module attributes reference
   inner module bindings, not other outer-scope virtuals), but the
   invariant is implicit. A `debug_assert!` or a small visited-set
   would make a future cycle a hard error rather than a stack overflow.
2. `dependency_bindings` snapshot through virtuals
   (`carina-core/src/resolver.rs:86-91`). The snapshot records only
   the surface binding name (`bs.bootstrap`), so the transitive edge
   `RolePolicy → bs.bootstrap.role` (the managed sibling that
   actually upstreams the value) is lost. Plan-tree rendering uses
   the surface name and is fine; a future consumer that needs the
   true managed-sibling edge for ordering would not be.
3. `trait BindingProvider` over Managed / Virtual / DataSource so
   `pre_apply` becomes generic over slices. The typed-slice shape
   here is enough to fix the headline class; the trait-generic shape
   is a refactor that pays off when a fourth binding kind appears.

## Acceptance criteria

- `ResolvedBindings::from_resources_with_state` and
  `from_managed_with_state` are deleted; the only pre-apply
  constructor is `pre_apply(PreApplyInputs<'_>)`.
- `add_virtual_resources` and `add_data_sources` are renamed to
  `layer_virtuals_post_apply` / `layer_data_sources_post_apply` and
  their doc-comments name the `state_writeback.rs` use case
  explicitly.
- Every pre-apply call site in the migration table goes through
  `pre_apply`. The migration is the entire PR — no partial migration.
- `PlanFile` version bumps to `4` and persists `virtual_resources`.
  Older saved plans (version `3`) error out with a clear message
  pointing the user at re-running `plan`.
- `redact_secrets_in_virtual` is added and called in
  `build_plan_file`; tests pin the redaction over the virtual chain.
- The #3246 reproduction (`carina-rs/infra/envs/registry/dev/bootstrap/`)
  prints "No changes" under the implementation PR's binary.
- A unit test in `carina-core/src/resolver.rs` asserts the virtual-chain
  resolution. An e2e test in `carina-cli/tests/` asserts the plan
  path produces no the legacy replace effect for the headline shape. A
  saved-plan apply test exercises the persisted `virtual_resources`
  path.
- The `apply/mod.rs:1698-1700` "no virtual slice" comment is deleted.
