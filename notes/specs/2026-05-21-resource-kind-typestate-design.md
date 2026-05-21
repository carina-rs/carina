# Resource Kind Typestate Split: Design

<!-- derived-from ./2026-05-02-resource-vs-data-source-design.md -->

## Goal

Split `Resource` (currently a single struct with `kind: ResourceKind`
enum field) into three sibling types — `ManagedResource`,
`VirtualResource`, `DataSource` — so the IR encodes lifecycle
differences in the type system rather than as a runtime tag. Use the
split to fix carina#3169 (apply-path exports written from the
pre-apply attribute snapshot) by making "a `VirtualResource`'s
attributes are post-apply resolved" a compile-time invariant.

Closes #3169 via task decomposition (see the **Task decomposition**
section).

## Non-goals

- Removing `Resource` immediately. Migration is staged: typed
  wrappers ship first, the runtime tag stays until all consumer call
  sites have been ported to the typed view.
- Changing on-disk state file schema. Serde round-trip stays
  byte-identical; the typestate split is in-memory only.
- Refactoring the provider plugin protocol. Provider crates see the
  same `Resource` shape over the WIT boundary; the typestate split
  lives in `carina-core` / `carina-cli`.
- Touching DSL syntax. `let rd = use { … }` and module-call expansion
  emit a `VirtualResource` instead of a `Resource { kind: Virtual,
  … }`, but the surface DSL is unchanged.

## Background

### What broke (#3169)

`carina-cli/src/commands/apply/mod.rs::run_apply` calls
`resolve_refs_with_state_and_remote` **once**, at the head of the
apply path, against the **pre-apply** `current_states` snapshot:

```rust
resolve_refs_with_state_and_remote(
    &mut resources_for_plan,
    &current_states,                 // pre-apply
    &remote_bindings,
    &wait_aliases,
)?;
```

For a `VirtualResource` (e.g. a `let rd = infra_deploy { … }`
module-call proxy with `attributes { role_arn = role.arn }`), this
materialises `rd.attributes.role_arn = OLD_ARN`. Effects then
execute and the real `role` resource's `arn` is updated in
`applied_states[role.id]`; the writeback at
`build_state_after_apply` correctly persists the new ARN to
`state.resources[role].attributes.arn`.

But the virtual resource's `attributes` snapshot is never
re-resolved. `finalize_apply` then calls
`resolve_exports(params, &resources_for_plan, &state,
wait_aliases)`. Inside, `ResolvedBindings::from_resources_with_state`
builds the `rd` binding from `rd.resolved_attributes()` (= OLD_ARN),
and the state-side `or_insert` merge can't fix it because virtual
resources have no row in `state.resources` — there is no
`current_states[rd.id]` entry to override the stale DSL-side value.

Result:

```
state.resources[role].attributes.arn   = NEW_ARN    ✓ (writeback correct)
state.exports.role_arn                  = OLD_ARN    ✗ (frozen virtual attrs)
```

### Why runtime `is_virtual()` checks aren't enough

A runtime fix ("`resolver` skips `is_virtual()` resources; `resolve_exports`
re-resolves them against post-apply state") works but encodes the
invariant as convention. Any future code path that reads
`virtual_resource.attributes` outside the post-apply context
re-introduces the bug. Per project memory
`feedback_type_safety_over_runtime_checks`, the preferred shape for
this class is "make the wrong call unrepresentable".

### Existing typed boundaries

- `ResourceKind::DataSource` is already handled by call-site
  filters in `executor`, `differ`, `destroy`, `apply`; the type
  isn't enforced but the convention is consistent.
- `Effect::Replace`'s `from: Box<State>` typestate-encodes "we have
  a pre-apply state" (vs `Effect::Create(Resource)` which doesn't).
- `WritebackPlan<'a>` from carina#3170 encodes the at-most-one-write
  invariant on state writeback.

The typestate split extends the same direction to the IR root.

## Design

### Three sibling types

```rust
// carina-core/src/resource/managed.rs
pub struct ManagedResource {
    pub id: ResourceId,
    pub attributes: IndexMap<String, Value>,  // fully resolved at pre-apply
    pub directives: Directives,
    pub prefixes: HashMap<String, String>,
    pub binding: Option<String>,
    pub dependency_bindings: BTreeSet<String>,
    pub module_source: Option<ModuleSource>,
    pub quoted_string_attrs: HashSet<String>,
}

// carina-core/src/resource/virtual.rs
pub struct VirtualResource {
    pub id: ResourceId,
    /// Attributes that may contain unresolved `ResourceRef` /
    /// `BindingRef` values. Resolution is deferred until post-apply.
    pub attributes: IndexMap<String, Value>,
    pub binding: Option<String>,
    pub dependency_bindings: BTreeSet<String>,
    pub module_name: String,
    pub instance: String,
    pub quoted_string_attrs: HashSet<String>,
}

// carina-core/src/resource/data_source.rs
pub struct DataSource {
    pub id: ResourceId,
    pub attributes: IndexMap<String, Value>,  // resolved at read time
    pub directives: Directives,
    pub binding: Option<String>,
    pub dependency_bindings: BTreeSet<String>,
    pub module_source: Option<ModuleSource>,
    pub quoted_string_attrs: HashSet<String>,
}
```

Each carries only the fields meaningful for that lifecycle.
`VirtualResource` doesn't carry `directives` (no `prevent_destroy`
applies to a synthetic IR node), `prefixes` (no auto-generated names
on a non-provider resource), or `module_source: Option<…>` (the
`module_name`/`instance` are always set for virtuals — see #2516 — so
flatten the field).

