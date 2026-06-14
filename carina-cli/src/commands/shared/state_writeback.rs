//! State write-back helpers shared between `apply` and `destroy`.
//!
//! These helpers translate the in-memory execution result into a persisted
//! `StateFile`: applying name overrides, building the post-apply state,
//! resolving exports, and (for destroy) removing destroyed resources.

use std::collections::{HashMap, HashSet};

use carina_core::effect::Effect;
use carina_core::executor::ExecutionResult;
use carina_core::plan::Plan;
use carina_core::resource::{ConcreteValue, Resource, ResourceId, State, Value};
use carina_core::schema::SchemaRegistry;
use carina_state::{LockInfo, ResourceState, StateBackend, StateFile};
use colored::Colorize;

use crate::error::AppError;

/// Apply permanent name overrides from state to desired resources.
///
/// When a create_before_destroy replacement produces a non-renameable temporary name
/// (can_rename=false), the state stores the permanent name. This function applies
/// those overrides so the plan doesn't detect a false diff.
pub(crate) fn apply_name_overrides(resources: &mut [Resource], state_file: &Option<StateFile>) {
    let state_file = match state_file {
        Some(sf) => sf,
        None => return,
    };

    let overrides = state_file.build_name_overrides();
    if overrides.is_empty() {
        return;
    }

    for resource in resources.iter_mut() {
        if let Some(name_overrides) = overrides.get(&resource.id) {
            for (attr, value) in name_overrides {
                resource.attributes.insert(
                    attr.clone(),
                    Value::Concrete(ConcreteValue::String(value.clone())),
                );
            }
        }
    }
}

/// Queue a state refresh for a resource after a failed operation.
///
/// This is kept for use by tests in `tests.rs`. The core executor has its own
/// internal version.
#[cfg(test)]
pub(crate) fn queue_state_refresh(
    pending_refreshes: &mut HashMap<ResourceId, String>,
    id: &ResourceId,
    identifier: Option<&str>,
) {
    if let Some(identifier) = identifier.filter(|identifier| !identifier.is_empty()) {
        pending_refreshes.insert(id.clone(), identifier.to_string());
    }
}

/// Resource attribute map ready for export-resolution after an apply
/// or state-refresh.
///
/// Pinning the merge rule in a type closes a class of bug that
/// recurred between #3266 and #3271. Two facts about the inputs:
///
/// 1. `state.resources` carries only **managed** resources — data
///    sources are read each run and discarded at writeback, so
///    consulting `state.resources` alone leaves any
///    `<data_source>.<attr>` reference in `exports {}` unresolved
///    (carina#3266, carina#3271).
/// 2. `current_states` holds the live execution view — every
///    resource read this run, including data sources. For managed
///    resources it may still carry the **pre-apply** snapshot
///    (whatever the executor read at refresh time, before the apply
///    mutation ran), so the post-apply attribute values in
///    `state.resources` must win on key collision.
///
/// The single correct merge is therefore: seed from
/// `current_states.clone()` (so data sources survive), then overlay
/// entries derived from `state.resources` (so managed
/// post-apply values win). Earlier ad-hoc call sites that built
/// `current_states` HashMaps by hand re-introduced the bug each
/// time they forgot one half of the merge; this newtype forces the
/// merge to happen exactly once, in
/// [`PostApplyStates::from_current_and_state`].
#[derive(Debug, Clone)]
pub(crate) struct PostApplyStates {
    map: HashMap<ResourceId, carina_core::resource::State>,
}

impl PostApplyStates {
    /// Build the post-apply view from the live execution `current_states`
    /// (with data-source read results) and the `StateFile` whose
    /// `state.resources` was just rewritten by the apply writeback.
    ///
    /// State-derived managed entries win on key collision (see the
    /// type-level doc-comment for why).
    pub(crate) fn from_current_and_state(
        current_states: &HashMap<ResourceId, carina_core::resource::State>,
        state: &StateFile,
    ) -> Self {
        let mut map = current_states.clone();
        for rs in &state.resources {
            let id = ResourceId::with_provider(
                &rs.provider,
                &rs.resource_type,
                &rs.name,
                rs.directives.provider_instance.clone(),
            );
            let attrs: HashMap<String, carina_core::resource::Value> = rs
                .attributes
                .iter()
                .filter_map(|(k, v)| {
                    carina_core::value::json_to_dsl_value(v).map(|val| (k.clone(), val))
                })
                .collect();
            map.insert(
                id.clone(),
                carina_core::resource::State::existing(id, attrs),
            );
        }
        Self { map }
    }

    /// Borrow the underlying map for callers that need to feed it
    /// into a `PreApplyInputs.current_states` slot. Read-only — the
    /// merge is finalized at construction time.
    pub(crate) fn as_map(&self) -> &HashMap<ResourceId, carina_core::resource::State> {
        &self.map
    }
}

