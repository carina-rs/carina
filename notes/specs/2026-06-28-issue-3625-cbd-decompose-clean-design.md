# Issue #3625 CBD Replace Decomposition — Clean Redesign Plan

Issue: https://github.com/carina-rs/carina/issues/3625
Prior PR (closed): https://github.com/carina-rs/carina/pull/3626
Prior design doc: `notes/specs/2026-06-27-issue-3625-cbd-decompose-plan.md` (on the closed `issue-3625-cbd-decompose` branch)

## Goal

Fix `carina apply` so that a create-before-destroy (CBD) replacement schedules the consumer's update between the new resource's create and the old resource's delete. Today the consumer update is invisible to the scheduler, the old delete races ahead of it, and the provider rejects the delete with "used by another resource".

The approach is the same A-2 chosen for the prior PR: drop `Effect::Replace` and decompose every replacement into independent `Effect::Create` / `Effect::Update` / `Effect::Delete` effects in the plan.

## Why a clean rewrite

The prior PR landed the decomposition incrementally:

- Rounds 1–6 produced the `Effect::Replace` removal and dropped the rename Update.
- Rounds 7–8 added the `NameOverride` mechanism needed to operate with a temporary name on the cloud side.
- Round 9 surfaced multiple type-safety problems in that mechanism.

Specifically, the following CLAUDE.md violations remained:

- `apply_name_overrides` is invoked at three entry points with a copy-pasted 7-step "resolve → apply → rebuild bindings → re-resolve → re-apply → rebuild" dance (sibling code paths doing the same dance).
- The `unique_name_attribute`-missing CBD check exists only on the cascade-promoted path; user-authored `directives { create_before_destroy = true }` on a schema without a unique-name attribute still fails silently at apply time (one bug producing symptoms in multiple code paths).
- `NameOverride.original_value: String` uses the empty string as a sentinel for "legacy unknown", so the type cannot distinguish legacy migration from a deliberately stored empty value.
- The synthetic-key registration for anonymous resources is gated to `ScheduleInputs::Apply`, with a comment that future destroy paths must remember to register it (callers-must-remember-to seam).

Instead of patching these one more time, rewrite the design so they are resolved at the type level from the start. Keep every correctness insight earned in the prior PR: drop the rename Update; use a temporary name on the cloud side; surface consumer updates as independent effects; introduce `blocked_by_updates`; handle anonymous resources in scheduling; use a fixed-point loop for chained-CBD auto-promotion; box the oversized variants.

## Root cause (recap)

Today `Effect::Replace` executes `create → cascading update → delete → optional rename` sequentially inside the executor. To the scheduler, Replace is one effect and `cascading_updates` is not a graph node.

When a consumer already has its own `Effect::Update` in the plan, `cascade_dependent_updates` skips it. The consumer update then has no ordering constraint relative to the old delete, and the two run in parallel.

A-2 eliminates the hidden operation by surfacing the consumer update and the old delete as plan effects and using ordinary scheduler edges to order them.

## Final design

### CBD execution order (no rename)

```
Create(new replaced resource, temporary name)
  -> Update(consumers that reference new value)
  -> Delete(old replaced resource)
```

This is the Round 6 conclusion from the prior PR. Drop the `can_rename` distinction and leave the new resource on the cloud side under its temporary name. Record the override in state via `name_overrides` so the next plan recognises that the DSL name and the cloud name are deliberately divergent.

Not emitting a rename Update means there is no scheduler cycle for consumers that read the renamed attribute. The cycle class the prior PR fought through Rounds 3–5 simply cannot form.

### DBC (destroy-before-create) execution order

```
Delete(old)
  -> Create(new)
```

DBC does not need a temporary name (the old resource is gone before the new one is created, so there is no name collision). It does not register a `permanent_name_overrides` entry.

### `Effect` structural changes