### The post-apply invariant, encoded by type

Resolver signature changes to:

```rust
// pre-apply path: only managed resources have their refs resolved here
pub fn resolve_managed_refs_with_state_and_remote(
    resources: &mut [ManagedResource],
    current_states: &HashMap<ResourceId, State>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    wait_aliases: &[WaitAliasSpec],
) -> Result<(), String>;

// post-apply path: virtual resources are resolved here, against the
// post-apply state. Callable only after apply has run.
pub fn resolve_virtual_refs_post_apply(
    virtuals: &mut [VirtualResource],
    bindings: &ResolvedBindings,
) -> Result<(), String>;
```

Calling `resolve_managed_refs_with_state_and_remote(&mut virtuals,
…)` does not compile — the slice type is wrong. Calling
`resolve_virtual_refs_post_apply(&mut managed, …)` does not compile
for the same reason. The invariant is enforced at the call site by
the borrow checker.

`ResolvedBindings::from_resources_with_state` similarly splits:

```rust
impl ResolvedBindings {
    pub fn from_managed_with_state(
        managed: &[ManagedResource],
        current_states: &HashMap<ResourceId, State>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        wait_aliases: &[WaitAliasSpec],
    ) -> Self;

    /// Add virtual-resource bindings on top of an existing
    /// post-apply view. Must be called **after** any managed-side
    /// bindings have been recorded so the virtuals see the
    /// post-apply attribute values.
    pub fn add_virtual_resources(
        &mut self,
        virtuals: &[VirtualResource],
    ) -> Result<(), String>;
}
```

`resolve_exports` rebuilds bindings as:

```rust
let mut bindings = ResolvedBindings::from_managed_with_state(
    managed,
    &post_apply_states,
    &HashMap::new(),
    wait_aliases,
);
bindings.add_virtual_resources(virtuals)?;
```

The order is enforced by API contract: virtuals cannot be added
before managed bindings exist (the `add_virtual_resources` doc
states this; runtime error if violated, but the typical call site
is one function in `state_writeback.rs`).

### `Plan`/`Effect` keep their existing typed shape

`Effect::Create(Resource)`, `Effect::Update { to: Resource, … }`,
`Effect::Replace { to: Resource, … }`: these continue to carry
`Resource`, not `ManagedResource`. The reason is that effects are
constructed by the differ, which only emits effects for managed
resources anyway (`if resource.is_virtual() { continue; }` —
preserved as a runtime check until step-7 below). Migrating effects
to `ManagedResource` is desirable but not blocking for #3169.

### Common interface

A `trait ResourceLike` exposes the shared accessors
(`id()`, `attributes()`, `binding()`, `dependency_bindings()`) so
that read-only callers — plan tree builders, formatters,
diagnostics — can stay generic over the three types. Write-side
callers (resolver, effect-executor, writeback) take a concrete type
and benefit from the typed dispatch.

## Task decomposition

Each item below is a separate GitHub issue, tracked from the #3169
tracking checklist. Ordering reflects compile-time dependencies.

