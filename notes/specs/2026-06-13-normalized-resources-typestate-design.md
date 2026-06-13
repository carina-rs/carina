# Normalized-Resources Typestate: Design

## Goal

Make "the desired-side normalization pipeline has been applied" a
compile-time invariant on every resource that enters a CloudControl /
SDK patch builder. After this change, the `apply` executor can no
longer construct an `UpdatePatch` (or any other provider request) from
a `Resource` that has not gone through the same canonicalize →
`normalize_desired` → `merge_default_tags` → `resolve_enum_aliases`
sequence that the plan path runs in `PlanPreprocessor::prepare`. The
broken state ("apply rebuilt the desired value but forgot one stage")
becomes unrepresentable.

This single change closes carina#3480 (apply path skips
`merge_default_tags`, dropping provider `default_tags` on any
in-place tag update), the symptom recorded against
[carina-rs/carina-provider-awscc#351][awscc-351] (CloudControl
`UpdateResource` drops provider-level `default_tags` on any in-place
update that touches a sibling tag), and the structural follow-up
carina#3068 (single-source the desired-side normalization stage
ordering shared by plan and apply).

[awscc-351]: https://github.com/carina-rs/carina-provider-awscc/issues/351

## Non-goals

- Changing the provider plugin protocol or any WIT type. Providers
  keep seeing `Resource` / `UpdateRequest` over the boundary; the
  typestate lives inside `carina-core::executor` and the wiring layer
  that feeds it.
- Touching DSL surface syntax, `provider { default_tags = … }`
  semantics, or the `_default_tag_keys` internal attribute that the
  plan-display renderer reads.
