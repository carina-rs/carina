//! State write-back helpers shared between `apply` and `destroy`.
//!
//! These helpers translate the in-memory execution result into a persisted
//! `StateFile`: applying name overrides, building the post-apply state,
//! resolving exports, and (for destroy) removing destroyed resources.

use std::collections::{BTreeSet, HashMap, HashSet};

use carina_core::effect::{DeferredReplaceDelete, DeletedInstanceKey, Effect, EffectGeneration};
use carina_core::executor::ExecutionResult;
use carina_core::plan::{Plan, ReplaceDisplayInfo};
use carina_core::resource::{ConcreteValue, Resource, ResourceId, State, Value};
use carina_core::schema::{ResourceSchema, SchemaRegistry};
use carina_state::{DeposedInstance, DeposedKey, LockInfo, ResourceState, StateBackend, StateFile};
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
            let id = ResourceId::with_provider_name_compat(
                &rs.provider,
                &rs.resource_type,
                &rs.identity,
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

    /// Borrow the underlying storage-form map. Callers that feed this
    /// into binding resolution must convert it through
    /// `into_plan_input_map` at that boundary.
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
    schemas: &SchemaRegistry,
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
    let plan_input_states = carina_core::resource::into_plan_input_map(
        post_apply_states.as_map().clone(),
        schemas,
        sorted_resources,
    );
    let mut bindings = ResolvedBindings::pre_apply(carina_core::binding_index::PreApplyInputs {
        managed: sorted_resources,
        compositions: &[],
        data_sources,
        current_states: &plan_input_states,
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
    pub runtime_synthesized_resources: &'a [Resource],
    pub current_states: &'a HashMap<ResourceId, State>,
    pub applied_states: &'a HashMap<ResourceId, State>,
    pub plan: &'a Plan,
    pub successfully_deleted: &'a HashSet<DeletedInstanceKey>,
    pub failed_refreshes: &'a HashSet<ResourceId>,
    pub schemas: &'a SchemaRegistry,
}

/// Typed plan of state-file writes computed from an apply result.
///
/// Enforces the invariant that **upsert and cleanup are mutually
/// exclusive for any given `ResourceId`**: `add_upsert` and
/// `add_cleanup` reject overlapping writes with `WritebackConflict`.
/// Deposes intentionally coexist with an upsert, are applied after
/// upserts and before cleanups, and are merged by identifier plus
/// provider-instance routing.
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
    deposes: Vec<PlannedDepose>,
    remove_deposed: Vec<PlannedRemoveDeposed>,
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

impl<'a> UpsertSource<'a> {
    fn state(&self) -> &'a State {
        match self {
            Self::Applied(state) | Self::CurrentState(state) => state,
        }
    }

    fn is_applied(&self) -> bool {
        matches!(self, Self::Applied(_))
    }
}

struct PlannedDepose {
    id: ResourceId,
    instance: DeposedInstance,
}

struct PlannedRemoveDeposed {
    id: ResourceId,
    key: DeposedKey,
}

enum ReplacementOutcome {
    CurrentRowOnly,
    CreateAppliedDeposeOld(PlannedDepose),
    DeleteSucceededCreateAbsent(ResourceId),
    KeepOld,
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
            deposes: Vec::new(),
            remove_deposed: Vec::new(),
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

    fn apply_replacement_outcome(
        &mut self,
        outcome: ReplacementOutcome,
    ) -> Result<(), WritebackConflict> {
        match outcome {
            ReplacementOutcome::CurrentRowOnly | ReplacementOutcome::KeepOld => Ok(()),
            ReplacementOutcome::CreateAppliedDeposeOld(depose) => {
                self.deposes.push(depose);
                Ok(())
            }
            ReplacementOutcome::DeleteSucceededCreateAbsent(id) => {
                self.upserts.shift_remove(&id);
                self.add_cleanup(id)
            }
        }
    }

    fn add_remove_deposed(&mut self, id: ResourceId, key: DeposedKey) {
        self.remove_deposed.push(PlannedRemoveDeposed { id, key });
    }
}

struct ReplacementDeleteInput<'a> {
    id: &'a ResourceId,
    identifier: &'a str,
    provider_instance: Option<String>,
    generation: &'a EffectGeneration,
    fallback_dependency_bindings: BTreeSet<String>,
    previous_attributes: Option<&'a HashMap<String, Value>>,
}

static CURRENT_GENERATION: EffectGeneration = EffectGeneration::Current;

fn delete_succeeded(
    successfully_deleted: &HashSet<DeletedInstanceKey>,
    id: &ResourceId,
    generation: &EffectGeneration,
    identifier: &str,
) -> bool {
    successfully_deleted.contains(&DeletedInstanceKey::new(
        id.clone(),
        generation.clone(),
        identifier.to_string(),
    ))
}

fn identifier_was_successfully_deleted(
    successfully_deleted: &HashSet<DeletedInstanceKey>,
    id: &ResourceId,
    provider_instance: &Option<String>,
    identifier: &str,
) -> bool {
    let mut delete_id = id.clone();
    delete_id.provider_instance = provider_instance.clone();
    successfully_deleted.contains(&DeletedInstanceKey::current(
        delete_id,
        identifier.to_string(),
    ))
}

fn classify_replacement_outcome(
    delete: ReplacementDeleteInput<'_>,
    create_upsert: Option<&PlannedUpsert<'_>>,
    state_file: &StateFile,
    current_states: &HashMap<ResourceId, State>,
    successfully_deleted: &HashSet<DeletedInstanceKey>,
    schemas: &SchemaRegistry,
) -> ReplacementOutcome {
    let applied_create = create_upsert.filter(|upsert| upsert.source.is_applied());
    match applied_create {
        Some(_)
            if delete_succeeded(
                successfully_deleted,
                delete.id,
                delete.generation,
                delete.identifier,
            ) =>
        {
            ReplacementOutcome::CurrentRowOnly
        }
        Some(upsert) => {
            let new_identifier = upsert.source.state().identifier.as_deref();
            if new_identifier == Some(delete.identifier) {
                return ReplacementOutcome::CurrentRowOnly;
            }
            ReplacementOutcome::CreateAppliedDeposeOld(planned_depose(
                delete,
                upsert.resource,
                schemas.get_for(upsert.resource),
                state_file,
                current_states,
            ))
        }
        None if delete_succeeded(
            successfully_deleted,
            delete.id,
            delete.generation,
            delete.identifier,
        ) =>
        {
            ReplacementOutcome::DeleteSucceededCreateAbsent(delete.id.clone())
        }
        None => ReplacementOutcome::KeepOld,
    }
}