Remove `Effect::Replace`. The supporting types the prior PR introduced for the now-discarded rename Update — `CascadingUpdate`, `Effect::Update.from: UpdateBase`, `ScheduleEdge::BlockedByIfDelete` (for `Effect::Replace.apply_edges`) — are simply not introduced. `Effect::Update.from` stays `Box<State>`.

Keep / add the following:

- `Effect::Delete.blocked_by_updates: HashSet<ResourceIdentity>`, `#[serde(default, skip_serializing_if = "HashSet::is_empty")]`.
- `ScheduleEdge::BlockedByIfDelete` stays as the edge `Effect::Delete.dependencies` emits during apply (the PR #3624 destroy-time semantics). Delete-side dependencies create an edge only when the target binding resolves to another Delete — Create/Update targets do not get an edge.
- Box the large variants from the start: `Effect::DeferredReplace(Box<DeferredReplacePayload>)`, and the `state` fields on `BasicEffectResult::Success` / `BasicEffectResult::PartialSuccess`.

### Identity-keyed scheduling (no BindingKey enum)

The original Phase 1 section called for a `BindingKey::{Binding(String), Anonymous { resource_type, name }}` enum and `Effect::binding_key()`. That design predated #3632 and the merged identity foundation.

Per the identity-axis spec merged in #3633, identity is one axis: `ResourceIdentity`. It is not a two-variant key that distinguishes let-bound from anonymous resources. The scheduler indexes effects directly with `HashMap<ResourceIdentity, usize>`; no `BindingKey` enum is needed. The closed PR #3630 exposed why the old anonymous shape was wrong: it reached for resource type plus user-facing `name` rather than a system-assigned identity. The #3633 and #3647 merges closed that gap by making every scheduler-bound `Effect` payload carry resolved identity through the `Resolved*` types.

`Effect::identity() -> ResourceIdentity` is the projection from an effect to its scheduler key. `ScheduleEdge` variants carry `ResourceIdentity`, so edge consumers perform strict keyed lookup instead of reinterpreting strings.

The lookup API splits along the type the caller already holds:

- `lookup_by_identity(&ResourceIdentity)` is used for strict keyed lookup, including every `ScheduleEdge` consumer.
- `lookup_by_string_ref(&str)` is used by plain-string ref paths: `Resource::dependency_bindings`, `directives.depends_on`, value-position binding refs, and composition expansion. It wraps the string in `ResourceIdentity::new` and delegates to `lookup_by_identity`; no anonymous fallback scan is needed.

Phase 2 uses the same key directly: `Effect::Delete.blocked_by_updates: HashSet<ResourceIdentity>`.

### `ReplacementGroup` — atomic addition of Create + Delete + display metadata

`Plan::add_replacement(ReplacementGroup)` is offered as `pub(crate)` and adds the Create, the Delete, and the `ReplaceDisplayMetadata` together. External crates can still construct standalone `Effect::Create` / `Effect::Delete`, but only `Plan::add_replacement` can register a paired entry in `Plan.replace_display`.

```rust
pub(crate) struct ReplacementGroup {
    pub id: ResourceId,
    pub create: Resource,
    pub delete: DeleteParams,
    pub directives: Directives,
    pub changed_create_only: ChangedCreateOnly,
    pub cascade_ref_hints: Vec<(String, String)>,
    pub temporary_name: Option<TemporaryName>,  // Some only on CBD
    pub consumer_updates: Vec<ResourceIdentity>, // populates blocked_by_updates
}
```

Inside `Plan::add_replacement`:

- Push `Effect::Create(create)`, capture its index.
- Push `Effect::Delete { blocked_by_updates: consumer_updates.into_iter().collect(), … }`, capture its index.
- Push `Plan.replace_display` with `ReplaceDisplayMetadata { create_idx, delete_idx, … }`.
- On CBD with `temporary_name.is_some()`, also push a `Plan.permanent_name_overrides` entry.

### `permanent_name_overrides` and `NameOverride` — `Option` distinguishes legacy from persisted

The prior PR used `NameOverride.original_value: String` with empty-string as a "legacy unknown" sentinel. Use `Option<String>` from the start:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NameOverride {
    pub temp_value: String,
    /// The DSL value at the time the override was recorded. `None` indicates
    /// a state file migrated from v7 or earlier where the original was not
    /// captured. The single comparison site lives in `should_apply_override`.
    #[serde(default)]
    pub original_value: Option<String>,
}
```

Apply decision:

```rust
pub fn should_apply_override(
    current_dsl_value: Option<&str>,
    override_: &NameOverride,
) -> ApplyDecision {
    match (&override_.original_value, current_dsl_value) {
        // Recorded DSL value equals the current DSL value: apply (expect no-op).
        (Some(orig), Some(cur)) if orig == cur => ApplyDecision::Apply,
        // Mismatch: user edited the DSL, skip and let the differ fire a new CBD.
        (Some(_), Some(_)) => ApplyDecision::Skip,
        // Current DSL value is unresolved: cannot compare, conservatively apply
        // (matching the legacy behaviour) and emit a separate warning.
        (Some(_), None) => ApplyDecision::ApplyWithUnknownDsl,
        // Legacy migration: original unknown, always apply with a warning.
        (None, _) => ApplyDecision::ApplyLegacy,
    }
}
```

Callers destructure `ApplyDecision`, so the four cases — Apply, Skip, ApplyWithUnknownDsl, ApplyLegacy — are exhaustively handled. Adding a fifth caller in the future requires matching every arm or failing to compile.

### `OverrideAwareResources` — typed marker for "resolved + override-applied"

The prior PR pasted the override-and-rebuild sequence at three entry points. Roll it into a single helper from the start:

```rust
pub struct OverrideAwareResources {
    resources: Vec<Resource>,
    bindings: ResolvedBindings,
    legacy_overrides_applied: Vec<ResourceId>,  // warning targets
    skipped_overrides: Vec<ResourceId>,         // detected DSL changes
}

impl OverrideAwareResources {
    /// Single constructor. Performs first-pass resolver, keeps a comparison
    /// snapshot, applies overrides, rebuilds bindings, runs the second-pass
    /// resolver.
    pub fn build(
        unresolved: Vec<Resource>,
        state_file: Option<&StateFile>,
        inputs: PreApplyInputs<'_>,
    ) -> Result<Self, ResolverError> { ... }
}
```

Downstream APIs (`create_plan`, scheduler, IAM preflight) accept `&OverrideAwareResources`. Bypassing the override step is impossible: a new entry point either calls `OverrideAwareResources::build` or fails to typecheck.

### `apply_name_overrides` runs after the resolver

Inside `OverrideAwareResources::build`:

1. **First-pass resolver**: run `resolve_refs_with_state_and_remote` on the unresolved resources to produce resolved values.
2. **Keep a comparison snapshot**: clone the resolved resources before override application (used in step 3 to defeat the chained-CBD second-pass problem).
3. **Override decision**: for each resource, call `should_apply_override`. The comparison uses the attribute value from the snapshot — not from a partially-overridden working copy — so an upstream resource's override does not pollute the downstream override's match.
4. **Apply**: on `Apply` / `ApplyWithUnknownDsl` / `ApplyLegacy`, rewrite the desired attribute to `temp_value`; on `Skip`, leave it alone.
5. **Rebuild bindings**: produce a fresh `ResolvedBindings::pre_apply` from the override-applied resources.
6. **Second-pass resolver**: re-resolve refs like `<binding>.name` so consumers see the override-aware value.
7. Return the assembled `OverrideAwareResources`.

The chained-CBD problem (A → B name reference) is resolved by step 3: B's `current_dsl_value` is read from the pre-override snapshot, not from A-overridden bindings, so B's `original_value` match is not invalidated by A's override.

### `unique_name_attribute`-missing check lives inside `generate_temporary_name`

The prior PR added the check only on the cascade-promoted path. Encode it in the type signature from the start:

```rust
pub fn generate_temporary_name(
    schema: &Schema,
    name: &str,
) -> Result<TemporaryName, MissingNameAttributeError>;
```

Every CBD construction path (auto-promote, user directive, cascade) must `?` the result. The error is mapped to a `PlanError` and surfaces at plan time. A CBD on a schema without a `unique_name_attribute` cannot reach the plan output.

### Chained CBD auto-promote — fixed-point loop

The prior PR retrofitted this loop in Round 7. Start with it:

```rust
loop {
    let promoted = promote_pending_replaces_for_dependents(&mut pending_replaces, ...);
    if !promoted { break; }
}
```

Lookups in `pending_replaces` use `ResourceIdentity`, so anonymous middle nodes are included.

### `Plan.replace_display` rendering

The display layer reads `replace_display` and collapses same-binding Create/Delete pairs into a single `+/-` row. Consumer updates render as regular `~` rows. `Plan::summary` counts `replace_display.len()` and excludes the constituent Create/Delete from raw counts so they do not double-count.

`ReplaceDisplayMetadata.previous_attributes` carries pre-replace state values, which can include secrets. The redaction walker visits it.

### Saved plan format v8

Bump `PlanFile.version` to v8. Introduce `PlanFile::CURRENT_VERSION`. Round-trip tests assert against that constant rather than a literal.

`redact_secrets_in_plan` clones the Plan and redacts in place rather than rebuilding via `Plan::new() + add(effect)`. New `Plan` fields are preserved by default; `previous_attributes` walks through the same `redact_value` path as effect-side values.

The apply-from-plan boundary (`run_apply_from_plan_with_observer_factory`) reads the version from the raw JSON before deserialising `PlanFile`. Anything other than v8 produces an explicit "re-run carina plan" error rather than a low-level serde failure.

### v7 → v8 migration

Old state files store `name_overrides` as `HashMap<String, String>`. `NameOverride::Deserialize` accepts the legacy bare string via an untagged enum and lifts it to `NameOverride { temp_value: s, original_value: None }`.

The migration window risk — an operator who edits the DSL name during the upgrade run silently has the rename overwritten — is mitigated by:

- Stderr warning per affected resource listing the temp value and the override target.
- `carina apply` refuses to run against a v7 state without the `--accept-legacy-name-overrides` flag. The operator must either pass the flag or run `carina plan` first to inspect drift before applying.

Once the state has been written under v8, subsequent applies do not require the flag.

### Boxing large variants

`Effect::DeferredReplace(Box<DeferredReplacePayload>)`, `BasicEffectResult::Success { state: Option<Box<State>> }`, and `BasicEffectResult::PartialSuccess { state: Box<State> }` are boxed from the start. `#[allow(clippy::large_enum_variant)]` is not used.

### DBC (destroy-before-create) wording

New files and new doc text use `DBC`. Older code/docs that still say `DBD` are left alone in this PR — a separate cleanup PR will sweep them together to keep the scope of this work focused.

## Scheduler edge summary

`Effect::apply_edges()` / `destroy_edges()` remain exhaustive over `Effect` (the PR #3624 contract). New variants fail to compile until both directions are declared.

| Variant | apply_edges | destroy_edges |
|---|---|---|
| Create | (none) | (none — destroy plans contain no Create) |
| Update | `to.attributes` resource refs as `DependsOn` | (none) |
| Delete | `dependencies` as `BlockedByIfDelete` + `blocked_by_updates` as `DependsOn` | `dependencies` as `BlockedBy` |
| Read | (none) | (n/a) |
| Import | (none) | (n/a) |
| Remove | (none) | (n/a) |
| Move | (none) | (n/a) |
| Wait | `until` target as `DependsOn` + `depends_on` directive | (n/a) |
| DeferredReplace | absorbed-delete and template-driven edges | (n/a) |

`ScheduleEdge::BlockedByIfDelete` is the apply-side edge `Effect::Delete.dependencies` emits. It creates an edge only when the target binding resolves to another Delete — Create/Update targets are ignored, which is the constraint the prior PR's Round 2 review made explicit.

## Impact surface

Comparable to the prior PR's impact surface (30–40 files across `carina-core`, `carina-cli`, `carina-tui`). Differences:

- No `UpdateBase` machinery (no rename Update).
- No copy-paste at entry points (the override application is single-helper).
- `OverrideAwareResources` newtype changes downstream API signatures.

## Phase breakdown

Each phase is a standalone PR that stacks on the previous one and reaches green verify on its own.

### Phase 0 — Red e2e and mock provider hooks

Goal: establish the regression test and the mock-provider machinery the later phases need.

Tasks:

- T0.1: Add `CARINA_MOCK_OP_LOG` and `CARINA_MOCK_CREATE_FAIL_FOR` hooks to the mock provider.
- T0.2: Add `carina-cli/tests/apply_cbd_consumer_ordering_e2e.rs` with the WebACL + Distribution-equivalent fixture, asserting the `create web_acl → update distribution → delete web_acl` order.

Acceptance: the e2e test is Red as intended; mock-provider unit tests are green; `cargo check` passes.

PR: standalone (only the mock-provider extension and the Red e2e).

### Phase 1 — `ResourceIdentity` scheduler foundation

Goal: lay the typed scheduler foundation. Replace string-keyed binding lookups so anonymous resources are first-class.

Tasks:

- T1.1: Introduce `Effect::identity() -> ResourceIdentity`.
- T1.2: Switch the scheduler's binding index to `HashMap<ResourceIdentity, usize>` inside `build_effect_dependency_analysis`.
- T1.3: Update existing test fixtures.

Acceptance: full verify green; new unit tests cover both binding-name and anonymous lookups.

PR: stacks on Phase 0 (or branches from main; no functional dependency).

### Phase 2 — `Effect::Delete.blocked_by_updates` and edge reshape

Goal: introduce the apply-side edge that orders Delete after consumer Updates. Re-confirm `BlockedByIfDelete` semantics.

Tasks:

- T2.1: Add `blocked_by_updates: HashSet<ResourceIdentity>` to `Effect::Delete`.
- T2.2: `apply_edges()` returns both the `BlockedByIfDelete` edges from `dependencies` and the `DependsOn` edges from `blocked_by_updates`.
- T2.3: `destroy_edges()` returns only the `BlockedBy` edges from `dependencies`.
- T2.4: Update every `Effect::Delete` construction site, including tests.

Acceptance: full verify green; scheduler-edge unit tests assert CBD ordering.

PR: stacks on Phase 1.

### Phase 3 — `ReplacementGroup` + `Plan::add_replacement`

Goal: provide the typed shape that adds Create + Delete + display metadata atomically.

Tasks:

- T3.1: Add `replace_display: Vec<ReplaceDisplayMetadata>` to `Plan`, `#[serde(default)]`.
- T3.2: Add `ReplacementGroup` and `Plan::add_replacement` as `pub(crate)`.
- T3.3: Finalise `ReplaceDisplayMetadata` (`create_idx`, `delete_idx`, `create_before_destroy`, `changed_create_only`, `cascade_ref_hints`, `temporary_name`, `previous_attributes`).

Acceptance: full verify green; `Plan::add_replacement` unit tests verify atomic registration of Create + Delete + replace_display.

PR: stacks on Phase 2.

### Phase 4 — Decommission `Effect::Replace` in the differ

Goal: production code never constructs `Effect::Replace`.

Tasks:

- T4.1: Add `PendingReplace` intermediate representation inside `differ/plan.rs`.
- T4.2: Rewrite `cascade_dependent_updates` on top of `PendingReplace`; auto-promote runs as a fixed-point loop.
- T4.3: Route `decompose_replace_into_effects` through `Plan::add_replacement(ReplacementGroup)`.
- T4.4: Make `generate_temporary_name` return `Result<TemporaryName, MissingNameAttributeError>`; surface `MissingNameAttributeError` as `PlanError`.
- T4.5: Rewrite cascade tests against the new shape (independent Update added or reused; `Delete.blocked_by_updates` carries the consumer binding; create-only consumer promotes to Replace; unique_name_attribute-missing CBD produces a plan error).

Acceptance: `grep Effect::Replace` returns zero hits; Phase 0's Red e2e turns Green.

PR: stacks on Phase 3.

### Phase 5 — `NameOverride` + `OverrideAwareResources` + post-resolver override apply

Goal: introduce the override mechanism with type-level safety from the start.

Tasks:

- T5.1: Add `NameOverride { temp_value, original_value: Option<String> }` in `carina-state` (untagged Deserialize lifts the v7 bare string).
- T5.2: Add `should_apply_override` and the `ApplyDecision` enum.
- T5.3: Add `Plan.permanent_name_overrides: Vec<PermanentNameOverride>`.
- T5.4: In `decompose_replace_into_effects`, push a permanent_name_overrides entry whenever CBD generates a temporary name.
- T5.5: In `state_writeback`, persist `NameOverride` for every CBD that uses a temporary name (no `can_rename` branching).
- T5.6: Add the `OverrideAwareResources` newtype and the `::build` constructor; it runs first-pass resolver → snapshot → override apply → bindings rebuild → second-pass resolver inside one helper.
- T5.7: Switch every entry point (`apply/mod.rs`, `plan.rs`, `wiring/mod.rs`, `fixture_plan.rs`) to a single `OverrideAwareResources::build` call. Downstream APIs (`create_plan`, etc.) take `&OverrideAwareResources`.
- T5.8: `destroy.rs` and `state.rs` continue to operate without `OverrideAwareResources`; document the reason inline.
- T5.9: Emit a per-resource v7 → v8 migration warning to stderr.
- T5.10: `carina apply` refuses to read a v7 state without `--accept-legacy-name-overrides`.

Acceptance: full verify green; chained-CBD second-pass (A → B name reference) e2e is green (B's override is not falsely skipped); var-substituted name e2e is green.

PR: stacks on Phase 4. Largest phase; may split into 2–3 sub-PRs if needed.

### Phase 6 — Display restoration (`+/-` collapse) and detail rows

Goal: the scheduler holds Create + Delete separately, but the operator-facing display still shows a single `+/-` row.

Tasks:

- T6.1: In `carina-cli/src/display/mod.rs`, read `Plan.replace_display` and render the `+/-` row.
- T6.2: Add a `build_replace_rows` helper in `carina-core/src/detail_rows.rs` that takes `ReplaceDisplayMetadata` as input.
- T6.3: In `carina-core/src/plan_tree.rs`, collapse Create/Delete pairs in the tree using `replace_display`.
- T6.4: In `Plan::summary`, count replaces from `replace_display.len()`.
- T6.5: Update TUI display (`carina-tui/src/app/mod.rs`, `ui/detail.rs`) to consume `replace_display`.
- T6.6: Refresh snapshot tests (insta).

Acceptance: full verify green; `+/-` rendering matches the prior PR; consumer updates render as `~`.

PR: stacks on Phase 5.

### Phase 7 — Remove legacy executor paths; saved-plan v8

Goal: delete `executor/replace.rs` and `executor/phased.rs`; bump the saved-plan version.

Tasks:

- T7.1: Delete `executor/replace.rs` (`execute_replace_parallel`, `execute_cbd_replace_parallel`, `execute_dbc_replace_parallel`, `ReplaceContext`, `SingleEffectResult::Replace`).
- T7.2: Delete the Replace-specific phase in `executor/phased.rs` (the basic scheduler handles every shape).
- T7.3: Bump `PlanFile.version` to v8; introduce `PlanFile::CURRENT_VERSION`.
- T7.4: Rewrite `redact_secrets_in_plan` to clone-in-place (new fields are not dropped).
- T7.5: In `run_apply_from_plan_with_observer_factory`, read the version from raw JSON and reject anything other than v8 with an explicit error.

Acceptance: full verify green; `grep Effect::Replace` returns zero hits; saved-plan round-trip tests preserve `replace_display` and `permanent_name_overrides`.

PR: stacks on Phase 6.

### Phase 8 — Box variants and DBC wording

Goal: lock in the Round 9 cleanups from the start of the new tree.

Tasks:

- T8.1: Box `Effect::DeferredReplace`.
- T8.2: Box the `state` fields on `BasicEffectResult::Success` / `PartialSuccess`.
- T8.3: Confirm clippy is clean without `#[allow(clippy::large_enum_variant)]`.
- T8.4: Standardise new docs/comments on `DBC` (destroy-before-create).

Acceptance: full verify green; `grep '#\[allow(clippy::large_enum_variant)' --include="*.rs" carina-core/` returns zero hits.

PR: stacks on Phase 7.

### Phase 9 (optional) — Follow-up issue triage

Goal: park scope-adjacent work as separate issues so the main PR chain stays focused.

Tasks:

- T9.1: Re-file or reopen the `Effect::DeferredReplace` consumer-ordering verification issue (the existing #3627 can be amended).
- T9.2: `Delete.blocked_by_updates: HashSet<ResourceIdentity>` already adopts the typed key — close any prior follow-up issue requesting that reshape.
- T9.3: File the repo-wide `DBD → DBC` cleanup PR for older code/docs left out of this work.

## Test strategy

Each phase's acceptance criteria include the test additions for that phase. Cumulatively, `apply_cbd_consumer_ordering_e2e.rs` and the unit suites grow the following coverage:

- `cbd_replace_orders_consumer_update_between_create_and_delete` (Phase 0)
- `test_cbd_consumer_reading_name_does_not_break` (Phase 4)
- `test_cbd_permanent_name_override_persists` (Phase 5; asserts the second plan is a no-op)
- `test_cbd_dsl_rename_after_apply_triggers_new_cbd` (Phase 5)
- `chained_cbd_anonymous_middle_node_auto_promoted` (Phase 4 + Phase 5; covers anonymous + chained together)
- `auto_promote_with_missing_unique_name_attribute_emits_plan_error` (Phase 4)
- `apply_name_overrides_applies_for_var_substituted_dsl_name` (Phase 5)
- `apply_name_overrides_skips_for_ref_substituted_dsl_name_rename` (Phase 5)
- `chained_cbd_consumer_reading_b_dot_name_resolves_to_b_override` (Phase 5; second-pass regression guard)
- `redact_secrets_in_plan_preserves_replace_display` (Phase 7)
- `redact_secrets_in_plan_preserves_permanent_name_overrides` (Phase 7)
- `redact_secrets_in_plan_redacts_secret_in_previous_attributes` (Phase 7)
- `saved_plan_round_trip_preserves_permanent_name_overrides` (Phase 7)
- `legacy_v7_state_apply_requires_accept_legacy_name_overrides_flag` (Phase 5)

## Risks and open questions

- The `OverrideAwareResources` deref surface — many downstream paths want to read the underlying `Vec<Resource>`. Decide between exposing `Deref<Target=[Resource]>` as `pub(crate)` and adding focused query methods. Resolve at implementation time.
- The `ResourceIdentity` serde format — `Delete.blocked_by_updates: HashSet<ResourceIdentity>` appears in saved plans, so the string identity shape must remain stable across saved-plan round trips.
- The migration-warning emission point — emit at state-load, once. Reuse `log_state_migration_once` (or equivalent) if it already exists.
- Scope of the `--accept-legacy-name-overrides` flag — apply only, or every state-touching subcommand. `apply` alone is sufficient because `plan` never mutates state.

## How to proceed

1. Reviewer signs off on this plan.
2. Phase 0 implementation starts under TDD via Codex delegation.
3. Each completed phase lands as its own PR, stacking on the previous one.
4. Once every phase has landed, Issue #3625 closes via the final phase PR's `Closes #3625`.