| # | Task | Crate(s) | Blast radius |
|---|------|----------|--------------|
| 1 | Introduce `ManagedResource` / `VirtualResource` / `DataSource` typed wrappers in `carina-core/src/resource/`. Implement `From<&Resource>` (fallible) so existing call sites can opt-in incrementally. No behavior change. | carina-core | new files, +~400 lines |
| 2 | Introduce `ResourceLike` trait with read-only accessors; impl for all three typed wrappers and for `Resource` itself (back-compat shim). | carina-core | +~150 lines |
| 3 | Split resolver: `resolve_managed_refs_with_state_and_remote(&mut [ManagedResource], …)` and `resolve_virtual_refs_post_apply(&mut [VirtualResource], &ResolvedBindings)`. Keep the legacy `resolve_refs_with_state_and_remote(&mut [Resource], …)` as a thin shim that dispatches per `kind`. | carina-core | resolver.rs, +~120 lines |
| 4 | Split `ResolvedBindings::from_resources_with_state` into `from_managed_with_state` and `add_virtual_resources`. Legacy entry point delegates. | carina-core | binding_index.rs, +~100 lines |
| 5 | Apply-path fix for #3169: at `finalize_apply`, after `build_state_after_apply`, re-resolve `VirtualResource`s against post-apply view before `resolve_exports`. Add test reproducing the carina-rs/infra PR #64 exports drift. | carina-cli | apply/mod.rs, state_writeback.rs, +~250 lines (incl. tests) |
| 6 | Migrate executor (`carina-core/src/executor/phased.rs`'s `DepResolver`, `parallel.rs`'s virtual proxy construction) to take typed slices. Behavior unchanged; eliminates the `matches!(res.kind, ResourceKind::Virtual { .. })` runtime checks. | carina-core | executor/, +~300 lines |
| 7 | Migrate differ / planner / displayed plan-tree builders to take typed slices. Remove `if resource.is_virtual() { continue; }` guards (the type system now prevents virtuals from reaching managed code paths). | carina-core, carina-cli | wide, ~500 lines |
| 8 | Migrate destroy / validate / wiring / data-source paths. | carina-cli, carina-core | wide, ~300 lines |
| 9 | Remove the `kind: ResourceKind` field from `Resource` once all consumers are typed. Inline-merge `Resource` into `ManagedResource` (rename + delete). | carina-core | wide, mostly mechanical, ~200 lines |

Item 5 is the user-facing fix for #3169 — it lands as soon as items
1–4 ship, even if 6–9 remain pending. 6–9 are pure type-safety
hardening with no behavior change.

### Out-of-band manual recovery

The current carina-rs/infra `envs/registry/dev/infra-deploy/`
state row has already been hand-repaired (per the carina#3170 PR
handoff). No automated recovery is in scope; the typestate split
prevents the bug from recurring, but does not retroactively fix
state files that already drifted.

## Compile-time invariants this design establishes

1. **A `VirtualResource`'s `attributes` are never resolved against a
   pre-apply state.** The pre-apply resolver's signature takes
   `&mut [ManagedResource]`; passing a virtual is a compile error.
2. **A managed resource's `attributes` are not resolved twice.** The
   post-apply resolver's signature takes `&mut [VirtualResource]`;
   passing a managed is a compile error.
3. **`ResolvedBindings::add_virtual_resources` requires a managed
   bindings view to exist.** It mutates `&mut self`, so the prior
   construction call must have produced a `ResolvedBindings`.
4. **Effects never carry a `VirtualResource`.** `Effect::Create`
   etc. continue to take `Resource` (legacy) or `ManagedResource`
   (post-item-7) — virtuals cannot construct an effect.

## Test plan

- New unit test in `carina-cli/src/commands/apply/tests.rs`
  reproducing the #3169 scenario: a virtual resource whose
  `attributes.role_arn` references a `ManagedResource` (the IAM
  Role) which gets Replaced; assert that
  `build_state_after_apply` → `resolve_exports` emits the
  post-Replace ARN, not the pre-Replace ARN.
- Per-task integration tests as each migration step lands.
- Final cross-check: PR-merge CI on a stack that exercises module
  call exports across a Replace (e.g.
  `carina-rs/infra envs/registry/dev/infra-deploy/`).

## Rollout

The 9 sub-issues land sequentially against `main`. Each PR is
non-draft and goes through TDD + simplify + 5-round review per the
project's pick-issue convention. Item 5 (the user-facing fix) is
prioritized as the gating deliverable for closing #3169.