- Changing the on-disk state file format. `ExplicitFields` already
  records the merged-tag children (verified by inspection of the
  carina#3475 repro state); the typestate split is in-memory only.
- Re-normalizing state-side (`current_states`) on the apply path.
  Apply already calls `canonicalize_states_with_schemas` before
  `PlanPreprocessor::prepare`; the state-side passes are
  plan-only-by-construction and out of scope here.

## Background

### What broke (carina#3480 / awscc#351)

A directory whose `providers.crn` carries

```crn
provider awscc {
  default_tags = { ManagedBy = "carina", Project = "issue-351" }
}
```

and whose resource carries `tags = { Name = "v1" }` ends, after
`create`, with three tags on the AWS resource and three children in
the state-side `explicit.tags`. Renaming `Name` (an in-place update,
no `default_tags` edit) leaves the explicit-tree children intact but
strips `ManagedBy` and `Project` from both the AWS resource and the
state-side `attributes.tags`. The CloudControl patch the provider
received contained `{"op":"add","path":"/Tags","value":[{"Key":"Name",
"Value":"v2"}]}` — a full-`/Tags` overwrite with the user-explicit
subset only, and AWS therefore deleted the other tags.

The plan summary at the same moment said only `~ Name: v1 → v2`. No
warning about the dropped default tags, because the plan-time diff
was correct (`prev_explicit.tags.children` contained all three keys,
projection kept all three on both sides, only `Name` differed) — the
breakage happened later in the apply executor.

### Why this is one bug, not two

The plan path and the apply path are two hand-mirrored copies of the
same conceptual pipeline:

| Stage                                    | Plan (`PlanPreprocessor::prepare`)               | Apply (`executor::renormalize`) |
| ---------------------------------------- | ------------------------------------------------ | ------------------------------- |
| `canonicalize_resources_with_schemas`    | yes (called from `apply/mod.rs` before `prepare`) | yes                             |
| `normalize_desired`                      | yes                                              | yes                             |
| `merge_default_tags`                     | **yes**                                          | **no**                          |
| `resolve_enum_aliases_for_resources`     | yes                                              | yes                             |

`carina-core/src/executor/basic.rs::renormalize` already carries a
doc-comment confessing the risk:

> The two orderings happen to match today, asserted only by a doc
> comment. A future edit to either side's ordering would silently
> desync — the exact divergence class carina#3063 exists to eliminate,
> just one level up (sequence rather than function).

The carina#3480 break is exactly that desync, manifesting on the one
stage that was never duplicated into `renormalize` in the first place:
`merge_default_tags`. A patch that drops `merge_default_tags` straight
back into `renormalize` would close awscc#351 today, but it would not
close the *class* — any future stage added to `PlanPreprocessor` is
born missing from `renormalize`, and the next consumer (a new
provider-config-driven mutation) reintroduces the same shape of bug.

### Why a runtime guard is not enough

The shape of the runtime bug is exactly the one the project rule
"make the broken state unrepresentable" was written for: every
executor patch site receives a `&Resource` whose normalization
provenance is invisible at the type level, and the only thing
stopping the bug is a doc comment plus the convention that
`resolve_resource_with_source` always calls `renormalize` at the
end. If a new resolution helper appears tomorrow that bypasses
`renormalize` (or forgets `merge_default_tags`), the compiler will not
catch it.

`feedback_type_safety_over_runtime_checks` and
`feedback_long_term_and_type_safety_at_pr_time` both call this out:
the question "if a new caller appears tomorrow, does it need to
remember to run this stage too?" must be answered by the type
signature, not by reviewer vigilance.

## Design

### A single newtype produced by a single function

Introduce a wrapper in `carina-core::executor`:

```rust
// carina-core/src/executor/normalized.rs (new file)

/// A `Resource` that has been through the full plan-time desired-side
/// normalization pipeline. The only constructor is
/// [`apply_desired_normalization`]; callers that want to feed a
/// resource into a provider patch builder must thread one of these
/// instead of a raw `Resource`.
///
/// The pipeline runs the same sequence as
/// `PlanPreprocessor::prepare`'s desired-side passes, in the same
/// order:
/// 1. `canonicalize_resources_with_schemas`
/// 2. `ProviderNormalizer::normalize_desired`
/// 3. `ProviderNormalizer::merge_default_tags` (per provider config)
/// 4. `resolve_enum_aliases_for_resources`
pub struct NormalizedResource(Resource);

impl NormalizedResource {
    /// Borrow the underlying resource for read-only use (patch
    /// builders, `resolved_attributes`, etc.). No public constructor
    /// outside this module — owning a `NormalizedResource` is the
    /// proof that the pipeline ran.
    pub fn as_resource(&self) -> &Resource { &self.0 }
}

pub async fn apply_desired_normalization(
    resource: Resource,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> NormalizedResource {
    let mut one = [resource];
    canonicalize_resources_with_schemas(&mut one, schemas);
    normalizer.normalize_desired(&mut one).await;
    for config in provider_configs {
        if !config.default_tags.is_empty() {
            normalizer
                .merge_default_tags(&mut one, &config.default_tags, schemas)
                .await;
        }
    }
    resolve_enum_aliases_for_resources(&mut one, factories);
    let [resource] = one;
    NormalizedResource(resource)
}
```

There is intentionally no `From<Resource>` and no public field. The
private tuple field plus the module boundary make
`apply_desired_normalization` the single way to mint the type — the
same shape `ResolvedBindings::pre_apply` uses today to gate "bindings
have been resolved".

### Patch builders take only `&NormalizedResource`

```rust
// carina-core/src/provider.rs
pub fn build_update_patch(
    changed_attributes: &[String],
    to: &NormalizedResource,   // was &Resource
    from: &State,
) -> UpdatePatch { … }

// carina-core/src/executor/replace.rs
pub(super) fn compute_full_diff_patch(
    from: &State,
    to: &NormalizedResource,   // was &Resource
) -> UpdatePatch { … }
```

`UpdateRequest` carries `UpdatePatch`, not a `Resource`, so the WIT
boundary stays unchanged — only the in-process call sites that
construct the patch need a `NormalizedResource`. Today those are
exactly the three patch sites enumerated below.

### Resolver helpers return the typestate

```rust
// carina-core/src/executor/basic.rs
pub(super) async fn resolve_resource(
    resource: &Resource,
    bindings: &ResolvedBindings,
    pipeline: &RenormalizePipeline<'_>,
) -> Result<NormalizedResource, String> { … }

pub(super) async fn resolve_resource_with_source(
    target: &Resource,
    source: &Resource,
    bindings: &ResolvedBindings,
    pipeline: &RenormalizePipeline<'_>,
) -> Result<NormalizedResource, String> { … }
```

`RenormalizePipeline` gains a `provider_configs: &[ProviderConfig]`
field so the resolver helpers can hand them to
`apply_desired_normalization`. Existing fields (`normalizer`,
`factories`, `schemas`) stay; only the new field is added at the
construction site in `execute_effects`. The internal helper
`renormalize` is deleted — its body becomes
`apply_desired_normalization`, and there is no second copy of the
stage order anywhere in the executor.

### `PlanPreprocessor::prepare` calls the same function

The plan side keeps its own seam (it also runs state-side passes,
plan-only `strip/restore` for `Value::Deferred(_)`, and `wait_bindings`
canonicalization), but the desired-side block becomes a single call to
`apply_desired_normalization`. The reordering rule "stages 1–4 happen
in this order" is now expressed exactly once, in the body of
`apply_desired_normalization`. A future stage (say, `normalize_secrets`)
is added there and is automatically picked up by every consumer.

### Why this answers both lenses

- **Root cause** — apply path is missing `merge_default_tags`, so the
  one upstream change is "make apply call the same pipeline as plan,
  by construction". Done by collapsing the two pipeline definitions
  into one function the executor must call.
- **Long-term / type safety** — a fourth stage tomorrow does not need
  every patch site to be touched, because the patch sites only accept
  `&NormalizedResource` and `NormalizedResource` is only produced by
  the canonical function. A new patch site that takes `&Resource`
  cannot exist without the author deliberately weakening the API.

## Migration

### Touched files (compile-time radius)

The patch builders and their direct callers are the radius:

- `carina-core/src/provider.rs` — `build_update_patch` signature.
- `carina-core/src/executor/basic.rs` — `resolve_resource`,
  `resolve_resource_with_source`, `RenormalizePipeline`, the Update
  effect branch.
- `carina-core/src/executor/replace.rs` — `compute_full_diff_patch`,
  the Replace effect cascade path.
- `carina-core/src/executor/phased.rs` — phased cascade patch site.
- `carina-core/src/executor/mod.rs` / new `normalized.rs` — wrapper
  type and pipeline function.
- `carina-cli/src/wiring/mod.rs` — `PlanPreprocessor::prepare` switched
  to call `apply_desired_normalization` for its desired-side block;
  state-side passes and `strip/restore` stay where they are.

`carina-cli/src/commands/apply/mod.rs` already builds the
`unresolved_resources` map and constructs `RenormalizePipeline` via
`execute_effects`; the only change there is passing
`&parsed.providers` into the pipeline-construction call, mirroring
the `provider_configs` argument `PlanPreprocessor::prepare` already
accepts.

Provider crates (`carina-provider-aws`, `carina-provider-awscc`,
`carina-provider-mock`) are unaffected: their `Provider::update`
implementations receive `UpdateRequest`, which has not changed shape.
The plug-in WIT boundary likewise sees no diff.

### Stage order

`apply_desired_normalization`'s body is now the *single* source of
truth for the desired-side ordering. The existing `renormalize`
doc-comment that catalogued the risk of desync is dropped along with
the function. Tests cover the order with a fake normalizer that
records the calls it received (see "Tests" below).

### Compile-fail guard

A doc-test under `carina-core/src/executor/normalized.rs` that
attempts to call `build_update_patch` with a raw `&Resource` ensures
the typestate cannot be downgraded by accident:

```rust
/// ```compile_fail
/// use carina_core::provider::build_update_patch;
/// use carina_core::resource::{Resource, State};
/// let r: Resource = unimplemented!();
/// let s: State = unimplemented!();
/// build_update_patch(&[], &r, &s);   // must not compile
/// ```
```

The check is a one-line guard against a future contributor relaxing
the signature back to `&Resource`.

## Tests

- `carina-core/src/executor/normalized_tests.rs` — covers the
  pipeline stage order, that `merge_default_tags` is invoked when a
  provider config carries non-empty `default_tags`, and that
  `apply_desired_normalization` is idempotent.
- `carina-core/src/executor/basic.rs` Update branch test —
  reproduces the carina#3480 / awscc#351 shape end-to-end: a fake
  provider with `default_tags = { ManagedBy, Project }`, a resource
  with `tags = { Name = "v1" }`, a `Diff::Update` with
  `changed_attributes = ["tags"]`, and asserts the resulting
  `UpdatePatch.ops` carry `{Name, ManagedBy, Project}` — *not*
  `{Name}` alone. This is the failing test that the implementation PR
  will turn green.
- `carina-cli/src/wiring/tests.rs` —
  `test_merge_default_tags_prevents_false_diff` already covers the
  plan-side guarantee; we keep it and add an apply-side sibling that
  drives `execute_effects` with the same fixture and asserts the patch
  ops the provider was handed.

## Acceptance

- `carina#3480` / `awscc#351` repro (a `logs.LogGroup` with a
  `Name`-only tag edit under a `default_tags` provider config) no
  longer drops the default tags from AWS or from state. The
  end-to-end test in the implementation PR covers this without a real
  AWS call.