/// Input parameters for `finalize_apply`.
///
/// Groups the execution result, resource data, and backend configuration
/// needed to save state after an apply operation.
pub(crate) struct FinalizeApplyInput<'a> {
    pub result: &'a ExecutionResult,
    pub state_file: Option<StateFile>,
    pub sorted_resources: &'a [Resource],
    /// Data sources (`read`-keyword resources). Layered into the
    /// export-resolution binding view so `exports { x = some_read.attr }`
    /// resolves (carina#3181).
    pub data_sources: &'a [carina_core::resource::DataSource],
    /// Live execution view of every resource read this run, including
    /// data-source results. Forwarded to `resolve_exports` so a
    /// `<data_source>.<attr>` export reference can resolve against
    /// the provider-read attributes — `state.resources` never persists
    /// data sources, so consulting it alone leaves those references
    /// unresolved and `state.exports` stuck at the prior literal
    /// (carina#3266).
    pub current_states: &'a HashMap<ResourceId, State>,
    pub plan: &'a Plan,
    pub backend: &'a dyn StateBackend,
    pub lock: Option<&'a LockInfo>,
    pub schemas: &'a SchemaRegistry,
    /// `Some(params)` rebuilds `state.exports` from the source
    /// configuration's `exports {}` block — empty `params` clears the
    /// map (#2932). `None` preserves the existing `state.exports` and
    /// is used by `apply --plan` because saved plan files do not
    /// persist `export_params` today; with `None` the apply path
    /// neither resolves nor wipes them, leaving the prior values
    /// intact for the next source-driven `carina apply` to
    /// reconcile.
    pub export_params: Option<&'a [carina_core::parser::InferredExportParam]>,
    /// Wait-binding aliases (carina#3085). When export expressions
    /// reference a `wait` binding (`exports { x = cert_issued.arn }`),
    /// these make `<wait-binding>.<attr>` resolve to `<target>.<attr>`
    /// at writeback, the same passthrough the plan path now applies.
    /// Empty when the configuration declares no `wait` bindings, or
    /// for the `apply --plan` path that has no source-side wait view
    /// (paired with `export_params: None`, so no export resolution
    /// runs there anyway).
    pub wait_aliases: &'a [carina_core::binding_index::WaitAliasSpec],
    /// Pre-resolve snapshot of every `Composition` in the
    /// configuration: each one carries its **authored**
    /// `ResourceRef` attribute values (e.g. `role_arn = role.arn`),
    /// not the pre-apply-resolved concrete values.
    ///
    /// `apply` mutates a working copy of `sorted_resources`
    /// in-place via `resolve_refs_with_state_and_remote` at the head
    /// of the pipeline, which collapses every `ResourceRef` —
    /// including the ones inside composition `attributes` — into
    /// pre-apply concrete values. By the time `finalize_apply`
    /// runs, the resolved copy holds the *pre-apply* values, and a
    /// post-apply re-resolve has nothing to chase.
    ///
    /// This field carries the unresolved snapshot taken **before**
    /// that head-of-pipeline pass, so the export-resolution path
    /// (#3169 / #3177) can re-resolve compositions against the
    /// post-apply state. Empty for the `apply --plan` path, where
    /// `export_params` is also `None` and no export resolution
    /// runs.
    pub pre_resolve_compositions: &'a [carina_core::resource::Composition],
}

/// Export writeback result split by per-export outcome.
///
/// This deliberately avoids the all-or-nothing `HashMap` shape that
/// caused carina#3551: a partially failed apply can leave exports that
/// depend on the failed resource unresolved, but carina#3498 requires
/// successfully completed resources from the same run to still be
/// persisted. Resolved exports are written to `state.exports`; skipped
/// exports are omitted and surfaced to the operator by the caller.
pub(crate) struct ExportResolution {
    /// Exports that resolved to concrete JSON and are safe to persist.
    resolved: HashMap<String, serde_json::Value>,
    /// Exports omitted because their value still depends on unresolved
    /// apply-time data.
    skipped: Vec<SkippedExport>,
}

impl ExportResolution {
    /// Persist export resolution into `state.exports` and emit one
    /// operator-visible stdout line per omitted export.
    ///
    /// This is a three-way merge: resolved exports win with their new
    /// values, skipped exports preserve any prior persisted value, and
    /// names absent from both sets are dropped so source-side export
    /// removals still converge (carina#3551, carina#2932).
    ///
    /// Consumes `self` so the skipped diagnostics cannot be silently
    /// dropped by a caller that reads only the resolved half
    /// (carina#3551 / CLAUDE.md "Long-term view alongside root-cause").
    pub(crate) fn write_into(self, state: &mut StateFile) {
        let mut next = HashMap::new();
        for skipped in &self.skipped {
            println!("{}", render_skipped(skipped));
            if let Some(prior) = state.exports.get(&skipped.name) {
                next.insert(skipped.name.clone(), prior.clone());
            }
        }
        for (name, value) in self.resolved {
            next.insert(name, value);
        }
        state.exports = next;
    }

    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (HashMap<String, serde_json::Value>, Vec<SkippedExport>) {
        (self.resolved, self.skipped)
    }
}

fn render_skipped(skipped: &SkippedExport) -> String {
    format!(
        "  {} export {} not written: {}",
        "!".yellow(),
        skipped.name,
        skipped.reason
    )
}

/// Diagnostic for an export omitted during state writeback.
///
/// carina#3551 showed that unresolved sibling-resource references must
/// not abort the whole writeback after carina#3498 made partial apply
/// persistence meaningful. The reason is pre-formatted diagnostic text
/// for the operator-visible progress log on stdout, not data intended
/// for programmatic matching.
#[derive(Debug)]
pub(crate) struct SkippedExport {
    /// Export name from the source `exports {}` block.
    name: String,
    /// Pre-formatted human reason for the omission. This is
    /// diagnostic data for the progress log on stdout, not a stable
    /// machine contract.
    reason: String,
}