fn planned_depose(
    delete: ReplacementDeleteInput<'_>,
    desired_resource: &Resource,
    schema: Option<&ResourceSchema>,
    state_file: &StateFile,
    current_states: &HashMap<ResourceId, State>,
) -> PlannedDepose {
    let existing = state_file.find_resource(
        &delete.id.provider,
        &delete.id.resource_type,
        delete.id.identity_or_empty(),
    );
    let current = current_states.get(delete.id);

    let attributes = if let Some(existing) = existing
        && existing.identifier.as_deref() == Some(delete.identifier)
    {
        // Re-merging desired secrets can store the new secret's hash
        // on the old snapshot. That is the safe direction: hashes are
        // compare-only, and this also masks keys that only became
        // secret in the new config.
        ResourceState::protect_state_json_for_resource_and_schema(
            desired_resource,
            schema,
            existing.attributes.clone(),
        )
    } else {
        if let Some(previous) = delete.previous_attributes {
            ResourceState::attributes_to_state_json_lossy_for_resource_and_schema(
                desired_resource,
                schema,
                previous,
            )
        } else if let Some(current) = current {
            ResourceState::attributes_to_state_json_lossy_for_resource_and_schema(
                desired_resource,
                schema,
                &current.attributes,
            )
        } else {
            HashMap::new()
        }
    };

    let dependency_bindings = existing
        .map(|rs| rs.dependency_bindings.clone())
        .filter(|bindings| !bindings.is_empty())
        .or_else(|| current.map(|state| state.dependency_bindings.clone()))
        .filter(|bindings| !bindings.is_empty())
        .unwrap_or(delete.fallback_dependency_bindings);

    PlannedDepose {
        id: delete.id.clone(),
        instance: DeposedInstance {
            key: DeposedKey::new_unique(),
            identifier: delete.identifier.to_string(),
            provider_instance: delete.provider_instance,
            attributes,
            dependency_bindings,
        },
    }
}

fn delete_input_from_effect<'a>(
    effect: &'a Effect,
    replacement: &'a ReplaceDisplayInfo<'a>,
) -> Option<ReplacementDeleteInput<'a>> {
    let Effect::Delete {
        id,
        identifier,
        generation,
        dependencies,
        ..
    } = effect
    else {
        return None;
    };
    Some(ReplacementDeleteInput {
        id: id.as_inner(),
        identifier,
        provider_instance: id.as_inner().provider_instance.clone(),
        generation,
        fallback_dependency_bindings: dependencies.iter().cloned().collect(),
        previous_attributes: Some(replacement.previous_attributes),
    })
}

fn delete_input_from_deferred(delete: &DeferredReplaceDelete) -> ReplacementDeleteInput<'_> {
    ReplacementDeleteInput {
        id: delete.id.as_inner(),
        identifier: &delete.identifier,
        provider_instance: delete.id.as_inner().provider_instance.clone(),
        generation: &CURRENT_GENERATION,
        fallback_dependency_bindings: delete.dependencies.iter().cloned().collect(),
        previous_attributes: None,
    }
}

fn create_side_upsert_ids(plan: &Plan) -> HashSet<ResourceId> {
    let mut ids = HashSet::new();
    for effect in plan.effects() {
        match effect {
            Effect::Create(resource) => {
                ids.insert(resource.id.clone());
            }
            Effect::DeferredReplace(payload) => {
                ids.extend(
                    payload
                        .deletes
                        .iter()
                        .map(|delete| delete.id.clone().into_inner()),
                );
            }
            _ => {}
        }
    }
    ids
}

fn plan_displaced_identifier_deposes(
    wb: &mut WritebackPlan<'_>,
    state_file: &StateFile,
    current_states: &HashMap<ResourceId, State>,
    plan: &Plan,
    successfully_deleted: &HashSet<DeletedInstanceKey>,
    failed_refreshes: &HashSet<ResourceId>,
    schemas: &SchemaRegistry,
) {
    let create_side_ids = create_side_upsert_ids(plan);
    for (id, upsert) in &wb.upserts {
        if !upsert.source.is_applied() {
            continue;
        }
        if !create_side_ids.contains(id) {
            continue;
        }
        let Some(existing) =
            state_file.find_resource(&id.provider, &id.resource_type, id.identity_or_empty())
        else {
            continue;
        };
        let Some(displaced_identifier) = existing.identifier.as_deref() else {
            continue;
        };
        if !failed_refreshes.contains(id)
            && current_states.get(id).is_some_and(|state| !state.exists)
        {
            continue;
        }
        let new_identifier = upsert.source.state().identifier.as_deref();
        if new_identifier == Some(displaced_identifier) {
            continue;
        }
        if identifier_was_successfully_deleted(
            successfully_deleted,
            id,
            &existing.directives.provider_instance,
            displaced_identifier,
        ) {
            continue;
        }
        if wb.deposes.iter().any(|depose| {
            &depose.id == id
                && depose.instance.identifier == displaced_identifier
                && depose.instance.provider_instance == existing.directives.provider_instance
        }) {
            continue;
        }

        wb.deposes.push(planned_depose(
            ReplacementDeleteInput {
                id,
                identifier: displaced_identifier,
                provider_instance: existing.directives.provider_instance.clone(),
                generation: &EffectGeneration::Current,
                fallback_dependency_bindings: existing.dependency_bindings.clone(),
                previous_attributes: None,
            },
            upsert.resource,
            schemas.get_for(upsert.resource),
            state_file,
            current_states,
        ));
    }
}

/// Build the typed writeback plan from the raw apply inputs.
///
/// Phase 1 (`sorted_resources` walk) registers upserts from
/// `applied_states` first, falling back to `current_states` when the
/// resource exists but had no provider call, and emitting cleanups
/// when the resource was absent post-refresh. Phase 2 (`plan.effects()`
/// walk) classifies replacement outcomes for decomposed replacements
/// and `DeferredReplace`, then registers ordinary cleanups for
/// `Delete` / `Remove` / `Move`'s `from`. `Effect::Move` deliberately
/// does **not** touch the `to` slot — that is Phase 1's job.
struct DecomposeInput<'a, 's> {
    state_file: &'s StateFile,
    sorted_resources: &'a [Resource],
    runtime_synthesized_resources: &'a [Resource],
    current_states: &'a HashMap<ResourceId, State>,
    applied_states: &'a HashMap<ResourceId, State>,
    plan: &'a Plan,
    successfully_deleted: &'a HashSet<DeletedInstanceKey>,
    failed_refreshes: &'a HashSet<ResourceId>,
    schemas: &'a SchemaRegistry,
}

fn decompose<'a>(input: DecomposeInput<'a, '_>) -> Result<WritebackPlan<'a>, WritebackConflict> {
    let DecomposeInput {
        state_file,
        sorted_resources,
        runtime_synthesized_resources,
        current_states,
        applied_states,
        plan,
        successfully_deleted,
        failed_refreshes,
        schemas,
    } = input;
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

    for resource in runtime_synthesized_resources {
        if let Some(applied) = applied_states.get(&resource.id) {
            wb.add_upsert(resource, UpsertSource::Applied(applied))?;
        }
    }

    let replacement_by_delete_idx: HashMap<usize, ReplaceDisplayInfo<'_>> = plan
        .replace_display_info()
        .map(|replacement| (replacement.delete_idx, replacement))
        .collect();

    for (idx, effect) in plan.effects().iter().enumerate() {
        if let Some(replacement) = replacement_by_delete_idx.get(&idx) {
            if let Some(delete) = delete_input_from_effect(effect, replacement) {
                let create_id = plan.effects()[replacement.create_idx].resource_id();
                let outcome = classify_replacement_outcome(
                    delete,
                    wb.upserts.get(create_id),
                    state_file,
                    current_states,
                    successfully_deleted,
                    schemas,
                );
                wb.apply_replacement_outcome(outcome)?;
            }
            continue;
        }

        if let Effect::DeferredReplace(payload) = effect {
            for delete in &payload.deletes {
                let delete = delete_input_from_deferred(delete);
                let create_upsert = wb.upserts.get(delete.id);
                let outcome = classify_replacement_outcome(
                    delete,
                    create_upsert,
                    state_file,
                    current_states,
                    successfully_deleted,
                    schemas,
                );
                wb.apply_replacement_outcome(outcome)?;
            }
            continue;
        }

        if let Effect::Delete {
            id,
            identifier,
            generation: generation @ EffectGeneration::Deposed(key),
            ..
        } = effect
            && delete_succeeded(successfully_deleted, id.as_inner(), generation, identifier)
        {
            wb.add_remove_deposed(id.clone().into_inner(), key.clone());
        }

        for id in effect.writeback_cleanup_ids(successfully_deleted) {
            wb.add_cleanup(id)?;
        }
    }

    plan_displaced_identifier_deposes(
        &mut wb,
        state_file,
        current_states,
        plan,
        successfully_deleted,
        failed_refreshes,
        schemas,
    );

    Ok(wb)
}