- `carina#3068` ("single-source the desired-side normalization stage
  ordering") is structurally satisfied. `executor::renormalize` is
  gone; `apply_desired_normalization` is the single function both
  paths call.
- No patch builder anywhere in the workspace accepts a `&Resource`;
  every call site receives a `&NormalizedResource`. The compile-fail
  doctest enforces it.
- All existing tests pass (`cargo nextest run --workspace
  --all-features` + `cargo test --workspace --doc` + clippy +
  `bash scripts/check-*.sh`).

## Task decomposition

The implementation lands as a single PR (no "wide blast radius" split
— the touched files are bounded above and the typed reshape only
works when every patch site flips together). The PR description
includes:

1. The new `NormalizedResource` wrapper and
   `apply_desired_normalization` function.
2. `RenormalizePipeline` extended with `provider_configs`;
   `resolve_resource{,_with_source}` and patch builders flipped to
   the typed signatures.
3. `renormalize` deleted; `PlanPreprocessor::prepare`'s desired-side
   block switched to the new function.
4. Failing-then-green test for carina#3480 (added in the same PR per
   the TDD-for-bug-fixes rule), compile-fail doctest, stage-order
   test.

After merge, awscc#351 can be closed against the carina-side fix once
provider plug-ins are next pinned (no provider-side patch is required
— this is purely a host-side typing change). carina#3068 closes at
the same time.

## Open questions

- Should `NormalizedResource` move into `carina-core::resource`
  alongside `Resource`, or stay in `executor::normalized`? It is
  consumed *only* by executor patch builders today; keeping it next to
  the consumers keeps the executor module self-contained. If a future
  caller outside `executor` needs to build a patch (e.g. a saved-plan
  `apply` path that already has provider configs), promoting the type
  is a non-breaking move. Default: keep in `executor` for now.
- Plan-side `PlanPreprocessor::prepare` currently calls the four
  stages with `&mut [Resource]` rather than `Vec<Resource>` in/out.
  `apply_desired_normalization` as written above takes `Resource`
  by value because executor resolution constructs a fresh `Resource`
  per effect. For the plan side we can either (a) provide an
  `apply_desired_normalization_in_place(resources: &mut [Resource],
  …)` companion that runs the same stages without producing
  `NormalizedResource` (plan side never asks for it), or (b) collect /
  re-emit. (a) is cheaper at the call site and keeps the existing
  `&mut [Resource]` shape; pick (a) unless review surfaces a reason
  not to.