/// Resolve export expressions using bindings built from applied state.
///
/// `sorted_resources` carries the in-memory resource graph including any
/// composition resources synthesised by module-call expansion
/// (`expand_module_call`). Virtual resources are not persisted to
/// `state.resources` because they have no provider-side identity, so a
/// writeback that consults `state.resources` alone misses module-call
/// bindings — a downstream `exports { x = my_module_call.attr }` then
/// fails with `unresolved reference my_module_call.attr` even though
/// `carina plan` rendered the value cleanly. Issue #2479.
///
/// # Post-apply composition re-resolution (#3169)
///
/// Before #3177 this function fed `sorted_resources` directly into
/// `from_resources_with_state`. That captured each composition's
/// **pre-apply** attribute snapshot — for a composition whose
/// `attributes.role_arn = role.arn` and a managed `role` that was
/// `Replace`d during apply, the pre-apply `role.arn` was the
/// *old* ARN. The writeback path then wrote that stale ARN into
/// `state.exports`, even though `state.resources[role].attributes.arn`
/// already held the new ARN. Issue #3169.
///
/// The fix splits `sorted_resources` by kind and re-resolves composition
/// attributes against the post-apply view before exports use them:
///
/// 1. Build `post_apply_states` by starting from the live
///    `current_states` (so data-source read results survive — see
///    carina#3266) and overlaying entries derived from
///    `state.resources` (managed resources' applied attributes win
///    over any pre-apply snapshot left in `current_states`).
/// 2. Split `sorted_resources` into a managed view (Managed +
///    DataSource collapsed onto `Resource` by field projection)
///    and a composition view ([`Composition::try_from`]).
/// 3. Build the bindings view from the managed slice via
///    [`ResolvedBindings::from_managed_with_state`] (#3176).
/// 4. Re-resolve each composition's `ResourceRef`s against that view via
///    [`resolve_virtual_refs_post_apply`] (#3175), so a composition that
///    references a Replaced managed gets the *post*-replace value.
/// 5. Layer the re-resolved compositions onto the bindings view via
///    `layer_compositions_post_apply` (#3176).
/// 6. Resolve export expressions against the combined view.
///
/// # Partial-apply tolerance (carina#3551 / carina#3498)
///
/// Export expressions can reference a resource attribute that remains
/// unresolved because that resource's Create or Update failed while
/// earlier resources in the same apply succeeded. Those per-export
/// resolution failures are reported in [`ExportResolution::skipped`]
/// and omitted from `state.exports` so the writeback still persists
/// completed resources. Genuine serialization corruption, such as a
/// non-finite float, remains an error and aborts writeback.
pub(crate) fn resolve_exports(
    export_params: &[carina_core::parser::InferredExportParam],
    sorted_resources: &[Resource],
    data_sources: &[carina_core::resource::DataSource],
    pre_resolve_compositions: &[carina_core::resource::Composition],
    post_apply_states: &PostApplyStates,
    wait_aliases: &[carina_core::binding_index::WaitAliasSpec],
) -> Result<ExportResolution, AppError> {
    use carina_core::binding_index::ResolvedBindings;
    use carina_core::resolver::resolve_virtual_refs_post_apply;
    use carina_core::value::SerializationError;

    // Step 1: the post-apply state view was assembled by
    // [`PostApplyStates::from_current_and_state`] before this call.
    // See that type's doc-comment for the merge rule that pins
    // carina#3266 (data-source survives) and carina#3271 (state
    // refresh path uses the same shape).

    // compositions: fresh clone of the **pre-resolve** snapshot. Their
    // attributes still carry `ResourceRef`s, so the post-apply
    // resolver below can pick up the post-apply state values for
    // any managed sibling that was Replaced during apply
    // (#3169 / #3177).
    let mut compositions: Vec<carina_core::resource::Composition> =
        pre_resolve_compositions.to_vec();

    // Step 3: build the bindings view from the managed slice plus
    // post-apply states, **with data sources but no compositions yet**.
    // The compositions' attribute maps still hold pre-apply
    // `ResourceRef`s and need to be re-resolved against the
    // post-apply state in Step 4; only after that re-resolution can
    // they be layered into the bindings view. We use `pre_apply`
    // with `compositions: &[]` here, then call
    // `layer_compositions_post_apply` once the re-resolution is done.
    // (carina#3181, carina#3248)
    let mut bindings = ResolvedBindings::pre_apply(carina_core::binding_index::PreApplyInputs {
        managed: sorted_resources,
        compositions: &[],
        data_sources,
        current_states: post_apply_states.as_map(),
        remote_bindings: &HashMap::new(),
        wait_aliases,
    });

    // Step 4: re-resolve each composition's attributes against the
    // post-apply bindings view. After this, a composition's
    // `role_arn = role.arn` no longer holds the pre-apply
    // `ResourceRef`; it holds the post-apply concrete value.
    resolve_virtual_refs_post_apply(&mut compositions, &bindings)?;

    // Step 5: layer the re-resolved compositions onto the bindings view.
    // Now `exports { foo = some_module.role_arn }` resolves against
    // a binding whose `role_arn` is the post-apply value.
    bindings.layer_compositions_post_apply(&compositions)?;

    // Step 6: resolve the export expressions against the combined
    // view.
    let mut resolved_exports = HashMap::new();
    let mut skipped = Vec::new();
    for param in export_params {
        if let Some(ref value) = param.value {
            let resolved = crate::commands::plan::resolve_export_value(value, &bindings);
            match dsl_value_to_json(&resolved) {
                Ok(Some(json)) => {
                    resolved_exports.insert(param.name.clone(), json);
                }
                Ok(None) => {}
                Err(SerializationError::UnresolvedResourceRef { path, .. }) => {
                    skipped.push(SkippedExport {
                        name: param.name.clone(),
                        reason: format!("unresolved reference {path}"),
                    });
                }
                Err(SerializationError::UnknownNotAllowed { reason, .. }) => {
                    skipped.push(SkippedExport {
                        name: param.name.clone(),
                        reason: format!("value not yet known ({reason})"),
                    });
                }
                Err(SerializationError::UnresolvedInterpolation { .. }) => {
                    skipped.push(SkippedExport {
                        name: param.name.clone(),
                        reason: "unresolved interpolation".into(),
                    });
                }
                Err(SerializationError::UnresolvedFunctionCall { name, .. }) => {
                    skipped.push(SkippedExport {
                        name: param.name.clone(),
                        reason: format!("unresolved function call {name}(...)"),
                    });
                }
                Err(e @ SerializationError::NonFiniteFloat { .. }) => return Err(e.into()),
            }
        }
    }
    Ok(ExportResolution {
        resolved: resolved_exports,
        skipped,
    })
}