pub(crate) fn build_state_after_apply(save: ApplyStateSave<'_>) -> Result<StateFile, AppError> {
    let ApplyStateSave {
        state_file,
        sorted_resources,
        runtime_synthesized_resources,
        current_states,
        applied_states,
        plan,
        successfully_deleted,
        failed_refreshes,
        schemas,
    } = save;
    let mut state = state_file.unwrap_or_default();

    let writeback = decompose(DecomposeInput {
        state_file: &state,
        sorted_resources,
        runtime_synthesized_resources,
        current_states,
        applied_states,
        plan,
        successfully_deleted,
        failed_refreshes,
        schemas,
    })?;
    let permanent_name_overrides = plan.permanent_name_overrides_for_state();

    for (id, planned) in &writeback.upserts {
        let resource = planned.resource;
        let existing = state.find_resource(&id.provider, &id.resource_type, id.identity_or_empty());
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

    for planned in writeback.deposes {
        apply_planned_depose(&mut state, planned);
    }

    for planned in writeback.remove_deposed {
        state.remove_deposed_generation(
            &planned.id.provider,
            &planned.id.resource_type,
            planned.id.identity_or_empty(),
            &planned.key,
        );
    }

    for id in &writeback.cleanups {
        state.remove_resource(&id.provider, &id.resource_type, id.identity_or_empty());
    }

    Ok(state)
}

fn apply_planned_depose(state: &mut StateFile, planned: PlannedDepose) {
    let row_provider_instance = planned.instance.provider_instance.clone();
    state.upsert_deposed_generation(
        &planned.id.provider,
        &planned.id.resource_type,
        planned.id.identity_or_empty(),
        row_provider_instance,
        planned.instance,
    );
}

/// Apply destroy results to the state file: remove destroyed resources and
/// clear any exports (since exports reference attributes of destroyed resources).
pub(crate) fn apply_destroy_to_state(
    state: &mut carina_state::StateFile,
    destroyed: &[DestroyedInstance],
) {
    for destroyed in destroyed {
        match &destroyed.generation {
            EffectGeneration::Current => {
                state.remove_resource(
                    &destroyed.id.provider,
                    &destroyed.id.resource_type,
                    destroyed.id.identity_or_empty(),
                );
            }
            EffectGeneration::Deposed(key) => {
                state.remove_deposed_generation(
                    &destroyed.id.provider,
                    &destroyed.id.resource_type,
                    destroyed.id.identity_or_empty(),
                    key,
                );
            }
        }
    }
    state.exports.clear();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DestroyedInstance {
    pub id: ResourceId,
    pub generation: EffectGeneration,
}

/// Build a minimal `Resource` for an orphaned resource from the state file.
///
/// This creates a Resource with attributes reconstructed from state data,
/// including `_binding` and `_dependency_bindings` so that dependency ordering
/// and tree display work correctly.
pub(crate) fn build_orphan_resource(sf: &carina_state::StateFile, id: &ResourceId) -> Resource {
    let rs = sf
        .find_resource(&id.provider, &id.resource_type, id.identity_or_empty())
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
        let ds_id =
            ResourceId::with_provider_identity("aws", "iam.Roles", "admin_access_roles", None);
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
        let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "log", None);

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
mod apply_state_save_tests {
    use super::*;
    use carina_core::differ::create_plan;
    use carina_core::effect::{
        DeferredReplaceDelete, DeferredReplacePayload, Effect, NonEmptyDeletes,
    };
    use carina_core::parser::{DeferredForExpression, ForBinding};
    use carina_core::plan::Plan;
    use carina_core::provider::ProviderRouter;
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Directives, Resource, ResourceId, State, UnknownReason, Value,
    };
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    use carina_core::value::SECRET_PREFIX;
    use carina_state::{DeposedInstance, DeposedKey, NameOverride};

    fn plan_with_name_override(id: &ResourceId, temp_value: &str, original_value: &str) -> Plan {
        serde_json::from_value(serde_json::json!({
            "effects": [],
            "permanent_name_overrides": [
                {
                    "resource_id": id,
                    "attribute": "name",
                    "temp_value": temp_value,
                    "original_value": original_value
                }
            ]
        }))
        .expect("plan with permanent name override should deserialize")
    }

    fn applied_name_state(id: &ResourceId, name: &str) -> State {
        State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String(name.to_string())),
            )]),
        )
        .with_identifier(format!("{name}-id"))
    }

    #[test]
    fn writeback_persists_typed_name_override_from_plan() {
        let mut resource = Resource::with_provider("mock", "test.resource", "thing", None);
        resource.set_attr(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("dsl-new".to_string())),
        );
        let id = resource.id.clone();
        let sorted_resources = vec![resource];
        let mut existing = ResourceState::new("test.resource", "thing", "mock");
        existing.name_overrides.insert(
            "name".to_string(),
            NameOverride {
                temp_value: "old-temp".to_string(),
                original_value: Some("stale-dsl".to_string()),
            },
        );
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let applied_states = HashMap::from([(id.clone(), applied_name_state(&id, "new-temp"))]);
        let plan = plan_with_name_override(&id, "new-temp", "dsl-new");

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &carina_core::schema::SchemaRegistry::new(),
        })
        .expect("state writeback should succeed");

        let saved = state
            .find_resource("mock", "test.resource", "thing")
            .expect("resource should be persisted");
        assert_eq!(
            saved.name_overrides.get("name"),
            Some(&NameOverride {
                temp_value: "new-temp".to_string(),
                original_value: Some("dsl-new".to_string()),
            })
        );
    }

    #[test]
    fn writeback_overwrites_legacy_original_value_with_plan_data() {
        let mut resource = Resource::with_provider("mock", "test.resource", "thing", None);
        resource.set_attr(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("dsl-value".to_string())),
        );
        let id = resource.id.clone();
        let sorted_resources = vec![resource];
        let mut existing = ResourceState::new("test.resource", "thing", "mock");
        existing.name_overrides.insert(
            "name".to_string(),
            NameOverride {
                temp_value: "legacy-temp".to_string(),
                original_value: None,
            },
        );
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let applied_states = HashMap::from([(id.clone(), applied_name_state(&id, "legacy-temp"))]);
        let plan = plan_with_name_override(&id, "legacy-temp", "dsl-value");

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &carina_core::schema::SchemaRegistry::new(),
        })
        .expect("state writeback should succeed");

        let saved = state
            .find_resource("mock", "test.resource", "thing")
            .expect("resource should be persisted");
        assert_eq!(
            saved.name_overrides.get("name"),
            Some(&NameOverride {
                temp_value: "legacy-temp".to_string(),
                original_value: Some("dsl-value".to_string()),
            })
        );
    }

    #[test]
    fn build_state_after_apply_persists_runtime_synthesized_resources() {
        let runtime_child = Resource::new("route53.Record", "validation_records[0]")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("_dns_record_name_".to_string())),
            );
        let child_id = runtime_child.id.clone();
        let child_state = State::existing(
            child_id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("_dns_record_name_".to_string())),
            )]),
        )
        .with_identifier("record-id");

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(StateFile::new()),
            sorted_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::from([(child_id.clone(), child_state)]),
            runtime_synthesized_resources: std::slice::from_ref(&runtime_child),
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &carina_core::schema::SchemaRegistry::new(),
        })
        .expect("state writeback should accept runtime-synthesized child");

        assert!(
            state
                .resources
                .iter()
                .any(|row| row.provider == child_id.provider
                    && row.resource_type == child_id.resource_type
                    && row.identity == "validation_records[0]"),
            "runtime-synthesized child must be persisted in state"
        );
    }

    #[test]
    fn successfully_deleted_uses_full_resource_id_not_identity_only() {
        let id_a = ResourceId::with_provider_identity("mock", "type.A", "shared", None);
        let id_b = ResourceId::with_provider_identity("mock", "type.B", "shared", None);
        let mut row_a = ResourceState::new("type.A", "shared", "mock");
        row_a.identifier = Some("a-old".to_string());
        let mut row_b = ResourceState::new("type.B", "shared", "mock");
        row_b.identifier = Some("b-old".to_string());
        let mut state_file = StateFile::new();
        state_file.resources.push(row_a);
        state_file.resources.push(row_b);

        let mut plan = Plan::new();
        for (id, identifier) in [(&id_a, "a-old"), (&id_b, "b-old")] {
            plan.add(Effect::Delete {
                id: carina_core::resource::ResolvedResourceId::new(id.clone()),
                identifier: identifier.to_string(),
                generation: EffectGeneration::Current,
                directives: Directives::default(),
                binding: None,
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            });
        }
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(id_a.clone(), "a-old")]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: &[],
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::new(),
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("writeback should accept independent deletes");

        assert!(
            state.find_resource("mock", "type.A", "shared").is_none(),
            "the succeeded delete row should be cleaned"
        );
        assert!(
            state.find_resource("mock", "type.B", "shared").is_some(),
            "the failed delete row with the same identity string must remain"
        );
    }

    #[test]
    fn top_level_deposed_delete_next_to_deferred_create_removes_entry_on_success() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            None,
        );
        let deposed_key = DeposedKey::new_unique();
        let mut row = ResourceState::new("route53.Record", "validation_records[0]", "aws");
        row.identifier = Some("new-record-id".to_string());
        row.deposed.push(DeposedInstance {
            key: deposed_key.clone(),
            identifier: "old-record-id".to_string(),
            provider_instance: None,
            attributes: HashMap::from([("name".to_string(), serde_json::json!("old.example.com"))]),
            dependency_bindings: BTreeSet::from(["cert".to_string()]),
        });
        let mut state_file = StateFile::new();
        state_file.resources.push(row);

        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: "old-record-id".to_string(),
            generation: EffectGeneration::Deposed(deposed_key.clone()),
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::from(["cert".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
        plan.add(Effect::DeferredCreate {
            id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "validation_records",
            )),
            upstream_binding: "cert".to_string(),
            template: Box::new(DeferredForExpression {
                file: Some("main.crn".to_string()),
                line: 1,
                header: "for opt in cert.domain_validation_options".to_string(),
                resource_type: "aws.route53.Record".to_string(),
                attributes: vec![],
                binding_name: "validation_records".to_string(),
                iterable_binding: "cert".to_string(),
                iterable_attr: "domain_validation_options".to_string(),
                binding: ForBinding::Simple("opt".to_string()),
                template_resource: Resource::with_provider(
                    "aws",
                    "route53.Record",
                    "validation_records",
                    None,
                )
                .with_binding("validation_records"),
            }),
        });
        let successfully_deleted = HashSet::from([DeletedInstanceKey::new(
            id.clone(),
            EffectGeneration::Deposed(deposed_key),
            "old-record-id",
        )]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: &[],
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::new(),
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("writeback should remove successful top-level deposed delete");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("new-record-id"));
        assert!(
            row.deposed.is_empty(),
            "successful top-level deposed delete must remove its stored generation"
        );
    }

    fn deferred_replace_delete(id: &ResourceId) -> DeferredReplaceDelete {
        DeferredReplaceDelete {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: "old-record-id".to_string(),
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::from(["cert".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }
    }

    fn deferred_replace_effect(id: &ResourceId) -> Effect {
        let template_resource = Resource::with_provider(
            &id.provider,
            &id.resource_type,
            "validation_records",
            id.provider_instance.clone(),
        )
        .with_binding("validation_records");
        Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(vec![deferred_replace_delete(id)])
                .expect("fixture has one delete"),
            id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "validation_records",
            )),
            upstream_binding: "cert".to_string(),
            template: Box::new(DeferredForExpression {
                file: Some("main.crn".to_string()),
                line: 1,
                header: "for opt in cert.domain_validation_options".to_string(),
                resource_type: "aws.route53.Record".to_string(),
                attributes: vec![],
                binding_name: "validation_records".to_string(),
                iterable_binding: "cert".to_string(),
                iterable_attr: "domain_validation_options".to_string(),
                binding: ForBinding::Simple("opt".to_string()),
                template_resource,
            }),
        }))
    }

    #[test]
    fn deferred_replace_delete_and_create_same_id_writes_one_upsert_no_cleanup() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            None,
        );
        let runtime_child =
            Resource::with_provider("aws", "route53.Record", "validation_records[0]", None)
                .with_binding("validation_records[0]");
        let applied_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("new-record-id");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::new();
        let applied_states = HashMap::from([(id.clone(), applied_state)]);
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(id.clone(), "old-record-id")]);
        let failed_refreshes = HashSet::new();
        let schemas = SchemaRegistry::new();

        let wb = decompose(DecomposeInput {
            state_file: &StateFile::new(),
            sorted_resources: &[],
            runtime_synthesized_resources: std::slice::from_ref(&runtime_child),
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        })
        .expect("successful DeferredReplace writeback must not collide");

        assert_eq!(wb.upserts.len(), 1);
        assert!(wb.upserts.contains_key(&id));
        assert!(
            !wb.cleanups.contains(&id),
            "successful create half must suppress cleanup for the replaced id"
        );
        assert!(wb.cleanups.is_empty());
    }

    #[test]
    fn deferred_replace_create_success_delete_not_successful_deposes_old_instance() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            Some("west".to_string()),
        );
        let runtime_child = Resource::with_provider(
            "aws",
            "route53.Record",
            "validation_records[0]",
            Some("west".to_string()),
        )
        .with_binding("validation_records[0]");
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("new.example.com".to_string())),
            )]),
        )
        .with_identifier("new-record-id");
        let current_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("old.example.com".to_string())),
            )]),
        )
        .with_identifier("old-record-id");
        let mut existing = ResourceState::new("route53.Record", "validation_records[0]", "aws")
            .with_identifier("old-record-id")
            .with_attribute("name", serde_json::json!("old.example.com"));
        existing.directives.provider_instance = Some("west".to_string());
        existing.dependency_bindings.insert("cert".to_string());
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), applied_state)]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: &[],
            runtime_synthesized_resources: std::slice::from_ref(&runtime_child),
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &carina_core::schema::SchemaRegistry::new(),
        })
        .expect("writeback should preserve old absorbed delete as deposed");

        let saved = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("new child row should be persisted");
        assert_eq!(saved.identifier.as_deref(), Some("new-record-id"));
        assert_eq!(saved.directives.provider_instance.as_deref(), Some("west"));
        let saved_json = serde_json::to_value(saved).expect("resource state should serialize");
        let deposed = saved_json
            .get("deposed")
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| {
                panic!("expected old-record-id to be retained as deposed; row: {saved_json:#?}")
            });
        assert_eq!(deposed.len(), 1);
        assert_eq!(deposed[0]["identifier"], serde_json::json!("old-record-id"));
        assert_eq!(deposed[0]["provider_instance"], serde_json::json!("west"));
    }

    #[test]
    fn replacement_create_same_identifier_does_not_depose() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            None,
        );
        let runtime_child =
            Resource::with_provider("aws", "route53.Record", "validation_records[0]", None)
                .with_binding("validation_records[0]");
        let applied_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("old-record-id");
        let current_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("old-record-id");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), applied_state)]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(StateFile::new()),
            sorted_resources: &[],
            runtime_synthesized_resources: std::slice::from_ref(&runtime_child),
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &carina_core::schema::SchemaRegistry::new(),
        })
        .expect("same-identifier replacement should write only the current row");

        let saved = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("current row should be persisted");
        assert_eq!(saved.identifier.as_deref(), Some("old-record-id"));
        assert!(
            saved.deposed.is_empty(),
            "same remote object must not be recorded as a deposed generation"
        );
    }

    #[test]
    fn update_only_applied_identifier_change_does_not_phantom_depose_old_identifier() {
        let desired = Resource::with_provider("awscc", "ec2.Vpc", "main", None)
            .with_binding("main")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            );
        let id = desired.id.clone();
        let existing = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-canonical-old")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let from_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )]),
        )
        .with_identifier("vpc-canonical-old");
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            )]),
        )
        .with_identifier("vpc-canonical-new");
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            from: Box::new(from_state),
            to: carina_core::resource::ResolvedResource::new(desired.clone()),
            changed_attributes: vec!["cidr_block".to_string()],
        });

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::from([(id, applied_state)]),
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("update writeback should not depose canonicalized identifiers");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("vpc-canonical-new"));
        assert!(
            row.deposed.is_empty(),
            "update-only identifier rewrites are the same live remote object"
        );
    }

    #[test]
    fn applied_upsert_that_displaces_undeleted_current_identifier_deposes_old_identifier() {
        let desired = Resource::with_provider("awscc", "ec2.Vpc", "main", None)
            .with_binding("main")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            );
        let id = desired.id.clone();
        let mut existing = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-old")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        existing.dependency_bindings.insert("igw".to_string());
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            )]),
        )
        .with_identifier("vpc-new");
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::from([(id, applied_state)]),
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("writeback should depose the displaced current identifier");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("vpc-new"));
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].identifier, "vpc-old");
        assert_eq!(
            row.deposed[0].attributes.get("cidr_block"),
            Some(&serde_json::json!("10.0.0.0/16"))
        );
        assert_eq!(
            row.deposed[0].dependency_bindings,
            BTreeSet::from(["igw".to_string()])
        );
    }

    #[test]
    fn applied_recreate_after_confirmed_missing_current_does_not_depose_old_identifier() {
        let desired = Resource::with_provider("awscc", "ec2.Vpc", "main", None)
            .with_binding("main")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            );
        let id = desired.id.clone();
        let existing = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-old")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let current_state = State::not_found(id.clone()).with_identifier("vpc-old");
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            )]),
        )
        .with_identifier("vpc-new");
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &HashMap::from([(id.clone(), current_state)]),
            applied_states: &HashMap::from([(id, applied_state)]),
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("writeback should treat the old identifier as already absent");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("vpc-new"));
        assert!(
            row.deposed.is_empty(),
            "pre-apply exists=false proves the displaced identifier is already gone"
        );
    }

    #[test]
    fn displaced_identifier_depose_records_existing_row_provider_instance() {
        let desired =
            Resource::with_provider("awscc", "ec2.Vpc", "main", Some("new-provider".to_string()))
                .with_binding("main")
                .with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
                );
        let id = desired.id.clone();
        let mut existing = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-old")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        existing.directives.provider_instance = Some("old-provider".to_string());
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            )]),
        )
        .with_identifier("vpc-new");
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::from([(id, applied_state)]),
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("writeback should depose the displaced current identifier");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("vpc-new"));
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].identifier, "vpc-old");
        assert_eq!(
            row.deposed[0].provider_instance.as_deref(),
            Some("old-provider"),
            "the deposed generation must route to the provider instance where the displaced object lived"
        );
    }

    #[test]
    fn rerouted_replacement_with_successful_old_routing_delete_does_not_depose_displaced_identifier()
     {
        let desired =
            Resource::with_provider("awscc", "ec2.Vpc", "main", Some("new-provider".to_string()))
                .with_binding("main")
                .with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
                );
        let create_id = desired.id.clone();
        let delete_id = ResourceId::with_provider_identity(
            "awscc",
            "ec2.Vpc",
            "main",
            Some("old-provider".to_string()),
        );
        let mut existing = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-old")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        existing.directives.provider_instance = Some("old-provider".to_string());
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let applied_state = State::existing(
            create_id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            )]),
        )
        .with_identifier("vpc-new");
        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(delete_id.clone()),
            identifier: "vpc-old".to_string(),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: Some("main".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));
        let previous_attributes = HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        )]);
        let mut plan_json = serde_json::to_value(&plan).expect("plan should serialize");
        plan_json["replace_display"] = serde_json::json!([
            {
                "create_idx": 1,
                "delete_idx": 0,
                "create_before_destroy": false,
                "changed_create_only": ["cidr_block"],
                "cascade_ref_hints": [],
                "temporary_name": null,
                "previous_attributes": previous_attributes,
            }
        ]);
        let plan: Plan =
            serde_json::from_value(plan_json).expect("replacement metadata should deserialize");
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(delete_id, "vpc-old")]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::from([(create_id, applied_state)]),
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("writeback should treat the old routed delete as successful");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("vpc-new"));
        assert!(
            row.deposed.is_empty(),
            "a displaced identifier that was deleted through the old provider routing must not be phantom-deposed"
        );
    }

    #[test]
    fn successful_delete_on_other_provider_instance_does_not_suppress_displaced_depose() {
        let desired =
            Resource::with_provider("awscc", "ec2.Vpc", "main", Some("new-provider".to_string()))
                .with_binding("main")
                .with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
                );
        let create_id = desired.id.clone();
        let mut existing = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("shared-name")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        existing.directives.provider_instance = Some("old-provider".to_string());
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let applied_state = State::existing(
            create_id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            )]),
        )
        .with_identifier("vpc-new");
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));
        let successfully_deleted = HashSet::from([DeletedInstanceKey::current(
            create_id.clone(),
            "shared-name",
        )]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::from([(create_id, applied_state)]),
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("writeback should not confuse provider-instance-local identifiers");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("vpc-new"));
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].identifier, "shared-name");
        assert_eq!(
            row.deposed[0].provider_instance.as_deref(),
            Some("old-provider")
        );
    }

    #[test]
    fn replacement_rerun_deposes_displaced_current_even_when_failed_delete_already_deposed_same_id()
    {
        let desired = Resource::with_provider(
            "aws",
            "route53.Record",
            "validation_records[0]",
            Some("west".to_string()),
        )
        .with_binding("validation_records[0]")
        .with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("new-2.example.com".to_string())),
        );
        let id = desired.id.clone();
        let mut existing = ResourceState::new("route53.Record", "validation_records[0]", "aws")
            .with_identifier("new-1-record-id")
            .with_attribute("name", serde_json::json!("new-1.example.com"));
        existing.directives.provider_instance = Some("west".to_string());
        existing.deposed.push(DeposedInstance {
            key: DeposedKey::new_unique(),
            identifier: "old-record-id".to_string(),
            provider_instance: Some("west".to_string()),
            attributes: HashMap::from([("name".to_string(), serde_json::json!("old.example.com"))]),
            dependency_bindings: BTreeSet::new(),
        });
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);

        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("new-2.example.com".to_string())),
            )]),
        )
        .with_identifier("new-2-record-id");
        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: "old-record-id".to_string(),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));
        let previous_attributes = HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("old.example.com".to_string())),
        )]);
        let mut plan_json = serde_json::to_value(&plan).expect("plan should serialize");
        plan_json["replace_display"] = serde_json::json!([
            {
                "create_idx": 1,
                "delete_idx": 0,
                "create_before_destroy": false,
                "changed_create_only": ["name"],
                "cascade_ref_hints": [],
                "temporary_name": null,
                "previous_attributes": previous_attributes,
            }
        ]);
        let plan: Plan =
            serde_json::from_value(plan_json).expect("replacement metadata should deserialize");

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::from([(id, applied_state)]),
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("writeback should preserve both failed old delete and displaced current");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("new-2-record-id"));
        let mut deposed: Vec<_> = row
            .deposed
            .iter()
            .map(|entry| {
                (
                    entry.identifier.as_str(),
                    entry.provider_instance.as_deref(),
                )
            })
            .collect();
        deposed.sort_unstable();
        assert_eq!(
            deposed,
            vec![
                ("new-1-record-id", Some("west")),
                ("old-record-id", Some("west"))
            ],
            "same-id dedup must only skip the exact displaced generation, not a different failed delete"
        );
    }

    #[test]
    fn replacement_rerun_deposes_displaced_current_and_retains_old_deposed_after_current_delete_success()
     {
        let desired = Resource::with_provider(
            "aws",
            "route53.Record",
            "validation_records[0]",
            Some("west".to_string()),
        )
        .with_binding("validation_records[0]")
        .with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("new-2.example.com".to_string())),
        );
        let id = desired.id.clone();
        let mut existing = ResourceState::new("route53.Record", "validation_records[0]", "aws")
            .with_identifier("new-1-record-id")
            .with_attribute("name", serde_json::json!("new-1.example.com"));
        existing.directives.provider_instance = Some("west".to_string());
        existing.deposed.push(DeposedInstance {
            key: DeposedKey::new_unique(),
            identifier: "old-record-id".to_string(),
            provider_instance: Some("west".to_string()),
            attributes: HashMap::from([("name".to_string(), serde_json::json!("old.example.com"))]),
            dependency_bindings: BTreeSet::new(),
        });
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);

        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("new-2.example.com".to_string())),
            )]),
        )
        .with_identifier("new-2-record-id");
        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: "old-record-id".to_string(),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));
        let previous_attributes = HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("old.example.com".to_string())),
        )]);
        let mut plan_json = serde_json::to_value(&plan).expect("plan should serialize");
        plan_json["replace_display"] = serde_json::json!([
            {
                "create_idx": 1,
                "delete_idx": 0,
                "create_before_destroy": false,
                "changed_create_only": ["name"],
                "cascade_ref_hints": [],
                "temporary_name": null,
                "previous_attributes": previous_attributes,
            }
        ]);
        let plan: Plan =
            serde_json::from_value(plan_json).expect("replacement metadata should deserialize");
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(id.clone(), "old-record-id")]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &HashMap::new(),
            applied_states: &HashMap::from([(id, applied_state)]),
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("writeback should depose displaced current and retain existing deposed entry");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("new-2-record-id"));
        let mut deposed: Vec<_> = row
            .deposed
            .iter()
            .map(|entry| {
                (
                    entry.identifier.as_str(),
                    entry.provider_instance.as_deref(),
                )
            })
            .collect();
        deposed.sort_unstable();
        assert_eq!(
            deposed,
            vec![
                ("new-1-record-id", Some("west")),
                ("old-record-id", Some("west"))
            ],
            "a Current-generation delete result must not remove an existing deposed generation"
        );
    }

    #[test]
    fn refresh_current_state_upsert_does_not_phantom_depose_displaced_identifier() {
        let desired = Resource::with_provider("awscc", "ec2.Vpc", "main", None)
            .with_binding("main")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            );
        let id = desired.id.clone();
        let existing = ResourceState::new("ec2.Vpc", "main", "awscc")
            .with_identifier("vpc-old")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let refreshed_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            )]),
        )
        .with_identifier("vpc-new");

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &HashMap::from([(id, refreshed_state)]),
            applied_states: &HashMap::new(),
            plan: &Plan::new(),
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &SchemaRegistry::new(),
        })
        .expect("refresh writeback should update the current row only");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(row.identifier.as_deref(), Some("vpc-new"));
        assert!(
            row.deposed.is_empty(),
            "refresh-only current-state upserts must not depose the previous identifier"
        );
    }

    #[test]
    fn deposed_fallback_attributes_hash_secrets_and_skip_unknowns() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            None,
        );
        let runtime_child =
            Resource::with_provider("aws", "route53.Record", "validation_records[0]", None)
                .with_binding("validation_records[0]")
                .with_attribute(
                    "password",
                    Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                        ConcreteValue::String("plain-secret".to_string()),
                    )))),
                );
        let applied_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("new-record-id");
        let current_state = State::existing(
            id.clone(),
            HashMap::from([
                (
                    "password".to_string(),
                    Value::Concrete(ConcreteValue::String("plain-secret".to_string())),
                ),
                (
                    "unknown".to_string(),
                    Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
                ),
            ]),
        )
        .with_identifier("old-record-id");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), applied_state)]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(StateFile::new()),
            sorted_resources: &[],
            runtime_synthesized_resources: std::slice::from_ref(&runtime_child),
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &carina_core::schema::SchemaRegistry::new(),
        })
        .expect("unknown deposed fallback attributes should be skipped, not fail writeback");

        let saved = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("new child row should be persisted");
        let deposed = saved
            .deposed
            .first()
            .expect("old instance should be deposed");
        let stored_secret = deposed
            .attributes
            .get("password")
            .and_then(serde_json::Value::as_str)
            .expect("secret fallback should be stored as a hash");
        assert!(stored_secret.starts_with(SECRET_PREFIX));
        assert!(
            !stored_secret.contains("plain-secret"),
            "deposed attributes must not persist plaintext secrets"
        );
        assert!(
            !deposed.attributes.contains_key("unknown"),
            "unserializable fallback attributes should be skipped"
        );
    }

    #[test]
    fn deposed_fallback_drops_schema_write_only_attribute_removed_from_new_desired() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            None,
        );
        let runtime_child =
            Resource::with_provider("aws", "route53.Record", "validation_records[0]", None)
                .with_binding("validation_records[0]")
                .with_attribute(
                    "name",
                    Value::Concrete(ConcreteValue::String("new.example.com".to_string())),
                );
        let applied_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("new-record-id");
        let current_state = State::existing(
            id.clone(),
            HashMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("old.example.com".to_string())),
                ),
                (
                    "password".to_string(),
                    Value::Concrete(ConcreteValue::String("plain-secret".to_string())),
                ),
            ]),
        )
        .with_identifier("old-record-id");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), applied_state)]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            ResourceSchema::new("route53.Record")
                .attribute(AttributeSchema::new("password", AttributeType::string()).write_only()),
        );

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(StateFile::new()),
            sorted_resources: &[],
            runtime_synthesized_resources: std::slice::from_ref(&runtime_child),
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        })
        .expect("schema-protected removed attributes should not fail writeback");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("new child row should be persisted");
        let deposed = row.deposed.first().expect("old instance should be deposed");
        assert_eq!(
            deposed.attributes.get("name"),
            Some(&serde_json::json!("old.example.com"))
        );
        assert!(
            !deposed.attributes.contains_key("password"),
            "schema write-only fallback must not persist removed plaintext"
        );
        let serialized = serde_json::to_string(&deposed).expect("deposed should serialize");
        assert!(
            !serialized.contains("plain-secret"),
            "deposed entry must not contain plaintext secret"
        );
    }

    #[test]
    fn apply_planned_depose_inserts_shell_when_row_missing() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            Some("west".to_string()),
        );
        let mut state = StateFile::new();

        apply_planned_depose(
            &mut state,
            PlannedDepose {
                id: id.clone(),
                instance: DeposedInstance {
                    key: DeposedKey::new_unique(),
                    identifier: "old-record-id".to_string(),
                    provider_instance: Some("west".to_string()),
                    attributes: HashMap::from([(
                        "name".to_string(),
                        serde_json::json!("old.example.com"),
                    )]),
                    dependency_bindings: BTreeSet::from(["cert".to_string()]),
                },
            },
        );

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("missing row should be created for deposed instance");
        assert_eq!(row.identifier, None);
        assert!(row.attributes.is_empty());
        assert_eq!(row.directives.provider_instance.as_deref(), Some("west"));
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].identifier, "old-record-id");
        assert_eq!(row.deposed[0].provider_instance.as_deref(), Some("west"));
    }

    #[test]
    fn apply_planned_depose_replaces_existing_entry_with_same_identifier() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            Some("west".to_string()),
        );
        let east_key = DeposedKey::new_unique();
        let west_key = DeposedKey::new_unique();
        let replacement_west_key = DeposedKey::new_unique();
        let mut row = ResourceState::new("route53.Record", "validation_records[0]", "aws");
        row.directives.provider_instance = Some("west".to_string());
        row.deposed.push(DeposedInstance {
            key: east_key.clone(),
            identifier: "old-record-id".to_string(),
            provider_instance: Some("east".to_string()),
            attributes: HashMap::from([("name".to_string(), serde_json::json!("stale"))]),
            dependency_bindings: BTreeSet::from(["old".to_string()]),
        });
        let mut state = StateFile::new();
        state.resources.push(row);

        apply_planned_depose(
            &mut state,
            PlannedDepose {
                id: id.clone(),
                instance: DeposedInstance {
                    key: west_key.clone(),
                    identifier: "old-record-id".to_string(),
                    provider_instance: Some("west".to_string()),
                    attributes: HashMap::from([("name".to_string(), serde_json::json!("fresh"))]),
                    dependency_bindings: BTreeSet::from(["new".to_string()]),
                },
            },
        );

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("row should still exist");
        assert_eq!(
            row.deposed.len(),
            2,
            "same identifier under different provider routing should be retained separately"
        );
        assert_eq!(row.deposed[0].key, east_key);
        assert_eq!(row.deposed[0].provider_instance.as_deref(), Some("east"));
        assert_eq!(row.deposed[1].key, west_key);
        assert_eq!(row.deposed[1].provider_instance.as_deref(), Some("west"));

        apply_planned_depose(
            &mut state,
            PlannedDepose {
                id,
                instance: DeposedInstance {
                    key: replacement_west_key.clone(),
                    identifier: "old-record-id".to_string(),
                    provider_instance: Some("west".to_string()),
                    attributes: HashMap::from([("name".to_string(), serde_json::json!("newer"))]),
                    dependency_bindings: BTreeSet::from(["newer".to_string()]),
                },
            },
        );

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("row should still exist");
        assert_eq!(row.deposed.len(), 2);
        let deposed = &row.deposed[1];
        assert_eq!(deposed.identifier, "old-record-id");
        assert_eq!(deposed.key, replacement_west_key);
        assert_eq!(deposed.provider_instance.as_deref(), Some("west"));
        assert_eq!(
            deposed.attributes.get("name"),
            Some(&serde_json::json!("newer"))
        );
        assert_eq!(
            deposed.dependency_bindings,
            BTreeSet::from(["newer".to_string()])
        );
    }

    #[test]
    fn replacement_writeback_appends_second_deposed_generation() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            None,
        );
        let runtime_child =
            Resource::with_provider("aws", "route53.Record", "validation_records[0]", None)
                .with_binding("validation_records[0]");
        let applied_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("new-record-id");
        let current_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("old.example.com".to_string())),
            )]),
        )
        .with_identifier("old-record-id");
        let first_key = DeposedKey::new_unique();
        let mut existing = ResourceState::new("route53.Record", "validation_records[0]", "aws")
            .with_identifier("old-record-id")
            .with_attribute("name", serde_json::json!("old.example.com"));
        existing.deposed.push(DeposedInstance {
            key: first_key.clone(),
            identifier: "older-record-id".to_string(),
            provider_instance: None,
            attributes: HashMap::from([("name".to_string(), serde_json::json!("older"))]),
            dependency_bindings: BTreeSet::from(["cert".to_string()]),
        });
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), applied_state)]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: &[],
            runtime_synthesized_resources: std::slice::from_ref(&runtime_child),
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &carina_core::schema::SchemaRegistry::new(),
        })
        .expect("writeback should preserve existing deposed entries and append the new one");

        let saved = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("new child row should be persisted");
        assert_eq!(saved.identifier.as_deref(), Some("new-record-id"));
        assert_eq!(saved.deposed.len(), 2);
        assert_eq!(saved.deposed[0].identifier, "older-record-id");
        assert_eq!(saved.deposed[1].identifier, "old-record-id");
        assert_ne!(saved.deposed[0].key, saved.deposed[1].key);
        assert_eq!(saved.deposed[0].key, first_key);
    }

    #[test]
    fn deferred_replace_delete_success_create_failure_writes_cleanup() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            None,
        );
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::new();
        let applied_states = HashMap::new();
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(id.clone(), "old-record-id")]);
        let failed_refreshes = HashSet::new();
        let schemas = SchemaRegistry::new();

        let wb = decompose(DecomposeInput {
            state_file: &StateFile::new(),
            sorted_resources: &[],
            runtime_synthesized_resources: &[],
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        })
        .expect("delete-only half of DeferredReplace should still write cleanup");

        assert!(wb.upserts.is_empty());
        assert_eq!(wb.cleanups, HashSet::from([id]));
    }

    #[test]
    fn plain_replacement_delete_success_create_failure_writes_cleanup() {
        let resource = Resource::new("ec2.Vpc", "main").with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
        );
        let id = resource.id.clone();
        let sorted_resources = vec![resource];
        let current_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )]),
        )
        .with_identifier("vpc-old");
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "",
            ResourceSchema::new("ec2.Vpc").attribute(
                AttributeSchema::new("cidr_block", AttributeType::string()).create_only(),
            ),
        );
        let plan_input_states = carina_core::resource::into_plan_input_map(
            current_states.clone(),
            &SchemaRegistry::new(),
            &[],
        );
        let plan = create_plan(
            &sorted_resources,
            &[],
            &ProviderRouter::new(),
            &plan_input_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &[],
        );
        assert_eq!(plan.replace_display_info().count(), 1);
        let applied_states = HashMap::new();
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(id.clone(), "vpc-old")]);
        let failed_refreshes = HashSet::new();

        let wb = decompose(DecomposeInput {
            state_file: &StateFile::new(),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        })
        .expect("delete-success/create-failure replacement should write cleanup");

        assert!(
            wb.upserts.is_empty(),
            "stale current-state fallback must not be written after the old instance was deleted"
        );
        assert_eq!(wb.cleanups, HashSet::from([id]));
    }

    #[test]
    fn moved_target_replacement_delete_failure_deposes_old_instance() {
        let from_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "old_bucket", None);
        let mut resource = Resource::with_provider("awscc", "s3.Bucket", "new_bucket", None)
            .with_attribute(
                "bucket_name",
                Value::Concrete(ConcreteValue::String("new-name".to_string())),
            );
        resource.directives.create_before_destroy = true;
        let id = resource.id.clone();
        let sorted_resources = vec![resource];
        let current_state = State::existing(
            id.clone(),
            HashMap::from([(
                "bucket_name".to_string(),
                Value::Concrete(ConcreteValue::String("old-name".to_string())),
            )]),
        )
        .with_identifier("bucket-old-id");
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            ResourceSchema::new("s3.Bucket")
                .attribute(
                    AttributeSchema::new("bucket_name", AttributeType::string()).create_only(),
                )
                .with_coexisting_replacement(),
        );
        let plan_input_states =
            carina_core::resource::into_plan_input_map(current_states.clone(), &schemas, &[]);
        let mut plan = create_plan(
            &sorted_resources,
            &[],
            &ProviderRouter::new(),
            &plan_input_states,
            &HashMap::new(),
            &schemas,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &[],
        );
        let replacement = plan
            .replace_display_info()
            .next()
            .expect("create-only diff should produce replacement metadata");
        assert!(replacement.create_before_destroy);
        let state_blocks: Vec<carina_core::parser::StateBlock> = Vec::new();
        let state_file_for_wiring = Some(StateFile::new());
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let unresolved_upstreams: HashSet<&str> = HashSet::new();

        crate::wiring::add_state_block_effects(
            &mut plan,
            &state_blocks,
            &state_file_for_wiring,
            &[(from_id.clone(), id.clone())],
            &sorted_resources,
            &bindings,
            &unresolved_upstreams,
        );

        assert_eq!(
            plan.replace_display_info().count(),
            1,
            "moved target replacement metadata must survive state-block suppression"
        );
        assert!(
            plan.effects()
                .iter()
                .any(|effect| matches!(effect, Effect::Delete { id: delete_id, .. } if delete_id.as_ref() == &id)),
            "replacement Delete for moved target must not be suppressed"
        );

        let mut existing = ResourceState::new("s3.Bucket", "new_bucket", "awscc")
            .with_identifier("bucket-old-id")
            .with_attribute("bucket_name", serde_json::json!("old-name"));
        existing.dependency_bindings.insert("logs".to_string());
        let mut state_file = StateFile::new();
        state_file.resources.push(existing);
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "bucket_name".to_string(),
                Value::Concrete(ConcreteValue::String("new-name".to_string())),
            )]),
        )
        .with_identifier("bucket-new-id");
        let applied_states = HashMap::from([(id.clone(), applied_state)]);

        let state = build_state_after_apply(ApplyStateSave {
            state_file: Some(state_file),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &HashSet::new(),
            failed_refreshes: &HashSet::new(),
            schemas: &schemas,
        })
        .expect("moved replacement writeback should preserve the failed delete as deposed");

        let row = state
            .find_resource("awscc", "s3.Bucket", "new_bucket")
            .expect("moved target row should be persisted");
        assert_eq!(row.identifier.as_deref(), Some("bucket-new-id"));
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].identifier, "bucket-old-id");
        assert_eq!(row.deposed[0].provider_instance, None);
        assert_eq!(
            row.deposed[0].attributes.get("bucket_name"),
            Some(&serde_json::json!("old-name"))
        );
    }

    #[test]
    fn move_from_overlapping_desired_resource_still_errors() {
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            None,
        );
        let desired =
            Resource::with_provider("aws", "route53.Record", "validation_records[0]", None);
        let applied_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("new-record-id");
        let mut plan = Plan::new();
        plan.add(Effect::Move {
            from: carina_core::resource::ResolvedResourceId::new(id.clone()),
            to: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
                "aws",
                "route53.Record",
                "validation_records[1]",
                None,
            )),
        });
        let current_states = HashMap::new();
        let applied_states = HashMap::from([(id.clone(), applied_state)]);
        let successfully_deleted = HashSet::new();
        let failed_refreshes = HashSet::new();
        let schemas = SchemaRegistry::new();

        let result = decompose(DecomposeInput {
            state_file: &StateFile::new(),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            current_states: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        });
        let err = match result {
            Ok(_) => panic!("Move from-address cleanup must still collide with desired upsert"),
            Err(err) => err,
        };

        assert!(matches!(err, WritebackConflict::UpsertCleanupOverlap { id: got } if got == id));
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
        resolve_exports(
            export_params,
            &[],
            &[],
            &[],
            &post_apply_states,
            &carina_core::schema::SchemaRegistry::new(),
            &[],
        )
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