/// Convert a DSL Value to a serde_json::Value for state persistence.
///
/// Returns:
/// - `Ok(Some(json))` for a representable concrete value
/// - `Ok(None)` for `Value::Deferred(DeferredValue::Secret)` only —
///   `state.exports` must not embed plaintext secrets, so exports of
///   secret-typed values are skipped silently. No other variant uses
///   this skip path.
/// - `Err(SerializationError)` for variants that should not have
///   reached this boundary — the resolver / canonicalize / for-expand
///   pass should have eliminated them — and for non-finite floats
///   (`NonFiniteFloat`) which JSON cannot represent. Surfacing as Err
///   names the specific bug instead of silently losing the export.
pub(crate) fn dsl_value_to_json(
    value: &carina_core::resource::Value,
) -> Result<Option<serde_json::Value>, carina_core::value::SerializationError> {
    use carina_core::resource::{ConcreteValue, DeferredValue, Value};
    use carina_core::value::{SerializationContext, SerializationError};
    let ctx = SerializationContext::StateWriteback;
    match value {
        Value::Concrete(ConcreteValue::String(s)) => Ok(Some(serde_json::Value::String(s.clone()))),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => {
            Ok(Some(serde_json::Value::String(s.to_string())))
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => {
            Ok(Some(carina_core::value::canonical_enum_to_json(c)))
        }
        Value::Concrete(ConcreteValue::Bool(b)) => Ok(Some(serde_json::Value::Bool(*b))),
        Value::Concrete(ConcreteValue::Int(i)) => Ok(Some(serde_json::Value::Number((*i).into()))),
        Value::Concrete(ConcreteValue::Float(f)) => {
            // Non-finite floats (NaN / +inf / -inf) cannot be represented
            // in JSON. Surface as `NonFiniteFloat` rather than mapping to
            // `Ok(None)` — the `Ok(None)` contract is reserved for
            // `Value::Deferred(DeferredValue::Secret)` skipping, and
            // silently dropping a non-finite export would hide a real
            // upstream bug. Mirrors `value_to_json_with_context` in
            // `carina-core/src/value.rs`. (#2859)
            let num =
                serde_json::Number::from_f64(*f).ok_or(SerializationError::NonFiniteFloat {
                    value: *f,
                    context: ctx,
                })?;
            Ok(Some(serde_json::Value::Number(num)))
        }
        Value::Concrete(ConcreteValue::Duration(d)) => {
            Ok(Some(serde_json::Value::Number((d.as_secs() as i64).into())))
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            // `Result::transpose` flips `Result<Option<T>, E>` to
            // `Option<Result<T, E>>`, so `filter_map` drops the
            // `Ok(None)` skips and propagates `Err`.
            let json_items: Vec<_> = items
                .iter()
                .map(dsl_value_to_json)
                .filter_map(Result::transpose)
                .collect::<Result<_, _>>()?;
            Ok(Some(serde_json::Value::Array(json_items)))
        }
        Value::Concrete(ConcreteValue::StringList(items)) => Ok(Some(serde_json::Value::Array(
            items
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ))),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let json_map: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| dsl_value_to_json(v).map(|jv| jv.map(|j| (k.clone(), j))))
                .filter_map(Result::transpose)
                .collect::<Result<_, _>>()?;
            Ok(Some(serde_json::Value::Object(json_map)))
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            Err(SerializationError::UnknownNotAllowed {
                reason: reason.clone(),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            Err(SerializationError::UnresolvedResourceRef {
                path: path.to_dot_string(),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            Err(SerializationError::UnresolvedResourceRef {
                path: binding.clone(),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::Interpolation(_)) => {
            Err(SerializationError::UnresolvedInterpolation { context: ctx })
        }
        Value::Deferred(DeferredValue::FunctionCall { name, .. }) => {
            Err(SerializationError::UnresolvedFunctionCall {
                name: name.clone(),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::Secret(_)) => Ok(None),
    }
}

pub(crate) struct ApplyStateSave<'a> {
    pub state_file: Option<StateFile>,
    pub sorted_resources: &'a [Resource],
    pub current_states: &'a HashMap<ResourceId, State>,
    pub applied_states: &'a HashMap<ResourceId, State>,
    pub permanent_name_overrides: &'a HashMap<ResourceId, HashMap<String, String>>,
    pub plan: &'a Plan,
    pub successfully_deleted: &'a HashSet<ResourceId>,
    pub failed_refreshes: &'a HashSet<ResourceId>,
    pub schemas: &'a SchemaRegistry,
}

/// Typed plan of state-file writes computed from an apply result.
///
/// Enforces the invariant that **at most one write lands on any given
/// `ResourceId`**: `add_upsert` and `add_cleanup` reject overlapping
/// writes with `WritebackConflict`. The apply step then runs a single
/// non-overlapping pass over `upserts` and `cleanups`.
///
/// `Effect::Move` is a cleanup operation only — the `to` row's
/// contents always come from Phase 1's `add_upsert`. For
/// Move+Replace / Move+Update / Move+Create the `to` row is fed by
/// `applied_states[to]` (the provider's post-apply State); for a pure
/// rename, by `current_states[to]` (which `materialize_moved_states`
/// has already populated from the `from` row before writeback runs).
/// Either way, Move's only writeback responsibility is dropping the
/// stale `from` address. See carina#3170 for the bug class this
/// prevents.
pub(crate) struct WritebackPlan<'a> {
    upserts: indexmap::IndexMap<ResourceId, PlannedUpsert<'a>>,
    cleanups: HashSet<ResourceId>,
}

/// One planned upsert. Carrying the desired `&Resource` here (rather
/// than re-deriving it from `sorted_resources` in the apply loop)
/// makes the "every upsert has a desired resource" invariant
/// representable in the type — there is no separate lookup that can
/// miss.
struct PlannedUpsert<'a> {
    resource: &'a Resource,
    source: UpsertSource<'a>,
}

/// Source of the post-apply `State` that feeds the `to` row of a
/// planned upsert. `Applied` (provider returned a result) is gated to
/// receive `permanent_name_overrides`; `CurrentState` is not.
enum UpsertSource<'a> {
    Applied(&'a State),
    CurrentState(&'a State),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WritebackConflict {
    #[error(
        "writeback planned two upserts for the same resource id: {id} (apply pipeline produced overlapping post-apply states)"
    )]
    DuplicateUpsert { id: ResourceId },
    #[error(
        "writeback planned both an upsert and a cleanup for the same resource id: {id} (likely a moved-block `from` colliding with a desired-side resource)"
    )]
    UpsertCleanupOverlap { id: ResourceId },
}

impl From<WritebackConflict> for AppError {
    fn from(e: WritebackConflict) -> Self {
        AppError::Validation(e.to_string())
    }
}

impl<'a> WritebackPlan<'a> {
    fn new() -> Self {
        Self {
            upserts: indexmap::IndexMap::new(),
            cleanups: HashSet::new(),
        }
    }

    /// Register an upsert for `resource` with `source`. The first call
    /// wins for this id; subsequent calls return `DuplicateUpsert`.
    /// Calling after `add_cleanup` for the same id returns
    /// `UpsertCleanupOverlap`.
    fn add_upsert(
        &mut self,
        resource: &'a Resource,
        source: UpsertSource<'a>,
    ) -> Result<(), WritebackConflict> {
        let id = &resource.id;
        if self.cleanups.contains(id) {
            return Err(WritebackConflict::UpsertCleanupOverlap { id: id.clone() });
        }
        if self.upserts.contains_key(id) {
            return Err(WritebackConflict::DuplicateUpsert { id: id.clone() });
        }
        self.upserts
            .insert(id.clone(), PlannedUpsert { resource, source });
        Ok(())
    }

    /// Register a cleanup against `id`. Calling after `add_upsert(id)`
    /// returns `UpsertCleanupOverlap`. Cleanup is idempotent.
    fn add_cleanup(&mut self, id: ResourceId) -> Result<(), WritebackConflict> {
        if self.upserts.contains_key(&id) {
            return Err(WritebackConflict::UpsertCleanupOverlap { id });
        }
        self.cleanups.insert(id);
        Ok(())
    }
}

/// Build the typed writeback plan from the raw apply inputs.
///
/// Phase 1 (`sorted_resources` walk) registers upserts from
/// `applied_states` first, falling back to `current_states` when the
/// resource exists but had no provider call, and emitting cleanups
/// when the resource was absent post-refresh. Phase 2 (`plan.effects()`
/// walk) registers cleanups for `Delete` / `Remove` / `Move`'s `from`.
/// `Effect::Move` deliberately does **not** touch the `to` slot — that
/// is Phase 1's job.
fn decompose<'a>(
    sorted_resources: &'a [Resource],
    current_states: &'a HashMap<ResourceId, State>,
    applied_states: &'a HashMap<ResourceId, State>,
    plan: &Plan,
    successfully_deleted: &HashSet<ResourceId>,
    failed_refreshes: &HashSet<ResourceId>,
) -> Result<WritebackPlan<'a>, WritebackConflict> {
    let mut wb = WritebackPlan::new();

    for resource in sorted_resources {
        if let Some(applied) = applied_states.get(&resource.id) {
            wb.add_upsert(resource, UpsertSource::Applied(applied))?;
        } else if failed_refreshes.contains(&resource.id) {
            // Refresh failed; we don't know whether the live resource
            // still exists, so leave any pre-existing row untouched.
            continue;
        } else if let Some(current) = current_states.get(&resource.id) {
            if current.exists {
                wb.add_upsert(resource, UpsertSource::CurrentState(current))?;
            } else {
                wb.add_cleanup(resource.id.clone())?;
            }
        }
    }

    for effect in plan.effects() {
        match effect {
            Effect::Delete { id, .. } if successfully_deleted.contains(id) => {
                wb.add_cleanup(id.clone())?;
            }
            Effect::Remove { id } => {
                wb.add_cleanup(id.clone())?;
            }
            Effect::Move { from, .. } => {
                // Move's only writeback responsibility is to drop the
                // stale `from` row. The `to` row is owned by Phase 1
                // (`applied_states[to]` for Move + Replace/Update/Create,
                // or `current_states[to]` for pure rename, which
                // `materialize_moved_states` transferred from `from`
                // before writeback runs).
                wb.add_cleanup(from.clone())?;
            }
            _ => {}
        }
    }

    Ok(wb)
}

pub(crate) fn build_state_after_apply(save: ApplyStateSave<'_>) -> Result<StateFile, AppError> {
    let ApplyStateSave {
        state_file,
        sorted_resources,
        current_states,
        applied_states,
        permanent_name_overrides,
        plan,
        successfully_deleted,
        failed_refreshes,
        schemas,
    } = save;
    let mut state = state_file.unwrap_or_default();

    let writeback = decompose(
        sorted_resources,
        current_states,
        applied_states,
        plan,
        successfully_deleted,
        failed_refreshes,
    )?;

    for (id, planned) in &writeback.upserts {
        let resource = planned.resource;
        let existing = state.find_resource(&id.provider, &id.resource_type, id.name_str());
        let write_only_keys: Vec<String> = schemas
            .get_for(resource)
            .map(|schema| {
                schema
                    .attributes
                    .iter()
                    .filter(|(_, attr)| attr.write_only)
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();

        let (applied_state, is_applied) = match planned.source {
            UpsertSource::Applied(s) => (s, true),
            UpsertSource::CurrentState(s) => (s, false),
        };
        let mut resource_state =
            ResourceState::from_provider_state(resource, applied_state, existing)?;
        if is_applied && let Some(overrides) = permanent_name_overrides.get(id) {
            resource_state.name_overrides = overrides.clone();
        }
        if !write_only_keys.is_empty() {
            resource_state.merge_write_only_attributes(resource, &write_only_keys);
        }
        state.upsert_resource(resource_state);
    }

    for id in &writeback.cleanups {
        state.remove_resource(&id.provider, &id.resource_type, id.name_str());
    }

    Ok(state)
}

/// Apply destroy results to the state file: remove destroyed resources and
/// clear any exports (since exports reference attributes of destroyed resources).
pub(crate) fn apply_destroy_to_state(
    state: &mut carina_state::StateFile,
    destroyed_ids: &[ResourceId],
) {
    for id in destroyed_ids {
        state.remove_resource(&id.provider, &id.resource_type, id.name_str());
    }
    state.exports.clear();
}

/// Build a minimal `Resource` for an orphaned resource from the state file.
///
/// This creates a Resource with attributes reconstructed from state data,
/// including `_binding` and `_dependency_bindings` so that dependency ordering
/// and tree display work correctly.
pub(crate) fn build_orphan_resource(sf: &carina_state::StateFile, id: &ResourceId) -> Resource {
    let rs = sf
        .find_resource(&id.provider, &id.resource_type, id.name_str())
        .expect("orphan must exist in state file");
    let attributes: HashMap<String, Value> = rs
        .attributes
        .iter()
        .filter_map(|(k, v)| carina_core::value::json_to_dsl_value(v).map(|val| (k.clone(), val)))
        .collect();
    Resource {
        id: id.clone(),
        attributes: attributes.into_iter().collect(),
        directives: rs.directives.clone(),
        prefixes: rs.prefixes.clone(),
        binding: rs.binding.clone(),
        dependency_bindings: rs.dependency_bindings.clone(),
        module_source: None,
        quoted_string_attrs: std::collections::HashSet::new(),
    }
}

#[cfg(test)]
mod post_apply_states_tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, ResourceId, State, Value};
    use carina_state::StateFile;

    /// Type-level contract pin for the carina#3266 + carina#3271 merge rule.
    ///
    /// `from_current_and_state` must:
    /// 1. Keep entries that exist in `current_states` but **not** in
    ///    `state.resources` — data sources land here (they are never
    ///    persisted to `state.resources`).
    /// 2. For entries present in **both** maps, pick the one
    ///    derived from `state.resources` — that is the post-apply
    ///    writeback value, while `current_states` may still hold
    ///    the pre-apply snapshot.
    #[test]
    fn data_source_only_in_current_states_survives() {
        let ds_id = ResourceId::with_provider("aws", "iam.Roles", "admin_access_roles", None);
        let mut ds_attrs = HashMap::new();
        ds_attrs.insert(
            "arns".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("arn:aws:iam::1:role/x".to_string()),
            )])),
        );
        let mut current_states = HashMap::new();
        current_states.insert(
            ds_id.clone(),
            State::existing(ds_id.clone(), ds_attrs.clone()),
        );

        // state.resources has no entry for the data source — that is
        // the production reality the bug springs from.
        let state = StateFile::new();

        let post = PostApplyStates::from_current_and_state(&current_states, &state);
        let entry = post
            .as_map()
            .get(&ds_id)
            .expect("data-source entry must survive merge");
        assert_eq!(entry.attributes.get("arns"), ds_attrs.get("arns"));
    }

    #[test]
    fn state_resources_entry_wins_over_current_states_on_collision() {
        let id = ResourceId::with_provider("awscc", "s3.Bucket", "log", None);

        // current_states carries the PRE-apply attribute value.
        let mut pre_attrs = HashMap::new();
        pre_attrs.insert(
            "versioning".to_string(),
            Value::Concrete(ConcreteValue::String("Suspended".to_string())),
        );
        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), State::existing(id.clone(), pre_attrs));

        // state.resources carries the POST-apply value — this must win.
        let json = serde_json::json!({
            "version": 5,
            "serial": 1,
            "lineage": "test",
            "carina_version": "0.4.0",
            "resources": [
                {
                    "resource_type": "s3.Bucket",
                    "name": "log",
                    "identifier": "log-bucket",
                    "provider": "awscc",
                    "attributes": { "versioning": "Enabled" }
                }
            ]
        });
        let state: StateFile = serde_json::from_value(json).unwrap();

        let post = PostApplyStates::from_current_and_state(&current_states, &state);
        let entry = post.as_map().get(&id).expect("merged entry");
        match entry.attributes.get("versioning") {
            Some(Value::Concrete(ConcreteValue::String(s))) => assert_eq!(s, "Enabled"),
            other => panic!("expected post-apply 'Enabled', got: {other:?}"),
        }
    }
}

#[cfg(test)]
mod resolve_exports_tests {
    use super::*;
    use carina_core::parser::{InferredExportParam, TypeExpr};
    use carina_core::resource::{
        AccessPath, ConcreteValue, DeferredValue, InterpolationPart, Value,
    };
    use carina_state::StateFile;

    fn export_param(name: &str, value: Value) -> InferredExportParam {
        InferredExportParam {
            name: name.to_string(),
            type_expr: TypeExpr::Unknown,
            value: Some(value),
        }
    }

    fn resolve_export_parts(
        export_params: &[InferredExportParam],
    ) -> (HashMap<String, serde_json::Value>, Vec<SkippedExport>) {
        let state = StateFile::new();
        let post_apply_states = PostApplyStates::from_current_and_state(&HashMap::new(), &state);
        resolve_exports(export_params, &[], &[], &[], &post_apply_states, &[])
            .expect("unresolved exports should be skipped, not abort writeback")
            .into_parts()
    }

    #[test]
    fn unresolved_export_is_skipped_without_dropping_resolved_exports() {
        let export_params = vec![
            export_param(
                "target_group_arn",
                Value::Deferred(DeferredValue::ResourceRef {
                    path: AccessPath::with_fields(
                        "registry_publish",
                        "target_group",
                        vec!["arn".into()],
                    ),
                }),
            ),
            export_param(
                "ok_export",
                Value::Concrete(ConcreteValue::String("ok".into())),
            ),
        ];

        let (resolved, skipped) = resolve_export_parts(&export_params);

        assert_eq!(resolved.get("ok_export"), Some(&serde_json::json!("ok")));
        assert!(
            !resolved.contains_key("target_group_arn"),
            "unresolved export must be omitted from resolved map"
        );
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].name, "target_group_arn");
        assert!(
            skipped[0]
                .reason
                .contains("registry_publish.target_group.arn"),
            "skip reason should name unresolved path, got: {:?}",
            skipped[0]
        );
        assert_eq!(
            render_skipped(&skipped[0]),
            "  ! export target_group_arn not written: unresolved reference registry_publish.target_group.arn"
        );
    }

    #[test]
    fn multiple_unresolved_exports_are_skipped_in_export_order() {
        let export_params = vec![
            export_param(
                "first_missing",
                Value::Deferred(DeferredValue::ResourceRef {
                    path: AccessPath::new("first", "id"),
                }),
            ),
            export_param(
                "ok_export",
                Value::Concrete(ConcreteValue::String("ok".into())),
            ),
            export_param(
                "second_missing",
                Value::Deferred(DeferredValue::ResourceRef {
                    path: AccessPath::new("second", "id"),
                }),
            ),
        ];

        let (resolved, skipped) = resolve_export_parts(&export_params);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved.get("ok_export"), Some(&serde_json::json!("ok")));
        assert_eq!(skipped.len(), 2);
        assert_eq!(skipped[0].name, "first_missing");
        assert_eq!(skipped[1].name, "second_missing");
    }

    #[test]
    fn unresolved_interpolation_export_is_skipped() {
        let export_params = vec![export_param(
            "template",
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("x".into()),
            ])),
        )];

        let (resolved, skipped) = resolve_export_parts(&export_params);

        assert!(resolved.is_empty());
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].name, "template");
        assert_eq!(skipped[0].reason, "unresolved interpolation");
    }

    #[test]
    fn unresolved_function_call_export_is_skipped() {
        let export_params = vec![export_param(
            "joined",
            Value::Deferred(DeferredValue::FunctionCall {
                name: "join".into(),
                args: vec![],
            }),
        )];

        let (resolved, skipped) = resolve_export_parts(&export_params);

        assert!(resolved.is_empty());
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].name, "joined");
        assert_eq!(skipped[0].reason, "unresolved function call join(...)");
    }

    #[test]
    fn write_into_preserves_prior_skipped_exports_and_drops_orphans() {
        let mut state = StateFile::new();
        state
            .exports
            .insert("ax".to_string(), serde_json::json!("old-a"));
        state
            .exports
            .insert("bx".to_string(), serde_json::json!("old-b"));
        state
            .exports
            .insert("orphan".to_string(), serde_json::json!("old-orphan"));

        ExportResolution {
            resolved: HashMap::from([("ax".to_string(), serde_json::json!("new-a"))]),
            skipped: vec![SkippedExport {
                name: "bx".to_string(),
                reason: "unresolved reference b.name".to_string(),
            }],
        }
        .write_into(&mut state);

        assert_eq!(state.exports.len(), 2);
        assert_eq!(state.exports.get("ax"), Some(&serde_json::json!("new-a")));
        assert_eq!(state.exports.get("bx"), Some(&serde_json::json!("old-b")));
        assert!(!state.exports.contains_key("orphan"));
    }
}

#[cfg(test)]
mod stage4_unknown_err_tests {
    use super::*;
    use carina_core::resource::{AccessPath, ConcreteValue, DeferredValue, UnknownReason, Value};
    use carina_core::value::{SerializationContext, SerializationError};

    /// RFC #2371 stage 4 contract pin: `dsl_value_to_json` returns
    /// `Err(SerializationError::UnknownNotAllowed)` for `Value::Deferred(DeferredValue::Unknown)`.
    /// State files must never carry the variant (constraint b); a
    /// silent fallback would re-introduce v1 corruption.
    #[test]
    fn unknown_returns_err_in_dsl_value_to_json() {
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        let v = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        }));
        let err = dsl_value_to_json(&v).unwrap_err();
        match err {
            SerializationError::UnknownNotAllowed {
                reason: UnknownReason::UpstreamRef { path: p },
                context: SerializationContext::StateWriteback,
            } => assert_eq!(p, path),
            other => {
                panic!("expected UnknownNotAllowed/UpstreamRef/StateWriteback, got: {other:?}")
            }
        }
    }

    /// `Value::Deferred(DeferredValue::ResourceRef)` reaching apply-time export resolution
    /// is a resolver bug — surface as `UnresolvedResourceRef` instead
    /// of silently dropping the export. (#2385)
    #[test]
    fn resource_ref_returns_unresolved_err() {
        let v = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::with_fields("net", "vpc", vec!["vpc_id".into()]),
        });
        let err = dsl_value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                &err,
                SerializationError::UnresolvedResourceRef {
                    path,
                    context: SerializationContext::StateWriteback,
                } if path == "net.vpc.vpc_id"
            ),
            "expected UnresolvedResourceRef/net.vpc.vpc_id/StateWriteback, got: {err:?}"
        );
    }

    /// `Value::Deferred(DeferredValue::Interpolation)` reaching apply-time is a resolver /
    /// canonicalize bug — surface as `UnresolvedInterpolation`. (#2386)
    #[test]
    fn interpolation_returns_unresolved_err() {
        use carina_core::resource::InterpolationPart;
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("x".into()),
        ]));
        let err = dsl_value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                &err,
                SerializationError::UnresolvedInterpolation {
                    context: SerializationContext::StateWriteback,
                }
            ),
            "expected UnresolvedInterpolation/StateWriteback, got: {err:?}"
        );
    }

    /// `Value::Deferred(DeferredValue::FunctionCall)` reaching apply-time is a resolver bug —
    /// surface as `UnresolvedFunctionCall`. (#2386)
    #[test]
    fn function_call_returns_unresolved_err() {
        let v = Value::Deferred(DeferredValue::FunctionCall {
            name: "join".into(),
            args: vec![],
        });
        let err = dsl_value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                &err,
                SerializationError::UnresolvedFunctionCall {
                    name,
                    context: SerializationContext::StateWriteback,
                } if name == "join"
            ),
            "expected UnresolvedFunctionCall/join/StateWriteback, got: {err:?}"
        );
    }

    /// `Value::Deferred(DeferredValue::Secret)` continues to be skipped silently — exports must
    /// not embed plaintext secrets in state.
    #[test]
    fn secret_returns_ok_none() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("password".into()),
        ))));
        assert!(matches!(dsl_value_to_json(&v), Ok(None)));
    }

    #[test]
    fn canonical_enum_serializes_as_typed_object() {
        let attr_type = carina_core::schema::AttributeType::enum_(
            carina_core::schema::TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
            Some(vec!["vpc".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let canonical = carina_core::resource::EnumValueResolver::new(&attr_type)
            .resolve_state_text("vpc")
            .unwrap();
        let v = Value::Concrete(ConcreteValue::CanonicalEnum(canonical));

        assert_eq!(
            dsl_value_to_json(&v).unwrap(),
            Some(serde_json::json!({
                "Enum": {
                    "identity": {
                        "provider": "aws",
                        "segments": ["ec2", "Eip"],
                        "kind": "Domain"
                    },
                    "api_value": "vpc"
                }
            }))
        );
    }

    /// Non-finite floats (NaN / +inf / -inf) cannot be represented in
    /// JSON. `dsl_value_to_json` must surface them as
    /// `SerializationError::NonFiniteFloat` rather than mapping to
    /// `Ok(None)`, which would silently drop the export. The
    /// `Ok(None)` skip path is reserved for `DeferredValue::Secret`.
    /// (#2859)
    #[test]
    fn non_finite_float_returns_err() {
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let v = Value::Concrete(ConcreteValue::Float(f));
            let err = dsl_value_to_json(&v).unwrap_err();
            match err {
                SerializationError::NonFiniteFloat {
                    value,
                    context: SerializationContext::StateWriteback,
                } => {
                    // NaN != NaN, so compare via classification.
                    assert_eq!(
                        value.is_nan(),
                        f.is_nan(),
                        "NaN classification mismatch for {f}"
                    );
                    assert_eq!(
                        value.is_infinite(),
                        f.is_infinite(),
                        "infinite classification mismatch for {f}"
                    );
                    if f.is_infinite() {
                        assert_eq!(
                            value.is_sign_negative(),
                            f.is_sign_negative(),
                            "sign mismatch for {f}"
                        );
                    }
                }
                other => panic!("expected NonFiniteFloat/StateWriteback for {f}, got: {other:?}"),
            }
        }
    }
}
