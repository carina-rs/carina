//! State write-back helpers shared between `apply` and `destroy`.
//!
//! These helpers translate the in-memory execution result into a persisted
//! `StateFile`: applying name overrides, building the post-apply state,
//! resolving exports, and (for destroy) removing destroyed resources.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use carina_core::data_source_value::DataSourceValues;
use carina_core::effect::{DeferredReplaceDelete, DeletedInstanceKey, Effect, EffectGeneration};
use carina_core::executor::{AcceptedRequest, Applied, ExecutionResult};
use carina_core::plan::{Plan, ReplaceDisplayInfo, ReplacementPreviousAttributesForStateWriteback};
#[cfg(test)]
use carina_core::provider::SentRecord;
use carina_core::provider::{
    AppliedIdentifier, InFlightCreate, InFlightPersistence, ObservedCreate, PersistedAttributes,
};
use carina_core::resource::{
    ConcreteValue, Current, ModuleSource, ResolvedResource, ResolvedResourceId, Resource,
    ResourceId, State, Value,
};
use carina_core::schema::{ResourceSchema, SchemaRegistry};
use carina_core::value::PersistedExportValue;
#[cfg(test)]
use carina_core::value::PersistedValue;
use carina_state::{
    DeposedAttributes, DeposedInstance, DeposedKey, PreviousSecretHashAuthority, ResourceState,
    RowBasis, StateBackend, StateFile,
};
use colored::Colorize;

use crate::commands::plan::MovedOrigins;
use crate::commands::shared::incremental_state_saver::FinalStateBase;
use crate::commands::shared::lock_heartbeat::StateLockRef;
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
                    Value::Concrete(ConcreteValue::String(value.clone().into())),
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
    pending_refreshes: &mut HashMap<ResourceId, AppliedIdentifier>,
    id: &ResourceId,
    identifier: &AppliedIdentifier,
) {
    pending_refreshes.insert(id.clone(), identifier.clone());
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
/// 2. `managed_current` and `data_source_values` together hold the live
///    execution view. For managed resources, `managed_current` may still
///    carry the **pre-apply** snapshot
///    (whatever the executor read at refresh time, before the apply
///    mutation ran), so the post-apply attribute values in
///    `state.resources` must win on key collision.
///
/// The single correct merge is therefore: preserve the transient data-source
/// map, clone `managed_current`, then overlay entries derived from
/// `state.resources` so managed post-apply values win. Earlier ad-hoc call
/// sites re-introduced the bug each time they forgot one half of the merge;
/// this newtype forces the merge to happen exactly once, in
/// [`PostApplyStates::from_current_and_state`].
#[derive(Debug, Clone)]
pub(crate) struct PostApplyStates {
    managed_current: HashMap<ResourceId, Current>,
    data_source_values: DataSourceValues,
}

impl PostApplyStates {
    /// Build the post-apply view from the live managed and data-source maps
    /// and the `StateFile` whose
    /// `state.resources` was just rewritten by the apply writeback.
    ///
    /// State-derived managed entries win on key collision (see the
    /// type-level doc-comment for why).
    pub(crate) fn from_current_and_state(
        managed_current: &HashMap<ResourceId, Current>,
        data_source_values: &DataSourceValues,
        state: &StateFile,
    ) -> Self {
        let mut managed_current = managed_current.clone();
        for rs in state.resources() {
            let identity = rs.identity.clone();
            let id = ResourceId::with_provider_identity(
                rs.provider.clone(),
                rs.resource_type.clone(),
                identity,
                rs.directives.provider_instance.clone(),
            );
            let current = state.build_state_for_resource(&id);
            managed_current.insert(id, current);
        }
        Self {
            managed_current,
            data_source_values: data_source_values.clone(),
        }
    }

    pub(crate) fn managed_current(&self) -> &HashMap<ResourceId, Current> {
        &self.managed_current
    }

    pub(crate) fn data_source_values(&self) -> &DataSourceValues {
        &self.data_source_values
    }
}

/// Immutable inputs shared by every checkpoint and the final apply write.
///
/// Owning these together prevents the background saver and final write from
/// silently diverging on the desired resources, plan, schemas, or saved-plan
/// secret hashes.
pub(crate) struct ApplyStateSaveInputs<S = ModuleSource> {
    sorted_resources: Vec<ResolvedResource<S>>,
    plan: Plan<S>,
    schemas: SchemaRegistry,
    planned_secret_hashes: Option<HashMap<ResourceId, PersistedAttributes>>,
}

impl<S: Clone> ApplyStateSaveInputs<S> {
    pub(crate) fn new(
        sorted_resources: Vec<ResolvedResource<S>>,
        plan: Plan<S>,
        schemas: SchemaRegistry,
        planned_secret_hashes: Option<HashMap<ResourceId, PersistedAttributes>>,
    ) -> Self {
        Self {
            sorted_resources,
            plan,
            schemas,
            planned_secret_hashes,
        }
    }

    pub(crate) fn sorted_resources(&self) -> &[ResolvedResource<S>] {
        &self.sorted_resources
    }

    pub(crate) fn schemas(&self) -> &SchemaRegistry {
        &self.schemas
    }

    pub(crate) fn build_state(
        &self,
        state: StateFile,
        result: &ExecutionResult<S>,
    ) -> Result<StateFile, AppError> {
        build_state_after_apply(
            ApplyStateSave {
                state_file: state,
                sorted_resources: &self.sorted_resources,
                runtime_synthesized_resources: &result.runtime_synthesized_resources,
                managed_current: &result.managed_current,
                applied_states: &result.applied_states,
                plan: &self.plan,
                successfully_deleted: &result.successfully_deleted,
                failed_refreshes: &result.failed_refreshes,
                schemas: &self.schemas,
            },
            self.planned_secret_hashes.as_ref(),
        )
    }
}

/// Source-driven inputs needed to resolve exports after apply.
///
/// Keeping these values in one bundle makes it impossible to request export
/// resolution without both the live data-source view and live resources.
/// The default [`Resource`] parameter is [`ModuleSource`], so saved-plan
/// resources cannot enter the live export-resolution path.
pub(crate) struct ExportResolutionInputs<'a> {
    pub export_params: &'a carina_core::validation::ExportsCheckedPreApply<
        carina_core::parser::ExpandedInferredExportParam,
    >,
    /// Total post-read outcomes. Apply-time reads replace their deferred
    /// entries before this reaches export resolution.
    pub data_source_values: &'a DataSourceValues,
    pub resources: &'a [Resource],
    /// Loaded provider custom types used when post-apply bindings make a
    /// previously deferred export expression concrete.
    pub provider_context: &'a carina_core::parser::LoadedProviderContext,
}

/// Input parameters for `finalize_apply`.
///
/// Groups the execution result, resource data, and backend configuration
/// needed to save state after an apply operation.
pub(crate) struct FinalizeApplyInput<'a, S = ModuleSource> {
    pub result: &'a ExecutionResult<S>,
    pub state_base: FinalStateBase,
    pub state_save_inputs: Arc<ApplyStateSaveInputs<S>>,
    /// `Some(inputs)` rebuilds `state.exports` from the source
    /// configuration's `exports {}` block — empty `params` clears the
    /// map (#2932). `None` preserves the existing `state.exports` and
    /// is used by `apply --plan` because saved plan files do not
    /// persist `export_params` today; with `None` the apply path
    /// neither resolves nor wipes them, leaving the prior values
    /// intact for the next source-driven `carina apply` to
    /// reconcile.
    pub export_resolution_inputs: Option<ExportResolutionInputs<'a>>,
    pub backend: &'a dyn StateBackend,
    pub lock: StateLockRef<'a>,
    /// Wait-binding aliases (carina#3085). When export expressions
    /// reference a `wait` binding (`exports { x = cert_issued.arn }`),
    /// these make `<wait-binding>.<attr>` resolve to `<target>.<attr>`
    /// at writeback, the same passthrough the plan path now applies.
    /// Empty when the configuration declares no `wait` bindings, or
    /// for the `apply --plan` path that has no source-side wait view
    /// (paired with `export_resolution_inputs: None`, so no export resolution
    /// runs there anyway).
    pub wait_aliases: &'a [carina_core::binding_index::WaitAliasSpec],
    /// Pre-resolve snapshot of every `LiveComposition` in the
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
    /// `export_resolution_inputs` is also `None` and no export resolution
    /// runs.
    pub pre_resolve_compositions: &'a [carina_core::resource::LiveComposition],
}

/// Export writeback result split by per-export outcome.
///
/// This deliberately avoids the all-or-nothing `HashMap` shape that
/// caused carina#3551: a partially failed apply can leave exports that
/// depend on the failed resource unresolved, but carina#3498 requires
/// successfully completed resources from the same run to still be
/// persisted. Resolved exports are written to `state.exports`; skipped
/// exports are omitted, surfaced per-export on stdout, and returned to the
/// caller as [`SkippedExports`] for exit-status handling.
pub(crate) struct ExportResolution {
    /// Exports that resolved to concrete JSON and are safe to persist.
    resolved: HashMap<String, PersistedExportValue>,
    /// Exports omitted because their value still depends on unresolved
    /// apply-time data.
    skipped: Vec<SkippedExport>,
}

/// Export names that could not be written during an otherwise successful
/// state writeback.
///
/// [`ExportResolution::write_into`] returns this after merging resolved values
/// into an in-memory state file. Command callers must carry it through the
/// backend save and only then choose a failing exit status, preserving the
/// partial-apply writeback contract.
///
/// That returned API flow is the real fix; `must_use` is an extra compile-time
/// tripwire against a future caller silently discarding the summary.
#[must_use = "skipped export summaries must be handled after state is saved"]
#[derive(Debug, Default)]
pub(crate) struct SkippedExports {
    names: Vec<String>,
}

impl SkippedExports {
    #[cfg(test)]
    pub(crate) fn from_names_for_test(names: &[&str]) -> Self {
        Self {
            names: names.iter().map(|name| (*name).to_string()).collect(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub(crate) fn count_description(&self) -> String {
        format!(
            "{} export{} not written",
            self.len(),
            if self.len() == 1 { "" } else { "s" },
        )
    }

    pub(crate) fn write_failure_description(&self) -> String {
        format!(
            "{} to state: {}",
            self.count_description(),
            self.names.join(", "),
        )
    }

    pub(crate) fn failure_message(&self) -> String {
        format!(
            "{} (see '! export ...' lines above).",
            self.write_failure_description(),
        )
    }
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
    /// Consumes `self` and returns a [`SkippedExports`] summary that callers
    /// thread through state saving into the command's exit status
    /// (carina#3710). This also prevents callers from reading only the resolved
    /// half and silently dropping skipped diagnostics
    /// (carina#3551 / CLAUDE.md "Long-term view alongside root-cause").
    pub(crate) fn write_into(self, state: &mut StateFile) -> SkippedExports {
        let Self { resolved, skipped } = self;
        let mut next = HashMap::new();
        for skipped in &skipped {
            println!("{}", render_skipped(skipped));
            if let Some(prior) = state.exports.get(&skipped.name) {
                next.insert(skipped.name.clone(), prior.clone());
            }
        }
        for (name, value) in resolved {
            next.insert(name, value);
        }
        state.exports = next;

        SkippedExports {
            names: skipped.into_iter().map(|export| export.name).collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (HashMap<String, PersistedExportValue>, Vec<SkippedExport>) {
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
/// 1. Build `post_apply_states` from the live managed and data-source maps,
///    overlaying managed entries derived from `state.resources` so applied
///    attributes win over any pre-apply managed snapshot (carina#3266).
/// 2. Split `sorted_resources` into a managed view (Managed +
///    DataSource collapsed onto `Resource` by field projection)
///    and a composition view ([`LiveComposition::try_from`]).
/// 3. Build the bindings view from the managed slice via
///    [`ResolvedBindings::from_managed_with_state`] (#3176).
/// 4. Re-resolve each composition's `ResourceRef`s against that view via
///    [`resolve_live_composition_refs_post_apply`] (#3175), so a composition that
///    references a Replaced managed gets the *post*-replace value.
/// 5. Layer the re-resolved compositions onto the bindings view via
///    `layer_live_compositions_post_apply` (#3176).
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
pub(crate) struct ResolveExportValuesPostApplyInput<'a> {
    pub export_params: &'a carina_core::validation::ExportsCheckedPreApply<
        carina_core::parser::ExpandedInferredExportParam,
    >,
    pub sorted_resources: &'a [Resource],
    pub pre_resolve_compositions: &'a [carina_core::resource::LiveComposition],
    pub post_apply_states: &'a PostApplyStates,
    pub schemas: &'a SchemaRegistry,
    pub wait_aliases: &'a [carina_core::binding_index::WaitAliasSpec],
    pub provider_context: &'a carina_core::parser::LoadedProviderContext,
}

pub(crate) fn resolve_export_values_post_apply(
    input: ResolveExportValuesPostApplyInput<'_>,
) -> Result<
    carina_core::validation::CheckedExportValues<carina_core::parser::ExpandedInferredExportParam>,
    AppError,
> {
    use carina_core::binding_index::ResolvedBindings;
    use carina_core::resolver::resolve_live_composition_refs_post_apply;

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
    let mut compositions: Vec<carina_core::resource::LiveComposition> =
        input.pre_resolve_compositions.to_vec();

    // Step 3: build the bindings view from the managed slice plus
    // post-apply states, **with data sources but no compositions yet**.
    // The compositions' attribute maps still hold pre-apply
    // `ResourceRef`s and need to be re-resolved against the
    // post-apply state in Step 4; only after that re-resolution can
    // they be layered into the bindings view. We use `pre_apply`
    // with `compositions: &[]` here, then call
    // `layer_live_compositions_post_apply` once the re-resolution is done.
    // (carina#3181, carina#3248)
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        input.post_apply_states.managed_current().clone(),
        input.schemas,
        input.sorted_resources,
    );
    let mut bindings =
        ResolvedBindings::pre_apply_live(carina_core::binding_index::LivePreApplyInputs {
            managed: input.sorted_resources,
            compositions: &[],
            managed_current: &plan_input_current,
            data_source_values: input.post_apply_states.data_source_values(),
            remote_bindings: &HashMap::new(),
            wait_aliases: input.wait_aliases,
        });

    // Step 4: re-resolve each composition's attributes against the
    // post-apply bindings view. After this, a composition's
    // `role_arn = role.arn` no longer holds the pre-apply
    // `ResourceRef`; it holds the post-apply concrete value.
    resolve_live_composition_refs_post_apply(&mut compositions, &bindings)?;

    // Step 5: layer the re-resolved compositions onto the bindings view.
    // Now `exports { foo = some_module.role_arn }` resolves against
    // a binding whose `role_arn` is the post-apply value.
    bindings.layer_live_compositions_post_apply(&compositions)?;

    // Step 6: resolve the export expressions against the combined
    // view. The input is already checked at the pre-apply boundary, but a
    // reference can become concrete only after a successful mutation. Run the
    // same constructor again so the post-apply value is checked before the
    // serializer can observe it.
    carina_core::validation::resolve_export_values_post_apply(
        input.export_params,
        &bindings,
        input.provider_context,
    )
    .map_err(AppError::Validation)
}

/// Convert post-apply checked export values into the state writeback shape.
///
/// Requiring [`carina_core::validation::CheckedExportValues`] here makes it
/// impossible to serialize the pre-apply display snapshot.
pub(crate) fn materialize_exports(
    export_params: &carina_core::validation::CheckedExportValues<
        carina_core::parser::ExpandedInferredExportParam,
    >,
) -> Result<ExportResolution, AppError> {
    use carina_core::value::SerializationError;

    let mut resolved_exports = HashMap::new();
    let mut skipped = Vec::new();
    for param in export_params.iter() {
        if let Some(ref value) = param.value {
            match PersistedExportValue::encode(value) {
                Ok(json) => {
                    resolved_exports.insert(param.name.clone(), json);
                }
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
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(ExportResolution {
        resolved: resolved_exports,
        skipped,
    })
}

pub(crate) struct ApplyStateSave<'a, S = ModuleSource> {
    pub state_file: StateFile,
    pub sorted_resources: &'a [ResolvedResource<S>],
    pub runtime_synthesized_resources: &'a [ResolvedResource<S>],
    pub managed_current: &'a HashMap<ResourceId, Current>,
    pub applied_states: &'a HashMap<ResourceId, Applied>,
    pub plan: &'a Plan<S>,
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
/// `Effect::Move` finalizes the address transfer after Phase 1 writes the
/// `to` row. For
/// Move+Replace / Move+Update / Move+Create the `to` row is fed by
/// `applied_states[to]` (the provider's post-apply State); for a pure
/// rename, by `managed_current[to]` (which `materialize_moved_states`
/// has already populated from the `from` row before writeback runs). The
/// final typed move operation merges every source-side deposed generation
/// not deleted by this apply into `to`, then drops the stale `from` row. See
/// carina#3170 for the overwrite bug class this prevents.
pub(crate) struct WritebackPlan<'a, S = ModuleSource> {
    upserts: indexmap::IndexMap<ResourceId, PlannedUpsert<'a, S>>,
    deposes: Vec<PlannedDepose>,
    remove_deposed: Vec<PlannedRemoveDeposed>,
    clear_currents: HashSet<ResolvedResourceId>,
    cleanups: HashSet<ResolvedResourceId>,
    moved_from_rows: HashSet<ResolvedResourceId>,
}

/// One planned upsert. Carrying the desired `&ResolvedResource` here (rather
/// than re-deriving it from `sorted_resources` in the apply loop)
/// makes the "every upsert has a desired resource" invariant
/// representable in the type — there is no separate lookup that can
/// miss.
struct PlannedUpsert<'a, S = ModuleSource> {
    resource: &'a ResolvedResource<S>,
    source: UpsertSource<'a>,
}

/// Provenance of a row write. Keeping these cases separate prevents an
/// observed failure or continuation from taking the complete-state path.
enum UpsertSource<'a> {
    Complete {
        state: &'a State,
        request: &'a AcceptedRequest,
    },
    Observed(&'a ObservedCreate),
    CreateInFlight(&'a InFlightCreate),
    Imported(&'a State),
    CurrentPresent(&'a State),
    CurrentCreateInFlight(&'a InFlightCreate),
    CurrentMissingIdentifier(&'a State),
}

impl<'a> UpsertSource<'a> {
    fn created_identifier(&self) -> Option<&str> {
        match self {
            Self::Complete { state, .. } | Self::Imported(state) => state.identifier.as_deref(),
            Self::Observed(created) => Some(created.identifier().as_str()),
            Self::CreateInFlight(pending) => pending.identifier().map(|value| value.as_str()),
            Self::CurrentPresent(_state) => None,
            Self::CurrentCreateInFlight(_pending) => None,
            Self::CurrentMissingIdentifier(_state) => None,
        }
    }

    fn is_create_result(&self) -> bool {
        match self {
            Self::Complete { .. } => true,
            Self::Observed(_created) => true,
            Self::CreateInFlight(pending) => {
                matches!(pending.persistence(), InFlightPersistence::Fresh(_))
            }
            Self::Imported(_state) => true,
            Self::CurrentPresent(_state) => false,
            Self::CurrentCreateInFlight(_pending) => false,
            Self::CurrentMissingIdentifier(_state) => false,
        }
    }

    fn preserves_row_after_delete(&self) -> bool {
        match self {
            Self::Observed(_created) => true,
            Self::CreateInFlight(_pending) => true,
            Self::CurrentCreateInFlight(_pending) => true,
            Self::Complete { .. } => false,
            Self::Imported(_state) => false,
            Self::CurrentPresent(_state) => false,
            Self::CurrentMissingIdentifier(_state) => false,
        }
    }

    fn records_name_overrides(&self) -> bool {
        self.is_create_result()
    }
}

struct PlannedDepose {
    id: ResolvedResourceId,
    instance: DeposedInstance,
}

struct PlannedRemoveDeposed {
    id: ResolvedResourceId,
    key: DeposedKey,
}

enum ReplacementOutcome {
    CurrentRowOnly,
    CreateAppliedDeposeOld(PlannedDepose),
    DeleteSucceededCreateAbsent(ResolvedResourceId),
    KeepOld,
}

enum ReplacementCreateResult<'upsert, 'resource, S = ModuleSource> {
    Upsert(&'upsert PlannedUpsert<'resource, S>),
    Ended,
    Absent,
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

impl<'a, S> WritebackPlan<'a, S> {
    fn new() -> Self {
        Self {
            upserts: indexmap::IndexMap::new(),
            deposes: Vec::new(),
            remove_deposed: Vec::new(),
            clear_currents: HashSet::new(),
            cleanups: HashSet::new(),
            moved_from_rows: HashSet::new(),
        }
    }

    fn claims(&self, id: &ResourceId, except: WriteClaim) -> bool {
        (except != WriteClaim::Upsert && self.upserts.contains_key(id))
            || (except != WriteClaim::ClearCurrent
                && self
                    .clear_currents
                    .iter()
                    .any(|claimed| claimed.as_inner() == id))
            || (except != WriteClaim::Cleanup
                && self.cleanups.iter().any(|claimed| claimed.as_inner() == id))
            || (except != WriteClaim::MovedFromRow
                && self
                    .moved_from_rows
                    .iter()
                    .any(|claimed| claimed.as_inner() == id))
    }

    /// Register an upsert for `resource` with `source`. The first call
    /// wins for this id; subsequent calls return `DuplicateUpsert`.
    /// Calling after `add_cleanup` for the same id returns
    /// `UpsertCleanupOverlap`.
    fn add_upsert(
        &mut self,
        resource: &'a ResolvedResource<S>,
        source: UpsertSource<'a>,
    ) -> Result<(), WritebackConflict> {
        let id = resource.resolved_id();
        if self.claims(id.as_inner(), WriteClaim::Upsert) {
            return Err(WritebackConflict::UpsertCleanupOverlap {
                id: id.into_inner(),
            });
        }
        if self.upserts.contains_key(id.as_inner()) {
            return Err(WritebackConflict::DuplicateUpsert {
                id: id.into_inner(),
            });
        }
        self.upserts
            .insert(id.into_inner(), PlannedUpsert { resource, source });
        Ok(())
    }

    /// Register a cleanup against `id`. Calling after `add_upsert(id)`
    /// returns `UpsertCleanupOverlap`. Cleanup is idempotent.
    fn add_cleanup(&mut self, id: ResolvedResourceId) -> Result<(), WritebackConflict> {
        if self.claims(id.as_inner(), WriteClaim::Cleanup) {
            return Err(WritebackConflict::UpsertCleanupOverlap {
                id: id.into_inner(),
            });
        }
        self.cleanups.insert(id);
        Ok(())
    }

    /// Register a provider verdict that clears the current create slot while
    /// retaining address metadata for any deposed generations.
    fn add_clear_current(&mut self, id: ResolvedResourceId) -> Result<(), WritebackConflict> {
        if self.claims(id.as_inner(), WriteClaim::ClearCurrent) {
            return Err(WritebackConflict::UpsertCleanupOverlap {
                id: id.into_inner(),
            });
        }
        self.clear_currents.insert(id);
        Ok(())
    }

    /// Claim a move source for the final typed row transfer. Unlike ordinary
    /// cleanup, finalization moves any still-live deposed generations to the
    /// destination before removing this whole source row.
    fn add_moved_from_row(&mut self, id: ResolvedResourceId) -> Result<(), WritebackConflict> {
        if self.claims(id.as_inner(), WriteClaim::MovedFromRow) {
            return Err(WritebackConflict::UpsertCleanupOverlap {
                id: id.into_inner(),
            });
        }
        self.moved_from_rows.insert(id);
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
                self.upserts.shift_remove(id.as_inner());
                self.add_cleanup(id)
            }
        }
    }

    fn add_remove_deposed(&mut self, id: ResolvedResourceId, key: DeposedKey) {
        self.remove_deposed.push(PlannedRemoveDeposed { id, key });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteClaim {
    Upsert,
    ClearCurrent,
    Cleanup,
    MovedFromRow,
}

struct ReplacementDeleteInput<'a> {
    id: &'a ResolvedResourceId,
    identifier: &'a str,
    provider_instance: Option<String>,
    generation: &'a EffectGeneration,
    fallback_dependency_bindings: BTreeSet<String>,
    previous_attributes: Option<ReplacementPreviousAttributesForStateWriteback<'a>>,
}

static CURRENT_GENERATION: EffectGeneration = EffectGeneration::Current;

fn delete_succeeded(
    successfully_deleted: &HashSet<DeletedInstanceKey>,
    id: &ResolvedResourceId,
    generation: &EffectGeneration,
    identifier: &str,
) -> bool {
    successfully_deleted.contains(&DeletedInstanceKey::new(
        id.clone().into_inner(),
        generation.clone(),
        identifier.to_string(),
    ))
}

fn identifier_was_successfully_deleted(
    successfully_deleted: &HashSet<DeletedInstanceKey>,
    id: &ResolvedResourceId,
    provider_instance: &Option<String>,
    identifier: &str,
) -> bool {
    let mut delete_id = id.as_inner().clone();
    delete_id.provider_instance = provider_instance.clone();
    successfully_deleted.contains(&DeletedInstanceKey::current(
        delete_id,
        identifier.to_string(),
    ))
}

fn classify_replacement_outcome<S>(
    delete: ReplacementDeleteInput<'_>,
    create_result: ReplacementCreateResult<'_, '_, S>,
    state_file: &StateFile,
    managed_current: &HashMap<ResourceId, Current>,
    moved_origins: &MovedOrigins,
    successfully_deleted: &HashSet<DeletedInstanceKey>,
    schemas: &SchemaRegistry,
) -> ReplacementOutcome {
    let create_upsert = match create_result {
        ReplacementCreateResult::Upsert(upsert) => Some(upsert),
        ReplacementCreateResult::Ended => return ReplacementOutcome::CurrentRowOnly,
        ReplacementCreateResult::Absent => None,
    };
    let create_result = create_upsert.filter(|upsert| upsert.source.is_create_result());
    match create_result {
        Some(_upsert)
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
            let new_identifier = upsert.source.created_identifier();
            if new_identifier == Some(delete.identifier) {
                return ReplacementOutcome::CurrentRowOnly;
            }
            ReplacementOutcome::CreateAppliedDeposeOld(planned_depose(
                delete,
                upsert.resource,
                schemas
                    .get_for(upsert.resource)
                    .map(|schema| schema.schema().as_schema()),
                state_file,
                managed_current,
                moved_origins,
            ))
        }
        None if delete_succeeded(
            successfully_deleted,
            delete.id,
            delete.generation,
            delete.identifier,
        ) && create_upsert
            .is_some_and(|upsert| upsert.source.preserves_row_after_delete()) =>
        {
            ReplacementOutcome::CurrentRowOnly
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

fn replacement_create_result<'upsert, 'resource, S>(
    id: &ResourceId,
    upserts: &'upsert indexmap::IndexMap<ResourceId, PlannedUpsert<'resource, S>>,
    applied_states: &HashMap<ResourceId, Applied>,
) -> ReplacementCreateResult<'upsert, 'resource, S> {
    if let Some(upsert) = upserts.get(id) {
        return ReplacementCreateResult::Upsert(upsert);
    }
    match applied_states.get(id) {
        Some(Applied::CreateEnded) => ReplacementCreateResult::Ended,
        Some(
            Applied::Complete { .. }
            | Applied::Observed(_)
            | Applied::CreateInFlight(_)
            | Applied::Imported(_)
            | Applied::DataSource(_)
            | Applied::WaitSnapshot(_),
        ) => ReplacementCreateResult::Absent,
        None => ReplacementCreateResult::Absent,
    }
}

fn planned_depose<S>(
    delete: ReplacementDeleteInput<'_>,
    desired_resource: &ResolvedResource<S>,
    schema: Option<&ResourceSchema>,
    state_file: &StateFile,
    managed_current: &HashMap<ResourceId, Current>,
    moved_origins: &MovedOrigins,
) -> PlannedDepose {
    let existing = moved_origins.find_persisted_row_at_address(state_file, delete.id.as_inner());
    let current = managed_current.get(delete.id.as_inner());
    let current_state = current.and_then(|current| match current {
        Current::Absent(state) => Some(state.state()),
        Current::Present(present) => Some(present.state()),
        Current::CreateInFlight(_pending) => None,
        Current::MissingIdentifier(state) => Some(state.state()),
    });
    let existing_attributes = existing.and_then(ResourceState::persisted_attributes);

    let attributes = if let Some(existing) = existing
        && existing.identifier().map(AppliedIdentifier::as_str) == Some(delete.identifier)
    {
        // Desired secret shapes only mask values carried by the old
        // generation. Compare-only hashes never let a newly desired or
        // provider-echoed write-only value enter this snapshot.
        existing.deposed_attributes_from_current_generation(desired_resource, schema)
    } else {
        let previous_hash_authority = existing_attributes
            .map(PreviousSecretHashAuthority::AllPreviouslyHashedKeys)
            .unwrap_or(PreviousSecretHashAuthority::None);
        if let Some(previous) = delete.previous_attributes {
            ResourceState::deposed_attributes_from_replacement_history(
                desired_resource,
                schema,
                previous,
                previous_hash_authority,
            )
        } else if let Some(current) = current_state {
            ResourceState::deposed_attributes_without_previous(
                desired_resource,
                schema,
                &current.attributes,
                previous_hash_authority,
            )
        } else {
            DeposedAttributes::default()
        }
    };

    let dependency_bindings = existing
        .map(|rs| rs.dependency_bindings.clone())
        .filter(|bindings| !bindings.is_empty())
        .or_else(|| current_state.map(|state| state.dependency_bindings.clone()))
        .filter(|bindings| !bindings.is_empty())
        .unwrap_or(delete.fallback_dependency_bindings);

    PlannedDepose {
        id: delete.id.clone(),
        instance: DeposedInstance::from_persisted_attributes(
            DeposedKey::new_unique(),
            delete.identifier.to_string(),
            delete.provider_instance,
            attributes,
            dependency_bindings,
        ),
    }
}

fn delete_input_from_effect<'a, S>(
    effect: &'a Effect<S>,
    previous_attributes: Option<ReplacementPreviousAttributesForStateWriteback<'a>>,
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
        id,
        identifier: identifier.as_str(),
        provider_instance: id.as_inner().provider_instance.clone(),
        generation,
        fallback_dependency_bindings: dependencies.iter().cloned().collect(),
        previous_attributes,
    })
}

fn delete_input_from_deferred(delete: &DeferredReplaceDelete) -> ReplacementDeleteInput<'_> {
    ReplacementDeleteInput {
        id: &delete.id,
        identifier: delete.identifier.as_str(),
        provider_instance: delete.id.as_inner().provider_instance.clone(),
        generation: &CURRENT_GENERATION,
        fallback_dependency_bindings: delete.dependencies.iter().cloned().collect(),
        previous_attributes: None,
    }
}

fn create_side_upsert_ids<S>(plan: &Plan<S>) -> HashSet<ResourceId> {
    let mut ids = HashSet::new();
    for effect in plan.effects() {
        match effect {
            Effect::Create(resource) => {
                ids.insert(resource.id.clone());
            }
            Effect::ResumeCreate { resource, .. } => {
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
            Effect::Read { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Import { .. }
            | Effect::DeferredCreate { .. }
            | Effect::Wait { .. } => {}
        }
    }
    ids
}

fn plan_displaced_identifier_deposes<S>(
    wb: &mut WritebackPlan<'_, S>,
    input: &DecomposeInput<'_, '_, S>,
    moved_origins: &MovedOrigins,
) {
    let create_side_ids = create_side_upsert_ids(input.plan);
    for (id, upsert) in &wb.upserts {
        if !upsert.source.is_create_result() {
            continue;
        }
        if !create_side_ids.contains(id) {
            continue;
        }
        let id = upsert.resource.resolved_id();
        let Some(existing) =
            moved_origins.find_persisted_row_at_address(input.state_file, id.as_inner())
        else {
            continue;
        };
        let Some(displaced_identifier) = existing.identifier().map(AppliedIdentifier::as_str)
        else {
            continue;
        };
        if !input.failed_refreshes.contains(id.as_inner())
            && input
                .managed_current
                .get(id.as_inner())
                .is_some_and(|current| match current {
                    Current::Absent(_state) => true,
                    Current::Present(_present) => false,
                    Current::CreateInFlight(_pending) => false,
                    Current::MissingIdentifier(_state) => false,
                })
        {
            continue;
        }
        let new_identifier = upsert.source.created_identifier();
        if new_identifier == Some(displaced_identifier) {
            continue;
        }
        if identifier_was_successfully_deleted(
            input.successfully_deleted,
            &id,
            &existing.directives.provider_instance,
            displaced_identifier,
        ) {
            continue;
        }
        if wb.deposes.iter().any(|depose| {
            depose.id == id
                && depose.instance.identifier == displaced_identifier
                && depose.instance.provider_instance == existing.directives.provider_instance
        }) {
            continue;
        }

        wb.deposes.push(planned_depose(
            ReplacementDeleteInput {
                id: &id,
                identifier: displaced_identifier,
                provider_instance: existing.directives.provider_instance.clone(),
                generation: &EffectGeneration::Current,
                fallback_dependency_bindings: existing.dependency_bindings.clone(),
                previous_attributes: None,
            },
            upsert.resource,
            input
                .schemas
                .get_for(upsert.resource)
                .map(|schema| schema.schema().as_schema()),
            input.state_file,
            input.managed_current,
            moved_origins,
        ));
    }
}

/// Build the typed writeback plan from the raw apply inputs.
///
/// Phase 1 (`sorted_resources` walk) registers upserts from
/// `applied_states` first, falling back to `managed_current` when the
/// resource exists but had no provider call, and emitting cleanups
/// when the resource was absent post-refresh. Phase 2 (`plan.effects()`
/// walk) classifies replacement outcomes for decomposed replacements
/// and `DeferredReplace`, then registers ordinary cleanups for
/// `Delete` / `Remove` / `Move`'s `from`. `Effect::Move` deliberately
/// does **not** touch the `to` slot — that is Phase 1's job.
struct DecomposeInput<'a, 's, S = ModuleSource> {
    state_file: &'s StateFile,
    sorted_resources: &'a [ResolvedResource<S>],
    runtime_synthesized_resources: &'a [ResolvedResource<S>],
    managed_current: &'a HashMap<ResourceId, Current>,
    applied_states: &'a HashMap<ResourceId, Applied>,
    plan: &'a Plan<S>,
    successfully_deleted: &'a HashSet<DeletedInstanceKey>,
    failed_refreshes: &'a HashSet<ResourceId>,
    schemas: &'a SchemaRegistry,
}

#[cfg(test)]
fn decompose<'a, S>(
    input: DecomposeInput<'a, '_, S>,
) -> Result<WritebackPlan<'a, S>, WritebackConflict> {
    let moved_origins = MovedOrigins::from_plan(input.plan);
    decompose_with_moved_origins(input, &moved_origins)
}

fn decompose_with_moved_origins<'a, S>(
    input: DecomposeInput<'a, '_, S>,
    moved_origins: &MovedOrigins,
) -> Result<WritebackPlan<'a, S>, WritebackConflict> {
    let state_file = input.state_file;
    let sorted_resources = input.sorted_resources;
    let runtime_synthesized_resources = input.runtime_synthesized_resources;
    let managed_current = input.managed_current;
    let applied_states = input.applied_states;
    let plan = input.plan;
    let successfully_deleted = input.successfully_deleted;
    let failed_refreshes = input.failed_refreshes;
    let schemas = input.schemas;
    let mut wb = WritebackPlan::new();

    for (id, key) in plan.abandoned_deposed_generations() {
        wb.add_remove_deposed(id.clone(), key.clone());
    }

    fn add_applied<'a, S>(
        wb: &mut WritebackPlan<'a, S>,
        resource: &'a ResolvedResource<S>,
        applied: &'a Applied,
    ) -> Result<(), WritebackConflict> {
        match applied {
            Applied::Complete { state, request } => {
                wb.add_upsert(resource, UpsertSource::Complete { state, request })
            }
            Applied::Observed(created) => wb.add_upsert(resource, UpsertSource::Observed(created)),
            Applied::CreateInFlight(pending) => {
                wb.add_upsert(resource, UpsertSource::CreateInFlight(pending))
            }
            Applied::CreateEnded => wb.add_clear_current(resource.resolved_id()),
            Applied::Imported(state) => wb.add_upsert(resource, UpsertSource::Imported(state)),
            Applied::DataSource(_state) | Applied::WaitSnapshot(_state) => Ok(()),
        }
    }

    for resource in sorted_resources {
        if let Some(applied) = applied_states.get(&resource.id) {
            add_applied(&mut wb, resource, applied)?;
        } else if failed_refreshes.contains(&resource.id) {
            // Refresh failed; we don't know whether the live resource
            // still exists, so leave any pre-existing row untouched.
            continue;
        } else if let Some(current) = managed_current.get(&resource.id) {
            match current {
                Current::Absent(_state) => wb.add_cleanup(resource.resolved_id())?,
                Current::Present(present) => {
                    wb.add_upsert(resource, UpsertSource::CurrentPresent(present.state()))?
                }
                Current::CreateInFlight(pending) => {
                    wb.add_upsert(resource, UpsertSource::CurrentCreateInFlight(pending))?
                }
                Current::MissingIdentifier(state) => wb.add_upsert(
                    resource,
                    UpsertSource::CurrentMissingIdentifier(state.state()),
                )?,
            }
        }
    }

    for resource in runtime_synthesized_resources {
        if let Some(applied) = applied_states.get(&resource.id) {
            add_applied(&mut wb, resource, applied)?;
        }
    }

    let replacement_by_delete_idx: HashMap<usize, ReplaceDisplayInfo<'_>> = plan
        .replace_display_info()
        .map(|replacement| (replacement.delete_idx, replacement))
        .collect();

    for (idx, effect) in plan.effects().iter().enumerate() {
        if let Some(replacement) = replacement_by_delete_idx.get(&idx) {
            let previous_attributes =
                plan.replacement_previous_attributes_for_state_writeback(replacement.delete_idx);
            if let Some(delete) = delete_input_from_effect(effect, previous_attributes) {
                let create_id = plan.effects()[replacement.create_idx].resource_id();
                let outcome = classify_replacement_outcome(
                    delete,
                    replacement_create_result(create_id, &wb.upserts, applied_states),
                    state_file,
                    managed_current,
                    moved_origins,
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
                let create_result =
                    replacement_create_result(delete.id.as_inner(), &wb.upserts, applied_states);
                let outcome = classify_replacement_outcome(
                    delete,
                    create_result,
                    state_file,
                    managed_current,
                    moved_origins,
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
            && delete_succeeded(successfully_deleted, id, generation, identifier.as_str())
        {
            wb.add_remove_deposed(id.clone(), key.clone());
        }

        if let Effect::Move { from, .. } = effect {
            wb.add_moved_from_row(from.clone())?;
        } else {
            for id in effect.writeback_cleanup_ids(successfully_deleted) {
                wb.add_cleanup(id)?;
            }
        }
    }

    plan_displaced_identifier_deposes(&mut wb, &input, moved_origins);

    Ok(wb)
}

pub(crate) fn build_state_after_apply<S: Clone>(
    save: ApplyStateSave<'_, S>,
    planned_secret_hashes: Option<&HashMap<ResourceId, PersistedAttributes>>,
) -> Result<StateFile, AppError> {
    let ApplyStateSave {
        state_file,
        sorted_resources,
        runtime_synthesized_resources,
        managed_current,
        applied_states,
        plan,
        successfully_deleted,
        failed_refreshes,
        schemas,
    } = save;
    let mut state = state_file;
    let moved_origins = MovedOrigins::from_plan(plan);

    let writeback = decompose_with_moved_origins(
        DecomposeInput {
            state_file: &state,
            sorted_resources,
            runtime_synthesized_resources,
            managed_current,
            applied_states,
            plan,
            successfully_deleted,
            failed_refreshes,
            schemas,
        },
        &moved_origins,
    )?;
    let permanent_name_overrides = plan.permanent_name_overrides_for_state();
    for (id, planned) in &writeback.upserts {
        let resource = planned.resource;
        let existing = moved_origins.find_persisted_row(&state, id);
        let preferred_hashes = planned_secret_hashes.and_then(|hashes| hashes.get(id));
        let schema = schemas
            .get_for(resource)
            .map(|schema| schema.schema().as_schema());
        let mut resource_state = match &planned.source {
            UpsertSource::Complete { state, request } => {
                ResourceState::from_provider_state_for_resource_and_schema(
                    resource,
                    state,
                    existing,
                    schema,
                    RowBasis::Applied(request),
                    preferred_hashes,
                )
                .map_err(|source| AppError::from_resource_state_encoding(id.clone(), source))?
            }
            UpsertSource::Imported(state) => {
                // Import sends no resource attributes, so it establishes no
                // write-only baseline. An updatable key is sent by the next
                // plan; a create-only key remains subject to the #1232 guard.
                ResourceState::from_provider_state_for_resource_and_schema(
                    resource,
                    state,
                    existing,
                    schema,
                    RowBasis::Imported,
                    preferred_hashes,
                )
                .map_err(|source| AppError::from_resource_state_encoding(id.clone(), source))?
            }
            UpsertSource::CurrentPresent(state) | UpsertSource::CurrentMissingIdentifier(state) => {
                ResourceState::from_provider_state_for_resource_and_schema(
                    resource,
                    state,
                    existing,
                    schema,
                    RowBasis::Carried,
                    preferred_hashes,
                )
                .map_err(|source| AppError::from_resource_state_encoding(id.clone(), source))?
            }
            UpsertSource::Observed(created) => ResourceState::from_observation(
                resource,
                created,
                existing,
                schema,
                preferred_hashes,
            )
            .map_err(|source| AppError::from_resource_state_encoding(id.clone(), source))?,
            UpsertSource::CreateInFlight(pending)
            | UpsertSource::CurrentCreateInFlight(pending) => {
                ResourceState::from_in_flight(resource, pending, existing, schema, preferred_hashes)
                    .map_err(|source| AppError::from_resource_state_encoding(id.clone(), source))?
            }
        };
        if planned.source.records_name_overrides()
            && let Some(overrides) = permanent_name_overrides.get(id)
        {
            resource_state.name_overrides = overrides.clone();
        }
        state.upsert_resource(resource_state)?;
    }

    for planned in writeback.deposes {
        apply_planned_depose(&mut state, planned)?;
    }

    for planned in writeback.remove_deposed {
        state.remove_deposed_generation(
            &planned.id.provider,
            &planned.id.resource_type,
            planned.id.identity_str(),
            &planned.key,
        );
    }

    for id in &writeback.clear_currents {
        state.clear_current_instance_preserving_address_metadata(
            &id.provider,
            &id.resource_type,
            id.identity_str(),
        );
    }

    for id in &writeback.cleanups {
        state.remove_resource(&id.provider, &id.resource_type, id.identity_str());
    }

    for effect in plan.effects() {
        if let Some(authority) = effect.move_authority() {
            state.complete_resource_move(authority)?;
        }
    }

    Ok(state)
}

fn apply_planned_depose(state: &mut StateFile, planned: PlannedDepose) -> Result<(), AppError> {
    let row_provider_instance = planned.instance.provider_instance.clone();
    state.upsert_deposed_generation(
        &planned.id.provider,
        &planned.id.resource_type,
        planned.id.identity(),
        row_provider_instance,
        planned.instance,
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestroyStateWrite {
    Checkpoint,
    Final,
}

/// Apply destroy results to the state file.
pub(crate) fn apply_destroy_to_state(
    state: &mut carina_state::StateFile,
    destroyed: &[DestroyedInstance],
    write: DestroyStateWrite,
) {
    for destroyed in destroyed {
        match &destroyed.generation {
            EffectGeneration::Current => {
                state.remove_resource(
                    &destroyed.id.provider,
                    &destroyed.id.resource_type,
                    destroyed.id.identity_str(),
                );
            }
            EffectGeneration::Deposed(key) => {
                state.remove_deposed_generation(
                    &destroyed.id.provider,
                    &destroyed.id.resource_type,
                    destroyed.id.identity_str(),
                    key,
                );
            }
        }
    }
    if write == DestroyStateWrite::Final {
        state.exports.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DestroyedInstance {
    pub id: ResolvedResourceId,
    pub generation: EffectGeneration,
}

/// Build a minimal `Resource` for an orphaned resource from the state file.
///
/// Provider attributes and host-owned metadata come from the same persisted
/// row, so callers cannot join independently-built orphan views.
pub(crate) fn build_orphan_resource(
    sf: &carina_state::StateFile,
    id: &ResolvedResourceId,
) -> Resource {
    let rs = sf
        .find_resource(&id.provider, &id.resource_type, id.identity_str())
        .expect("orphan must exist in state file");
    let attributes = rs
        .persisted_attributes()
        .map(PersistedAttributes::to_values)
        .unwrap_or_default();
    let mut resource = Resource::from_id(id.as_inner().clone());
    resource.attributes = attributes.into_iter().collect();
    resource.directives = rs.directives.clone().into();
    resource.prefixes = rs.prefixes.clone();
    resource.binding = rs.binding.clone();
    resource.dependency_bindings = rs.dependency_bindings.clone();
    resource
}

#[cfg(test)]
mod post_apply_states_tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, ResourceId, State, Value};
    use carina_state::{DeposedInstance, DeposedKey, ResourceState, StateFile};

    /// Type-level contract pin for the carina#3266 + carina#3271 merge rule.
    ///
    /// `from_current_and_state` must:
    /// 1. Preserve every entry in `data_source_states`; these transient reads
    ///    are never persisted to `state.resources`.
    /// 2. For managed entries present in both `managed_current` and
    ///    `state.resources`, pick the value derived from `state.resources` —
    ///    that is the post-apply writeback value, while `managed_current` may
    ///    still hold the pre-apply snapshot.
    #[test]
    fn data_source_states_survive_without_state_resource_rows() {
        let ds_id =
            ResourceId::with_provider_identity("aws", "iam.Roles", "admin_access_roles", None);
        let mut ds_attrs = HashMap::new();
        ds_attrs.insert(
            "arns".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("arn:aws:iam::1:role/x".into()),
            )])),
        );
        let data_sources = [carina_core::resource::DataSource::with_provider(
            "aws",
            "iam.Roles",
            "admin_access_roles",
            None,
        )];
        let data_source_values =
            DataSourceValues::from_data_sources(&data_sources, |data_source| {
                carina_core::data_source_value::DataSourceValue::read(State::existing(
                    data_source.id.clone(),
                    ds_attrs.clone(),
                ))
            });

        // state.resources has no entry for the data source — that is
        // the production reality the bug springs from.
        let state = StateFile::new();

        let post =
            PostApplyStates::from_current_and_state(&HashMap::new(), &data_source_values, &state);
        let entry = post
            .data_source_values()
            .read_states()
            .find_map(|(id, state)| (id == &ds_id).then_some(state))
            .expect("data-source entry must survive merge");
        assert_eq!(entry.attributes.get("arns"), ds_attrs.get("arns"));
    }

    #[test]
    fn state_resources_identifier_row_is_present_and_wins_over_current_state() {
        let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "log", None);

        // managed_current carries the PRE-apply attribute value.
        let mut pre_attrs = HashMap::new();
        pre_attrs.insert(
            "versioning".to_string(),
            Value::Concrete(ConcreteValue::String("Suspended".into())),
        );
        let mut managed_states = HashMap::new();
        managed_states.insert(id.clone(), State::existing(id.clone(), pre_attrs));

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
        let state = carina_state::check_and_migrate(&json.to_string())
            .unwrap()
            .into_state();

        let managed_current = crate::managed_current_from_states(managed_states);
        let post = PostApplyStates::from_current_and_state(
            &managed_current,
            &DataSourceValues::empty(),
            &state,
        );
        let current = post.managed_current().get(&id).expect("merged entry");
        let present = match current {
            Current::Absent(state) => {
                panic!("identifier row was classified absent: {state:?}")
            }
            Current::Present(present) => present,
            Current::CreateInFlight(pending) => {
                panic!("identifier row was classified in flight: {pending:?}")
            }
            Current::MissingIdentifier(state) => {
                panic!("identifier row lost its identifier: {state:?}")
            }
        };
        assert_eq!(present.identifier().as_str(), "log-bucket");
        let entry = present.state();
        match entry.attributes.get("versioning") {
            Some(Value::Concrete(ConcreteValue::String(s))) => assert_eq!(s, "Enabled"),
            other => panic!("expected post-apply 'Enabled', got: {other:?}"),
        }
    }

    #[test]
    fn deposed_only_shell_is_absent_from_post_apply_current() {
        let id = ResourceId::with_provider_identity("mock", "test.Resource", "main", None);
        let mut row = ResourceState::new("test.Resource", "main", "mock");
        row.deposed.push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            "old-id".to_string(),
            None,
            HashMap::new(),
            BTreeSet::new(),
        ));
        let mut state = StateFile::new();
        state
            .upsert_resource(row)
            .expect("deposed-only fixture must be valid");

        let post = PostApplyStates::from_current_and_state(
            &HashMap::new(),
            &DataSourceValues::empty(),
            &state,
        );
        let current = post
            .managed_current()
            .get(&id)
            .expect("deposed-only row must retain its address classification");

        match current {
            Current::Absent(state) => {
                assert!(!state.state().exists);
                assert!(state.state().attributes.is_empty());
            }
            Current::Present(present) => {
                panic!("deposed-only row was classified present: {present:?}")
            }
            Current::CreateInFlight(pending) => {
                panic!("deposed-only row was classified in flight: {pending:?}")
            }
            Current::MissingIdentifier(state) => {
                panic!("deposed-only row was treated as malformed current state: {state:?}")
            }
        }
    }

    #[test]
    fn identifierless_current_row_is_missing_identifier_post_apply() {
        let id = ResourceId::with_provider_identity("mock", "test.Resource", "main", None);
        let row = ResourceState::new("test.Resource", "main", "mock")
            .with_attribute("name", serde_json::json!("main"));
        let mut state = StateFile::new();
        state
            .upsert_resource(row)
            .expect("identifierless fixture must be valid");

        let post = PostApplyStates::from_current_and_state(
            &HashMap::new(),
            &DataSourceValues::empty(),
            &state,
        );
        let current = post
            .managed_current()
            .get(&id)
            .expect("identifierless row must retain its address classification");

        match current {
            Current::Absent(state) => {
                panic!("malformed current row was classified absent: {state:?}")
            }
            Current::Present(present) => {
                panic!("identifierless row was classified present: {present:?}")
            }
            Current::CreateInFlight(pending) => {
                panic!("identifierless row was classified in flight: {pending:?}")
            }
            Current::MissingIdentifier(state) => {
                assert!(state.state().exists);
                assert_eq!(
                    state.state().attributes.get("name"),
                    Some(&Value::Concrete(ConcreteValue::String("main".into())))
                );
            }
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
    use carina_core::explicit::ExplicitFields;
    use carina_core::parser::{DeferredForExpression, ForBinding};
    use carina_core::plan::Plan;
    use carina_core::provider::ProviderRouter;
    use carina_core::resource::{
        ApplyTimeUnknown, ConcreteValue, DeferredValue, Directives, ResolvedResourceId, Resource,
        ResourceId, State, Value,
    };
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    use carina_state::{CurrentInstance, DeposedInstance, DeposedKey, NameOverride};

    const LEGACY_SECRET_PREFIX: &str = "_secret:argon2:";

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    fn complete_create(state: State) -> Applied {
        Applied::Complete {
            state,
            request: AcceptedRequest::create_for_test(),
        }
    }

    fn complete_update(state: State, sent: &[&str], removed: &[&str]) -> Applied {
        Applied::Complete {
            state,
            request: AcceptedRequest::update_for_test(sent, removed),
        }
    }

    fn structural_legacy_secret(hash_hex: &str) -> serde_json::Value {
        serde_json::json!({
            carina_core::value::PERSISTED_SECRET_KEY:
                format!("argon2-legacy:73616c74:{hash_hex}"),
        })
    }

    fn plan_with_name_override(id: &ResourceId, temp_value: &str, original_value: &str) -> Plan {
        let mut plan = Plan::new();
        plan.fixture_add_permanent_name_override(
            ResolvedResourceId::new(id.clone()),
            "name",
            temp_value,
            Some(original_value.to_string()),
        );
        plan
    }

    fn applied_name_state(id: &ResourceId, name: &str) -> State {
        State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String(name.to_string().into())),
            )]),
        )
        .with_identifier(format!("{name}-id"))
    }

    fn fresh_in_flight(
        id: &ResourceId,
        token: &str,
        identifier: Option<&str>,
        sent: carina_core::provider::Sent,
    ) -> carina_core::provider::InFlightCreate {
        crate::fixture_in_flight_create(id, token, identifier, sent)
    }

    fn secret_string(value: &str) -> Value {
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String(value.to_string().into()),
        ))))
    }

    fn observed_create(
        id: &ResourceId,
        attributes: HashMap<String, Value>,
        sent: carina_core::provider::Sent,
    ) -> carina_core::provider::ObservedCreate {
        crate::fixture_observed_create(id, attributes, sent)
    }

    #[derive(Clone, Copy)]
    enum ReplacementAppliedKind {
        Complete,
        Observed,
        CreateInFlight,
        CreateEnded,
    }

    fn replacement_matrix_state(kind: ReplacementAppliedKind, delete_first: bool) -> StateFile {
        let resource =
            Resource::with_provider("aws", "route53.Record", "validation_records[0]", None)
                .with_binding("validation_records[0]")
                .with_attribute(
                    "name",
                    Value::Concrete(ConcreteValue::String("new.example.com".into())),
                );
        let id = resource.id.clone();
        let resolved_resource = resolved(resource.clone());

        let mut state = match kind {
            ReplacementAppliedKind::Complete
            | ReplacementAppliedKind::Observed
            | ReplacementAppliedKind::CreateInFlight => {
                let row = ResourceState::try_applied(
                    "route53.Record",
                    "validation_records[0]",
                    "aws",
                    "old-record-id",
                )
                .expect("fixture identifier is non-empty")
                .with_attribute("name", serde_json::json!("old.example.com"));
                let mut state = StateFile::new();
                state
                    .upsert_resource(row)
                    .expect("old current fixture must be valid");
                state
            }
            ReplacementAppliedKind::CreateEnded => build_state_after_apply(
                ApplyStateSave {
                    state_file: StateFile::new(),
                    sorted_resources: std::slice::from_ref(&resolved_resource),
                    runtime_synthesized_resources: &[],
                    managed_current: &HashMap::new(),
                    applied_states: &HashMap::from([(
                        id.clone(),
                        Applied::CreateInFlight(fresh_in_flight(
                            &id,
                            "persisted-token",
                            Some("pending-id"),
                            carina_core::provider::Sent::AllRequestAttributes,
                        )),
                    )]),
                    plan: &Plan::new(),
                    successfully_deleted: &HashSet::new(),
                    failed_refreshes: &HashSet::new(),
                    schemas: &SchemaRegistry::new(),
                },
                None,
            )
            .expect("in-flight matrix fixture must be valid"),
        };

        state
            .upsert_deposed_generation(
                &id.provider,
                &id.resource_type,
                resolved_resource.identity(),
                id.provider_instance.clone(),
                DeposedInstance::from_persisted_json(
                    DeposedKey::new_unique(),
                    "older-deposed-id".to_string(),
                    None,
                    HashMap::from([("name".to_string(), serde_json::json!("older"))]),
                    BTreeSet::new(),
                ),
            )
            .expect("older deposed fixture must be valid");
        if matches!(kind, ReplacementAppliedKind::CreateEnded) && !delete_first {
            state
                .upsert_deposed_generation(
                    &id.provider,
                    &id.resource_type,
                    resolved_resource.identity(),
                    id.provider_instance.clone(),
                    DeposedInstance::from_persisted_json(
                        DeposedKey::new_unique(),
                        "old-record-id".to_string(),
                        None,
                        HashMap::from([("name".to_string(), serde_json::json!("old.example.com"))]),
                        BTreeSet::new(),
                    ),
                )
                .expect("CBD old generation fixture must be valid");
        }

        let managed_current = HashMap::from([(id.clone(), state.build_state_for_resource(&id))]);
        let applied = match kind {
            ReplacementAppliedKind::Complete => complete_create(
                State::existing(
                    id.clone(),
                    HashMap::from([(
                        "name".to_string(),
                        Value::Concrete(ConcreteValue::String("complete.example.com".into())),
                    )]),
                )
                .with_identifier("complete-id"),
            ),
            ReplacementAppliedKind::Observed => Applied::Observed(
                observed_create(
                    &id,
                    HashMap::from([(
                        "name".to_string(),
                        Value::Concrete(ConcreteValue::String("observed.example.com".into())),
                    )]),
                    carina_core::provider::Sent::AllRequestAttributes,
                )
                .into(),
            ),
            ReplacementAppliedKind::CreateInFlight => Applied::CreateInFlight(fresh_in_flight(
                &id,
                "new-token",
                Some("pending-id"),
                carina_core::provider::Sent::AllRequestAttributes,
            )),
            ReplacementAppliedKind::CreateEnded => Applied::CreateEnded,
        };
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let successfully_deleted = if delete_first {
            HashSet::from([DeletedInstanceKey::current(id.clone(), "old-record-id")])
        } else {
            HashSet::new()
        };

        build_state_after_apply(
            ApplyStateSave {
                state_file: state,
                sorted_resources: &[],
                runtime_synthesized_resources: std::slice::from_ref(&resolved_resource),
                managed_current: &managed_current,
                applied_states: &HashMap::from([(id, applied)]),
                plan: &plan,
                successfully_deleted: &successfully_deleted,
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("replacement matrix writeback must succeed")
    }

    fn assert_applied_current(row: &ResourceState, expected_identifier: &str) {
        match row.current() {
            CurrentInstance::Applied {
                identifier,
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => {
                assert_eq!(identifier.as_str(), expected_identifier);
                assert!(!attributes.is_empty());
                assert!(write_only_attributes.is_empty());
                assert!(nested_write_only.is_empty());
                assert!(partial_read.is_none());
            }
            CurrentInstance::None => panic!("expected an applied current instance, got none"),
            CurrentInstance::CreateInFlight { pending } => panic!(
                "expected applied current, got in-flight {:?} from {}",
                pending,
                pending.started_at().as_str()
            ),
            CurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => panic!(
                "expected applied current, got identifierless state ({attributes:?}, {write_only_attributes:?}, {nested_write_only:?}, {partial_read:?})"
            ),
        }
    }

    fn assert_in_flight_current(row: &ResourceState) {
        match row.current() {
            CurrentInstance::CreateInFlight { pending } => {
                assert_eq!(pending.token().expose_for_provider_call(), "new-token");
                assert_eq!(
                    pending.identifier().map(AppliedIdentifier::as_str),
                    Some("pending-id")
                );
                assert!(!pending.started_at().as_str().is_empty());
            }
            CurrentInstance::None => panic!("expected an in-flight current instance, got none"),
            CurrentInstance::Applied {
                identifier,
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => panic!(
                "expected in-flight current, got applied {} ({attributes:?}, {write_only_attributes:?}, {nested_write_only:?}, {partial_read:?})",
                identifier.as_str()
            ),
            CurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => panic!(
                "expected in-flight current, got identifierless state ({attributes:?}, {write_only_attributes:?}, {nested_write_only:?}, {partial_read:?})"
            ),
        }
    }

    fn assert_no_current(row: &ResourceState) {
        match row.current() {
            CurrentInstance::None => {}
            CurrentInstance::Applied {
                identifier,
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => panic!(
                "expected no current instance, got applied {} ({attributes:?}, {write_only_attributes:?}, {nested_write_only:?}, {partial_read:?})",
                identifier.as_str()
            ),
            CurrentInstance::CreateInFlight { pending } => panic!(
                "expected no current instance, got in-flight {:?} from {}",
                pending,
                pending.started_at().as_str()
            ),
            CurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => panic!(
                "expected no current instance, got identifierless state ({attributes:?}, {write_only_attributes:?}, {nested_write_only:?}, {partial_read:?})"
            ),
        }
    }

    fn assert_replacement_matrix_case(kind: ReplacementAppliedKind, delete_first: bool) {
        let state = replacement_matrix_state(kind, delete_first);
        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("deposed sentinel keeps the replacement row");
        match kind {
            ReplacementAppliedKind::Complete => assert_applied_current(row, "complete-id"),
            ReplacementAppliedKind::Observed => assert_applied_current(row, "created-id"),
            ReplacementAppliedKind::CreateInFlight => assert_in_flight_current(row),
            ReplacementAppliedKind::CreateEnded => assert_no_current(row),
        }
        assert!(
            row.deposed
                .iter()
                .any(|instance| instance.identifier == "older-deposed-id"),
            "unrelated deposed generation must survive"
        );
        assert_eq!(
            row.deposed
                .iter()
                .any(|instance| instance.identifier == "old-record-id"),
            !delete_first,
            "the replaced old instance is deposed only when its delete did not succeed"
        );
    }

    #[test]
    fn create_before_destroy_writeback_matrix_is_lossless() {
        for kind in [
            ReplacementAppliedKind::Complete,
            ReplacementAppliedKind::Observed,
            ReplacementAppliedKind::CreateInFlight,
            ReplacementAppliedKind::CreateEnded,
        ] {
            assert_replacement_matrix_case(kind, false);
        }
    }

    #[test]
    fn delete_first_writeback_matrix_is_lossless() {
        for kind in [
            ReplacementAppliedKind::Complete,
            ReplacementAppliedKind::Observed,
            ReplacementAppliedKind::CreateInFlight,
            ReplacementAppliedKind::CreateEnded,
        ] {
            assert_replacement_matrix_case(kind, true);
        }
    }

    #[test]
    fn repended_cbd_replacement_does_not_reclassify_the_old_generation() {
        let initial = replacement_matrix_state(ReplacementAppliedKind::CreateInFlight, false);
        let id = ResourceId::with_provider_identity(
            "aws",
            "route53.Record",
            "validation_records[0]",
            None,
        );
        let desired =
            Resource::with_provider("aws", "route53.Record", "validation_records[0]", None)
                .with_binding("validation_records[0]")
                .with_attribute(
                    "name",
                    Value::Concrete(ConcreteValue::String("changed.example.com".into())),
                );
        let resolved_desired = resolved(desired);
        let before = initial
            .find_resource_for_id(&id)
            .expect("initial replacement row")
            .clone();
        let Current::CreateInFlight(pending) = initial.build_state_for_resource(&id) else {
            panic!("matrix fixture must load an in-flight current instance");
        };
        let repended = crate::fixture_repend_in_flight_create(pending, "repended-token");
        let managed_current = HashMap::from([(id.clone(), initial.build_state_for_resource(&id))]);
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: initial,
                sorted_resources: &[],
                runtime_synthesized_resources: std::slice::from_ref(&resolved_desired),
                managed_current: &managed_current,
                applied_states: &HashMap::from([(id.clone(), Applied::CreateInFlight(repended))]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("re-pended replacement writeback");

        let after = saved
            .find_resource_for_id(&id)
            .expect("re-pended replacement row");
        assert_eq!(
            serde_json::to_value(&after.deposed).expect("deposed state serializes"),
            serde_json::to_value(&before.deposed).expect("deposed state serializes"),
            "re-pending must not regenerate or replace the old generation"
        );
        let CurrentInstance::CreateInFlight { pending } = after.current() else {
            panic!("re-pended replacement must remain in flight");
        };
        assert_eq!(pending.token().expose_for_provider_call(), "repended-token");
        let CurrentInstance::CreateInFlight {
            pending: before_pending,
        } = before.current()
        else {
            panic!("initial replacement must be in flight");
        };
        assert_eq!(pending.identifier(), before_pending.identifier());
        let SentRecord::Persisted(envelope) = pending.sent() else {
            panic!("written continuation must carry persisted values");
        };
        let SentRecord::Persisted(before_envelope) = before_pending.sent() else {
            panic!("loaded continuation must carry persisted values");
        };
        assert_eq!(envelope.write_only(), before_envelope.write_only());
        assert_eq!(envelope.secrets(), before_envelope.secrets());
        assert_eq!(pending.started_at(), before_pending.started_at());
    }

    fn plan_after_writeback(
        resource: &Resource,
        state: &StateFile,
        schemas: &SchemaRegistry,
    ) -> Plan {
        let current = HashMap::from([(
            resource.id.clone(),
            state.build_state_for_resource(&resource.id),
        )]);
        let plan_input = carina_core::resource::into_plan_input_current_map::<
            carina_core::resource::ModuleSource,
        >(current, schemas, &[]);
        let saved_attrs = state.build_saved_attrs().lift(schemas);
        create_plan(
            std::slice::from_ref(&resolved(resource.clone())),
            &[],
            &ProviderRouter::new(),
            carina_core::differ::PlanPriorStates::new(
                &plan_input,
                &carina_core::differ::Orphans::new(),
            ),
            &state.build_directives(),
            schemas,
            &saved_attrs,
            &state.build_explicit(),
            &[],
        )
        .expect("post-writeback plan should build")
    }

    fn write_only_password_schema() -> SchemaRegistry {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("test.resource")
                .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("replacement_key", AttributeType::string())
                        .create_only()
                        .build(),
                )
                .attribute(
                    AttributeSchema::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        schemas
    }

    fn state_after_accepted_create(
        resource: &Resource,
        provider_attributes: HashMap<String, Value>,
        schemas: &SchemaRegistry,
    ) -> StateFile {
        let id = resource.id.clone();
        build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: std::slice::from_ref(&resolved(resource.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    id.clone(),
                    complete_create(
                        State::existing(id, provider_attributes).with_identifier("provider-id"),
                    ),
                )]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas,
            },
            None,
        )
        .expect("accepted create fixture should write state")
    }

    fn state_with_write_only_password(id: &ResourceId, password: &str) -> StateFile {
        let row = ResourceState::try_applied(
            id.resource_type.clone(),
            id.identity().expect("fixture identity is resolved").clone(),
            id.provider.clone(),
            "provider-id",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("password", serde_json::json!(password));
        let row =
            crate::fixture_resource_state_metadata(row, Some(vec!["password".to_string()]), None);
        let mut state = StateFile::new();
        state
            .upsert_resource(row)
            .expect("write-only fixture row is valid");
        state
    }

    fn assert_stored_password(state: &StateFile, id: &ResourceId, expected: Option<&str>) {
        let attributes = state
            .find_resource_for_id(id)
            .and_then(ResourceState::persisted_attributes)
            .expect("fixture row has applied attributes");
        assert_eq!(
            attributes.get("password"),
            expected.map(|value| Value::Concrete(ConcreteValue::String(value.into())))
        );
    }

    #[test]
    fn failed_update_carries_previous_write_only_value_and_next_plan_retries_it() {
        let desired = Resource::with_provider("mock", "test.resource", "failed-update", None)
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("new".into())),
            );
        let id = desired.id.clone();
        let existing = state_with_write_only_password(&id, "old");
        let refreshed = State::existing(id.clone(), HashMap::new()).with_identifier("provider-id");
        let schemas = write_only_password_schema();

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::from([(id.clone(), Current::from_state(refreshed))]),
                applied_states: &HashMap::new(),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("failed-update refresh should write state");

        assert_stored_password(&saved, &id, Some("old"));
        let next_plan = plan_after_writeback(&desired, &saved, &schemas);
        assert!(next_plan.effects().iter().any(|effect| matches!(
            effect,
            Effect::Update {
                changed_attributes,
                ..
            } if changed_attributes.iter().any(|key| key == "password")
        )));
    }

    #[test]
    fn failed_update_carries_live_pending_removal_and_next_plan_retries_attribute() {
        let name = Value::Concrete(ConcreteValue::String("x".into()));
        let tags = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "environment".to_string(),
            Value::Concrete(ConcreteValue::String("production".into())),
        )])));
        let applied = Resource::with_provider("mock", "ec2.InternetGateway", "igw", None)
            .with_attribute("name", name.clone())
            .with_attribute("tags", tags.clone());
        let desired = Resource::with_provider("mock", "ec2.InternetGateway", "igw", None)
            .with_attribute("name", name.clone());
        let id = desired.id.clone();
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("ec2.InternetGateway")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                        .build(),
                ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let existing = state_after_accepted_create(
            &applied,
            HashMap::from([("name".to_string(), name), ("tags".to_string(), tags)]),
            &schemas,
        );
        let prior_explicit = existing
            .find_resource_for_id(&id)
            .expect("created IGW row")
            .explicit
            .clone();
        let failed_plan = plan_after_writeback(&desired, &existing, &schemas);
        assert!(
            failed_plan.effects().iter().any(|effect| matches!(
                effect,
                Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == "tags")
            )),
            "removing authored tags must initially plan an update: {failed_plan:#?}"
        );
        let managed_current = HashMap::from([(id.clone(), existing.build_state_for_resource(&id))]);

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &managed_current,
                applied_states: &HashMap::new(),
                plan: &failed_plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("failed update should write refreshed current state");

        assert_eq!(
            saved
                .find_resource_for_id(&id)
                .expect("saved IGW row")
                .explicit,
            prior_explicit,
            "an unapplied update must rebuild desired authoring and retain the live pending tags removal"
        );
        let retry_plan = plan_after_writeback(&desired, &saved, &schemas);
        assert!(
            retry_plan.effects().iter().any(|effect| matches!(
                effect,
                Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == "tags")
            )),
            "the next plan must retry the unapplied tags removal: {retry_plan:#?}"
        );
    }

    #[test]
    fn failed_update_nested_struct_pending_removal_survives_writeback() {
        let settings = |description: Option<&str>| {
            let mut fields = indexmap::IndexMap::from([(
                "enabled".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            )]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let applied_settings = settings(Some("web"));
        let applied = Resource::with_provider("mock", "test.Resource", "main", None)
            .with_attribute("settings", applied_settings.clone());
        let desired = Resource::with_provider("mock", "test.Resource", "main", None)
            .with_attribute("settings", settings(None));
        let id = desired.id.clone();
        let schemas = SchemaRegistry::new();
        let existing = state_after_accepted_create(
            &applied,
            HashMap::from([("settings".to_string(), applied_settings)]),
            &schemas,
        );
        let failed_plan = plan_after_writeback(&desired, &existing, &schemas);
        assert!(
            failed_plan.effects().iter().any(|effect| matches!(
                effect,
                Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == "settings")
            )),
            "an Applied row must initially plan the nested struct removal: {failed_plan:#?}"
        );
        let managed_current = HashMap::from([(id.clone(), existing.build_state_for_resource(&id))]);

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &managed_current,
                applied_states: &HashMap::new(),
                plan: &failed_plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("failed update should carry the refreshed current state");

        let row = saved
            .find_resource_for_id(&id)
            .expect("carried row must exist");
        let retry_plan = plan_after_writeback(&desired, &saved, &schemas);
        assert!(
            retry_plan.effects().iter().any(|effect| matches!(
                effect,
                Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == "settings")
            )),
            "the carried row must retain the pending nested struct removal; explicit={:#?}, plan={retry_plan:#?}",
            row.explicit,
        );
    }

    #[test]
    fn failed_update_list_element_nested_pending_removal_survives_writeback() {
        let rule = |port, description: Option<&str>| {
            let mut fields = indexmap::IndexMap::from([(
                "port".to_string(),
                Value::Concrete(ConcreteValue::Int(port)),
            )]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let applied_rules = Value::Concrete(ConcreteValue::List(vec![
            rule(80, Some("web")),
            rule(443, None),
        ]));
        let desired_rules =
            Value::Concrete(ConcreteValue::List(vec![rule(80, None), rule(443, None)]));
        let applied = Resource::with_provider("mock", "test.Resource", "main", None)
            .with_attribute("rules", applied_rules.clone());
        let desired = Resource::with_provider("mock", "test.Resource", "main", None)
            .with_attribute("rules", desired_rules);
        let id = desired.id.clone();
        let schemas = SchemaRegistry::new();
        let existing = state_after_accepted_create(
            &applied,
            HashMap::from([("rules".to_string(), applied_rules)]),
            &schemas,
        );
        let failed_plan = plan_after_writeback(&desired, &existing, &schemas);
        assert!(
            failed_plan.effects().iter().any(|effect| matches!(
                effect,
                Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == "rules")
            )),
            "an Applied row must initially plan the list-element removal: {failed_plan:#?}"
        );
        let managed_current = HashMap::from([(id.clone(), existing.build_state_for_resource(&id))]);

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &managed_current,
                applied_states: &HashMap::new(),
                plan: &failed_plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("failed update should carry the refreshed current state");

        let row = saved
            .find_resource_for_id(&id)
            .expect("carried row must exist");
        let retry_plan = plan_after_writeback(&desired, &saved, &schemas);
        assert!(
            retry_plan.effects().iter().any(|effect| matches!(
                effect,
                Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == "rules")
            )),
            "the carried row must retain the pending list-element removal; explicit={:#?}, plan={retry_plan:#?}",
            row.explicit,
        );
    }

    #[test]
    fn replacement_create_does_not_carry_removed_write_only_value() {
        let schemas = write_only_password_schema();
        let old = Resource::with_provider("mock", "test.resource", "replace", None)
            .with_attribute(
                "replacement_key",
                Value::Concrete(ConcreteValue::String("old-key".into())),
            )
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("old-password".into())),
            );
        let id = old.id.clone();
        let existing = state_after_accepted_create(
            &old,
            HashMap::from([(
                "replacement_key".to_string(),
                Value::Concrete(ConcreteValue::String("old-key".into())),
            )]),
            &schemas,
        );
        let desired = Resource::with_provider("mock", "test.resource", "replace", None)
            .with_attribute(
                "replacement_key",
                Value::Concrete(ConcreteValue::String("new-key".into())),
            );
        let plan = plan_after_writeback(&desired, &existing, &schemas);
        assert!(
            plan.effects()
                .iter()
                .any(|effect| matches!(effect, Effect::Create(resource) if resource.id == id)),
            "create-only change must plan a replacement create: {plan:#?}"
        );
        let managed_current = HashMap::from([(id.clone(), existing.build_state_for_resource(&id))]);
        let applied = State::existing(
            id.clone(),
            HashMap::from([(
                "replacement_key".to_string(),
                Value::Concrete(ConcreteValue::String("new-key".into())),
            )]),
        )
        .with_identifier("replacement-id");

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired)),
                runtime_synthesized_resources: &[],
                managed_current: &managed_current,
                applied_states: &HashMap::from([(id.clone(), complete_create(applied))]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("replacement create should write state");

        assert_stored_password(&saved, &id, None);
    }

    #[test]
    fn carry_previous_recovers_legacy_unmarked_write_only_value() {
        let desired = Resource::with_provider("mock", "test.resource", "legacy-echo", None)
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("new".into())),
            );
        let id = desired.id.clone();
        let legacy_row = ResourceState::try_applied(
            id.resource_type.clone(),
            id.identity().expect("fixture identity is resolved").clone(),
            id.provider.clone(),
            "provider-id",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("password", serde_json::json!("old"));
        assert!(legacy_row.write_only_attributes().is_empty());
        let mut existing = StateFile::new();
        existing
            .upsert_resource(legacy_row)
            .expect("legacy provider-echo row is valid");
        let refreshed = State::existing(id.clone(), HashMap::new()).with_identifier("provider-id");
        let schemas = write_only_password_schema();

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::from([(id.clone(), Current::from_state(refreshed))]),
                applied_states: &HashMap::new(),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("legacy provider-echo value should carry through refresh writeback");

        assert_stored_password(&saved, &id, Some("old"));
        assert_eq!(
            saved
                .find_resource_for_id(&id)
                .expect("saved row exists")
                .write_only_attributes(),
            ["password"]
        );
        let next_plan = plan_after_writeback(&desired, &saved, &schemas);
        assert!(next_plan.effects().iter().any(|effect| matches!(
            effect,
            Effect::Update {
                changed_attributes,
                ..
            } if changed_attributes.iter().any(|key| key == "password")
        )));
    }

    #[test]
    fn successful_update_without_write_only_key_carries_previous_value() {
        let desired = Resource::with_provider("mock", "test.resource", "partial-request", None)
            .with_attribute(
                "comment",
                Value::Concrete(ConcreteValue::String("changed".into())),
            )
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("new".into())),
            );
        let id = desired.id.clone();
        let existing = state_with_write_only_password(&id, "old");
        let applied = State::existing(
            id.clone(),
            HashMap::from([
                (
                    "comment".to_string(),
                    Value::Concrete(ConcreteValue::String("changed".into())),
                ),
                (
                    "password".to_string(),
                    Value::Concrete(ConcreteValue::String("provider-value".into())),
                ),
            ]),
        )
        .with_identifier("provider-id");
        let schemas = write_only_password_schema();

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired)),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    id.clone(),
                    complete_update(applied, &["comment"], &[]),
                )]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("successful update should write state");

        assert_stored_password(&saved, &id, Some("old"));
    }

    #[test]
    fn successful_update_with_write_only_key_records_desired_value() {
        let desired = Resource::with_provider("mock", "test.resource", "sent-password", None)
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("new".into())),
            );
        let id = desired.id.clone();
        let existing = state_with_write_only_password(&id, "old");
        let applied = State::existing(id.clone(), HashMap::new()).with_identifier("provider-id");
        let schemas = write_only_password_schema();

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired)),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    id.clone(),
                    complete_update(applied, &["password"], &[]),
                )]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("successful update should write state");

        assert_stored_password(&saved, &id, Some("new"));
    }

    #[test]
    fn carried_write_only_plaintext_is_hashed_when_desired_wraps_it_in_secret() {
        let desired = Resource::with_provider("mock", "test.resource", "secret-promotion", None)
            .with_attribute("password", secret_string("old"));
        let id = desired.id.clone();
        let existing = state_with_write_only_password(&id, "old");
        let schemas = write_only_password_schema();

        let next_plan = plan_after_writeback(&desired, &existing, &schemas);
        assert_eq!(next_plan.mutation_count(), 0);

        let refreshed = State::existing(id.clone(), HashMap::new()).with_identifier("provider-id");
        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired)),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::from([(id.clone(), Current::from_state(refreshed))]),
                applied_states: &HashMap::new(),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("secret promotion should protect the carried value during writeback");

        let stored = saved
            .find_resource_for_id(&id)
            .and_then(ResourceState::persisted_attributes)
            .and_then(|attributes| attributes.redacted_secret_hash("password"))
            .expect("carried write-only secret must be stored as a hash");
        assert!(stored.verify(serde_json::to_vec(&serde_json::json!("old")).unwrap()));
    }

    #[test]
    fn successful_update_that_removed_write_only_key_drops_previous_value() {
        let desired = Resource::with_provider("mock", "test.resource", "removed-password", None);
        let id = desired.id.clone();
        let existing = state_with_write_only_password(&id, "old");
        let applied = State::existing(
            id.clone(),
            HashMap::from([(
                "password".to_string(),
                Value::Concrete(ConcreteValue::String("stale-provider-value".into())),
            )]),
        )
        .with_identifier("provider-id");
        let schemas = write_only_password_schema();

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired)),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    id.clone(),
                    complete_update(applied, &[], &["password"]),
                )]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("successful removal should write state");

        assert_stored_password(&saved, &id, None);
    }

    #[test]
    fn removed_write_only_key_is_sent_dropped_and_converges() {
        let schemas = write_only_password_schema();
        let before = Resource::with_provider("mock", "test.resource", "remove-converges", None)
            .with_attribute(
                "comment",
                Value::Concrete(ConcreteValue::String("before".into())),
            )
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("old-password".into())),
            );
        let id = before.id.clone();
        let existing = state_after_accepted_create(
            &before,
            HashMap::from([(
                "comment".to_string(),
                Value::Concrete(ConcreteValue::String("before".into())),
            )]),
            &schemas,
        );
        let desired = Resource::with_provider("mock", "test.resource", "remove-converges", None)
            .with_attribute(
                "comment",
                Value::Concrete(ConcreteValue::String("after".into())),
            );
        let plan = plan_after_writeback(&desired, &existing, &schemas);
        let changed_attributes = plan
            .effects()
            .iter()
            .find_map(|effect| match effect {
                Effect::Update {
                    changed_attributes, ..
                } => Some(changed_attributes),
                _ => None,
            })
            .expect("removing a write-only key with another change should plan an update");
        assert!(changed_attributes.iter().any(|key| key == "comment"));
        assert!(
            changed_attributes.iter().any(|key| key == "password"),
            "the differ must include the removed write-only key: {changed_attributes:?}"
        );
        let managed_current = HashMap::from([(id.clone(), existing.build_state_for_resource(&id))]);
        let applied = State::existing(
            id.clone(),
            HashMap::from([(
                "comment".to_string(),
                Value::Concrete(ConcreteValue::String("after".into())),
            )]),
        )
        .with_identifier("provider-id");

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: existing,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &managed_current,
                applied_states: &HashMap::from([(
                    id.clone(),
                    complete_update(applied, &["comment"], &["password"]),
                )]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("accepted update removal should write state");

        assert_stored_password(&saved, &id, None);
        assert!(
            !saved
                .find_resource_for_id(&id)
                .expect("saved row exists")
                .write_only_attributes()
                .iter()
                .any(|key| key == "password")
        );
        let next_plan = plan_after_writeback(&desired, &saved, &schemas);
        assert_eq!(next_plan.mutation_count(), 0, "{next_plan:#?}");
    }

    #[test]
    fn import_records_no_write_only_value_and_next_plan_sends_updatable_key() {
        let desired = Resource::with_provider("mock", "test.resource", "imported", None)
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("import-baseline".into())),
            );
        let id = desired.id.clone();
        let imported = State::existing(id.clone(), HashMap::new()).with_identifier("provider-id");
        let schemas = write_only_password_schema();

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id.clone(), Applied::Imported(imported))]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("import should write state");

        assert_stored_password(&saved, &id, None);
        let ExplicitFields::Struct { children } = &saved
            .find_resource_for_id(&id)
            .expect("imported row exists")
            .explicit
        else {
            panic!("import must establish a desired authoring record");
        };
        assert!(
            children.contains_key("password"),
            "authoring provenance must be independent of CarryPrevious write-only provenance"
        );
        let next_plan = plan_after_writeback(&desired, &saved, &schemas);
        assert!(next_plan.effects().iter().any(|effect| matches!(
            effect,
            Effect::Update {
                changed_attributes,
                ..
            } if changed_attributes.iter().any(|key| key == "password")
        )));
    }

    #[test]
    fn import_create_only_write_only_key_is_skipped_by_1232_guard() {
        let desired =
            Resource::with_provider("mock", "test.resource", "imported-create-only", None)
                .with_attribute(
                    "password",
                    Value::Concrete(ConcreteValue::String("cannot-update".into())),
                );
        let id = desired.id.clone();
        let imported = State::existing(id.clone(), HashMap::new()).with_identifier("provider-id");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("test.resource").attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .create_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id.clone(), Applied::Imported(imported))]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("import should write state");

        assert_stored_password(&saved, &id, None);
        assert_eq!(
            plan_after_writeback(&desired, &saved, &schemas).mutation_count(),
            0
        );
    }

    fn write_observed_fixture(
        resource: &Resource,
        observed_attributes: HashMap<String, Value>,
        sent: carina_core::provider::Sent,
        schemas: &SchemaRegistry,
    ) -> StateFile {
        let id = resource.id.clone();
        let created = observed_create(&id, observed_attributes, sent);
        build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: std::slice::from_ref(&resolved(resource.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id, Applied::Observed(created.into()))]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas,
            },
            None,
        )
        .expect("observed create writeback")
    }

    fn assert_secret_matches(value: Value, plaintext: &str) {
        let Value::Deferred(DeferredValue::RedactedSecret { hash }) = value else {
            panic!("expected a redacted secret hash");
        };
        let material = serde_json::to_vec(&serde_json::json!(plaintext))
            .expect("fixture secret material serializes");
        assert!(
            hash.verify(material),
            "secret hash did not match {plaintext:?}"
        );
    }

    #[test]
    fn observed_attributes_are_recorded_and_secret_marked_values_are_masked() {
        let resource = Resource::with_provider("mock", "test.resource", "observed", None)
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("desired-name".into())),
            )
            .with_attribute("credential", secret_string("desired-secret"));
        let state = write_observed_fixture(
            &resource,
            HashMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("cloud-name".into())),
                ),
                (
                    "credential".to_string(),
                    Value::Concrete(ConcreteValue::String("cloud-secret".into())),
                ),
            ]),
            carina_core::provider::Sent::AllRequestAttributes,
            &SchemaRegistry::new(),
        );
        let attributes = state
            .find_resource_for_id(&resource.id)
            .and_then(ResourceState::persisted_attributes)
            .expect("observed row has applied attributes");

        assert_eq!(
            attributes.get("name"),
            Some(Value::Concrete(ConcreteValue::String("cloud-name".into())))
        );
        assert_secret_matches(
            attributes
                .get("credential")
                .expect("observed secret is recorded"),
            "cloud-secret",
        );
    }

    #[test]
    fn observation_records_only_sent_unobserved_write_only_and_secret_values() {
        let resource = Resource::with_provider("mock", "test.resource", "sent", None)
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("desired-name".into())),
            )
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("sent-password".into())),
            )
            .with_attribute("api_key", secret_string("sent-api-key"))
            .with_attribute("unsent_secret", secret_string("not-sent"));
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("test.resource")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                )
                .attribute(AttributeSchema::optional("api_key", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("unsent_secret", AttributeType::string()).build(),
                ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let state = write_observed_fixture(
            &resource,
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("cloud-name".into())),
            )]),
            carina_core::provider::Sent::only(["password", "api_key"]),
            &schemas,
        );
        let row = state
            .find_resource_for_id(&resource.id)
            .expect("observed row exists");
        let attributes = row
            .persisted_attributes()
            .expect("observed row has attributes");

        assert_eq!(
            attributes.get("password"),
            Some(Value::Concrete(ConcreteValue::String(
                "sent-password".into()
            )))
        );
        assert_secret_matches(
            attributes.get("api_key").expect("sent secret is recorded"),
            "sent-api-key",
        );
        assert!(!attributes.contains_key("unsent_secret"));
        assert_eq!(row.write_only_attributes(), ["password"]);
    }

    #[test]
    fn failed_follow_up_secret_is_absent_and_next_plan_sends_it() {
        let resource = Resource::with_provider("mock", "test.resource", "follow-up", None)
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("created".into())),
            )
            .with_attribute("tag_secret", secret_string("desired-tag"));
        let state = write_observed_fixture(
            &resource,
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("created".into())),
            )]),
            carina_core::provider::Sent::only(["name"]),
            &SchemaRegistry::new(),
        );
        let attributes = state
            .find_resource_for_id(&resource.id)
            .and_then(ResourceState::persisted_attributes)
            .expect("observed row has attributes");
        assert!(!attributes.contains_key("tag_secret"));

        let plan = plan_after_writeback(&resource, &state, &SchemaRegistry::new());
        assert!(plan.effects().iter().any(|effect| matches!(
            effect,
            Effect::Update {
                changed_attributes,
                ..
            } if changed_attributes.iter().any(|key| key == "tag_secret")
        )));
    }

    #[test]
    fn readable_secret_records_observed_hash_and_next_plan_sends_desired() {
        let resource = Resource::with_provider("mock", "test.resource", "readable-secret", None)
            .with_attribute("credential", secret_string("desired-secret"));
        let state = write_observed_fixture(
            &resource,
            HashMap::from([(
                "credential".to_string(),
                Value::Concrete(ConcreteValue::String("cloud-secret".into())),
            )]),
            carina_core::provider::Sent::AllRequestAttributes,
            &SchemaRegistry::new(),
        );
        let attributes = state
            .find_resource_for_id(&resource.id)
            .and_then(ResourceState::persisted_attributes)
            .expect("observed row has attributes");
        assert_secret_matches(
            attributes
                .get("credential")
                .expect("observed readable secret is recorded"),
            "cloud-secret",
        );

        let plan = plan_after_writeback(&resource, &state, &SchemaRegistry::new());
        assert!(plan.effects().iter().any(|effect| matches!(
            effect,
            Effect::Update {
                changed_attributes,
                ..
            } if changed_attributes.iter().any(|key| key == "credential")
        )));
    }

    #[test]
    fn sent_create_only_write_only_attribute_causes_no_replacement() {
        let resource = Resource::with_provider("mock", "test.resource", "write-only", None)
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("sent-password".into())),
            );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("test.resource").attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .create_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let state = write_observed_fixture(
            &resource,
            HashMap::new(),
            carina_core::provider::Sent::only(["password"]),
            &schemas,
        );
        let row = state
            .find_resource_for_id(&resource.id)
            .expect("observed row exists");
        assert_eq!(row.write_only_attributes(), ["password"]);

        let plan = plan_after_writeback(&resource, &state, &schemas);
        assert_eq!(
            plan.mutation_count(),
            0,
            "sent create-only value must converge"
        );
    }

    #[test]
    fn missing_applied_entry_keeps_current_in_flight_row_byte_for_byte() {
        let resource = Resource::with_provider("mock", "test.resource", "pending", None)
            .with_binding("pending")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("stable".into())),
            );
        let id = resource.id.clone();
        let resolved_resource = resolved(resource.clone());
        let pending = fresh_in_flight(
            &id,
            "first-token",
            Some("issued-id"),
            carina_core::provider::Sent::AllRequestAttributes,
        );
        let initial = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: std::slice::from_ref(&resolved_resource),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id.clone(), Applied::CreateInFlight(pending))]),
                plan: &plan_with_name_override(&id, "stable-cbd", "stable"),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("initial in-flight writeback");
        let before = serde_json::to_value(
            initial
                .find_resource_for_id(&id)
                .expect("initial in-flight row exists"),
        )
        .expect("row serializes");
        let managed_current = HashMap::from([(id.clone(), initial.build_state_for_resource(&id))]);

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: initial,
                sorted_resources: std::slice::from_ref(&resolved_resource),
                runtime_synthesized_resources: &[],
                managed_current: &managed_current,
                applied_states: &HashMap::new(),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("skipped resume writeback");
        let after = serde_json::to_value(
            saved
                .find_resource_for_id(&id)
                .expect("skipped resume keeps the row"),
        )
        .expect("row serializes");

        assert_eq!(after, before);
    }

    #[test]
    fn moved_in_flight_without_applied_entry_is_written_at_destination() {
        let source =
            ResourceId::with_provider_identity("mock", "test.resource", "old_address", None);
        let destination =
            ResourceId::with_provider_identity("mock", "test.resource", "new_address", None);
        let source_resource = resolved(
            Resource::from_id(source.clone())
                .with_binding("old_address")
                .with_attribute(
                    "name",
                    Value::Concrete(ConcreteValue::String("stable".into())),
                ),
        );
        let pending = fresh_in_flight(
            &source,
            "original-token",
            Some("issued-id"),
            carina_core::provider::Sent::AllRequestAttributes,
        );
        let initial = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: std::slice::from_ref(&source_resource),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    source.clone(),
                    Applied::CreateInFlight(pending),
                )]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("initial source writeback");
        let before = serde_json::to_value(
            initial
                .find_resource_for_id(&source)
                .expect("source continuation exists"),
        )
        .expect("source row serializes");
        let move_effect = Effect::Move {
            from: ResolvedResourceId::new(source.clone()),
            to: ResolvedResourceId::new(destination.clone()),
            origin: None,
        };
        let authority = move_effect
            .move_authority()
            .expect("the fixture effect is a move");
        let moved_current = carina_core::differ::rebind_current_for_move(
            initial.build_state_for_resource(&source),
            &authority,
        )
        .expect("fixture move source route matches");
        let Current::CreateInFlight(rebound) = moved_current else {
            panic!("source continuation must stay in flight while moving")
        };
        let destination_resource = resolved(
            Resource::from_id(destination.clone())
                .with_binding("new_address")
                .with_attribute(
                    "name",
                    Value::Concrete(ConcreteValue::String("stable".into())),
                ),
        );
        let mut plan = Plan::new();
        plan.add(move_effect);

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: initial,
                sorted_resources: std::slice::from_ref(&destination_resource),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::from([(
                    destination.clone(),
                    Current::CreateInFlight(rebound),
                )]),
                applied_states: &HashMap::new(),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("skipped moved resume writeback");

        assert!(
            saved.find_resource_for_id(&source).is_none(),
            "move cleanup must remove only the source address"
        );
        let after = serde_json::to_value(
            saved
                .find_resource_for_id(&destination)
                .expect("unchanged continuation must land at the destination"),
        )
        .expect("destination row serializes");
        let mut expected = before;
        expected["identity"] = serde_json::Value::String("new_address".to_string());
        assert_eq!(after, expected);
    }

    #[test]
    fn checkpoint_after_moved_update_contains_only_the_destination() {
        let source = ResourceId::with_provider_identity("mock", "test.resource", "a", None);
        let destination = ResourceId::with_provider_identity("mock", "test.resource", "b", None);
        let destination_resource = resolved(Resource::from_id(destination.clone()).with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("after".into())),
        ));
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("test.resource")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut initial = StateFile::new();
        initial
            .upsert_resource(
                ResourceState::try_applied("test.resource", "a", "mock", "stable-id")
                    .expect("fixture identifier is non-empty")
                    .with_attribute("name", serde_json::json!("before")),
            )
            .expect("fixture row is valid");
        let applied = State::existing(
            destination.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("after".into())),
            )]),
        )
        .with_identifier("stable-id");
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            identifier: crate::fixture_applied_identifier("stable-id"),
            from: Box::new(
                State::existing(source.clone(), HashMap::new()).with_identifier("stable-id"),
            ),
            to: destination_resource.clone(),
            changed_attributes: vec!["name".to_string()],
        });
        plan.add(Effect::Move {
            from: ResolvedResourceId::new(source.clone()),
            to: ResolvedResourceId::new(destination.clone()),
            origin: None,
        });

        // Crash-shaped checkpoint: the provider Update on `b` has completed,
        // but the old post-executor state-only phase has not run yet.
        let checkpoint = build_state_after_apply(
            ApplyStateSave {
                state_file: initial,
                sorted_resources: std::slice::from_ref(&destination_resource),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    destination.clone(),
                    complete_update(applied, &["name"], &[]),
                )]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("checkpoint writeback should succeed");

        assert!(
            checkpoint.find_resource_for_id(&source).is_none(),
            "a durable provider result must not leave the moved-from row behind"
        );
        assert_eq!(
            checkpoint
                .find_resource_for_id(&destination)
                .and_then(|row| row.identifier())
                .map(|identifier| identifier.as_str()),
            Some("stable-id")
        );
    }

    #[test]
    fn repend_keeps_envelope_values_started_at_and_name_overrides() {
        let resource = Resource::with_provider("mock", "test.resource", "pending", None)
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("stable".into())),
            )
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("first-password".into())),
            )
            .with_attribute("api_key", secret_string("first-api-key"));
        let id = resource.id.clone();
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("test.resource")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                )
                .attribute(AttributeSchema::optional("api_key", AttributeType::string()).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let resolved_resource = resolved(resource.clone());
        let pending = fresh_in_flight(
            &id,
            "first-token",
            Some("issued-id"),
            carina_core::provider::Sent::only(["password", "api_key"]),
        );
        let initial = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: std::slice::from_ref(&resolved_resource),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id.clone(), Applied::CreateInFlight(pending))]),
                plan: &plan_with_name_override(&id, "stable-cbd", "stable"),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("initial in-flight writeback");
        let initial_row = initial
            .find_resource_for_id(&id)
            .expect("initial in-flight row");
        let CurrentInstance::CreateInFlight {
            pending: initial_pending,
        } = initial_row.current()
        else {
            panic!("expected in-flight current slot");
        };
        let SentRecord::Persisted(initial_envelope) = initial_pending.sent() else {
            panic!("written continuation must carry persisted values");
        };
        let initial_write_only = initial_envelope.write_only().clone();
        let initial_secrets = initial_envelope.secrets().clone();
        let initial_started_at = initial_pending.started_at().clone();
        let initial_name_overrides = initial_row.name_overrides.clone();

        let Current::CreateInFlight(loaded) = initial.build_state_for_resource(&id) else {
            panic!("written pending row must load as in flight");
        };
        let repended = crate::fixture_repend_in_flight_create(loaded, "second-token");
        let changed_resource = resource
            .clone()
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String("changed-password".into())),
            )
            .with_attribute("api_key", secret_string("changed-api-key"));
        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: initial,
                sorted_resources: std::slice::from_ref(&resolved(changed_resource)),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id.clone(), Applied::CreateInFlight(repended))]),
                plan: &plan_with_name_override(&id, "changed-cbd", "changed"),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("re-pend writeback");

        let row = saved.find_resource_for_id(&id).expect("re-pended row");
        let CurrentInstance::CreateInFlight { pending } = row.current() else {
            panic!("expected re-pended current slot");
        };
        assert_eq!(pending.token().expose_for_provider_call(), "second-token");
        let SentRecord::Persisted(envelope) = pending.sent() else {
            panic!("re-pended continuation must carry persisted values");
        };
        assert_eq!(envelope.write_only(), &initial_write_only);
        assert_eq!(envelope.secrets(), &initial_secrets);
        assert_eq!(pending.started_at(), &initial_started_at);
        assert_eq!(row.name_overrides, initial_name_overrides);
    }

    #[test]
    fn persisted_repend_seeds_unrecorded_explicit_from_desired() {
        let resource = Resource::with_provider("mock", "test.resource", "pending", None)
            .with_attribute("name", Value::Concrete(ConcreteValue::String("x".into())));
        let resolved_resource = resolved(resource.clone());
        let fresh = fresh_in_flight(
            &resource.id,
            "first-token",
            None,
            carina_core::provider::Sent::AllRequestAttributes,
        );
        let mut existing =
            ResourceState::from_in_flight(&resolved_resource, &fresh, None, None, None)
                .expect("fresh continuation must encode");
        existing.explicit = ExplicitFields::Unrecorded;

        let mut state = StateFile::new();
        state
            .upsert_resource(existing.clone())
            .expect("in-flight fixture must be valid");
        let Current::CreateInFlight(loaded) = state.build_state_for_resource(&resource.id) else {
            panic!("persisted fixture must load as in flight");
        };
        let repended = crate::fixture_repend_in_flight_create(loaded, "second-token");

        let seeded_without_prior =
            ResourceState::from_in_flight(&resolved_resource, &repended, None, None, None)
                .expect("persisted re-pend without a prior row must encode");
        assert_eq!(
            seeded_without_prior.explicit,
            ExplicitFields::Struct {
                children: HashMap::from([("name".to_string(), ExplicitFields::Leaf)])
            }
        );

        let repaired = ResourceState::from_in_flight(
            &resolved_resource,
            &repended,
            Some(&existing),
            None,
            None,
        )
        .expect("persisted re-pend must encode");
        assert_eq!(
            repaired.explicit,
            ExplicitFields::Struct {
                children: HashMap::from([("name".to_string(), ExplicitFields::Leaf)])
            }
        );
    }

    #[test]
    fn moved_repend_persists_the_continuation_route_as_the_row_route() {
        let source = ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "old_address",
            Some("east".to_string()),
        );
        let destination = ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "new_address",
            Some("west".to_string()),
        );
        let source_resource = resolved(Resource::from_id(source.clone()));
        let destination_resource = resolved(Resource::from_id(destination.clone()));

        let source_pending = fresh_in_flight(
            &source,
            "source-token",
            None,
            carina_core::provider::Sent::AllRequestAttributes,
        );
        let source_row =
            ResourceState::from_in_flight(&source_resource, &source_pending, None, None, None)
                .expect("source continuation must encode");
        let mut initial = StateFile::new();
        initial
            .upsert_resource(source_row)
            .expect("source row must be valid");

        // Load a separately persisted continuation on west so this fixture
        // exercises the writeback constructor's source of routing directly.
        let destination_pending = fresh_in_flight(
            &destination,
            "destination-token",
            None,
            carina_core::provider::Sent::AllRequestAttributes,
        );
        let destination_row = ResourceState::from_in_flight(
            &destination_resource,
            &destination_pending,
            None,
            None,
            None,
        )
        .expect("destination continuation must encode");
        let mut destination_state = StateFile::new();
        destination_state
            .upsert_resource(destination_row)
            .expect("destination row must be valid");
        let Current::CreateInFlight(loaded_destination) =
            destination_state.build_state_for_resource(&destination)
        else {
            panic!("destination continuation must load as in flight");
        };
        let repended = crate::fixture_repend_in_flight_create(
            loaded_destination,
            "destination-repended-token",
        );

        let mut plan = Plan::new();
        plan.add(Effect::Move {
            from: ResolvedResourceId::new(source.clone()),
            to: ResolvedResourceId::new(destination.clone()),
            origin: None,
        });
        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: initial,
                sorted_resources: std::slice::from_ref(&destination_resource),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    destination.clone(),
                    Applied::CreateInFlight(repended),
                )]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("moved re-pend writeback");

        let row = saved
            .find_resource_for_id(&destination)
            .expect("the destination route must own the persisted continuation");
        assert_eq!(row.directives.provider_instance.as_deref(), Some("west"));
    }

    #[test]
    fn only_create_ended_clears_current_and_keeps_deposed() {
        let resource = Resource::with_provider("mock", "test.resource", "pending", None)
            .with_binding("pending")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("stable".into())),
            );
        let id = resource.id.clone();
        let resolved_resource = resolved(resource.clone());
        let pending = fresh_in_flight(
            &id,
            "first-token",
            Some("issued-id"),
            carina_core::provider::Sent::AllRequestAttributes,
        );
        let mut initial = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: std::slice::from_ref(&resolved_resource),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id.clone(), Applied::CreateInFlight(pending))]),
                plan: &plan_with_name_override(&id, "stable-cbd", "stable"),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("initial in-flight writeback");
        initial
            .upsert_deposed_generation(
                &id.provider,
                &id.resource_type,
                resolved_resource.identity(),
                id.provider_instance.clone(),
                DeposedInstance::from_persisted_json(
                    DeposedKey::new_unique(),
                    "old-id".to_string(),
                    None,
                    HashMap::from([("name".to_string(), serde_json::json!("old"))]),
                    BTreeSet::from(["dependency".to_string()]),
                ),
            )
            .expect("deposed fixture is valid");
        let before = initial
            .find_resource_for_id(&id)
            .expect("row with deposed generation")
            .clone();

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: initial,
                sorted_resources: std::slice::from_ref(&resolved_resource),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id.clone(), Applied::CreateEnded)]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("ended create writeback");

        let row = saved
            .find_resource_for_id(&id)
            .expect("deposed generation keeps the address row");
        assert!(matches!(row.current(), CurrentInstance::None));
        assert_eq!(
            serde_json::to_value(&row.deposed).unwrap(),
            serde_json::to_value(&before.deposed).unwrap()
        );
        assert_eq!(row.binding, before.binding);
        assert_eq!(row.dependency_bindings, before.dependency_bindings);
        assert_eq!(row.directives, before.directives);
        assert_eq!(row.prefixes, before.prefixes);
        assert_eq!(row.name_overrides, before.name_overrides);
        assert_eq!(row.explicit, before.explicit);
        assert_eq!(row.protected, before.protected);
    }

    #[test]
    fn apply_writeback_stores_list_and_aligned_authoring_atomically_across_round_trip() {
        let rule = |port, description: Option<&str>| {
            let mut fields = indexmap::IndexMap::from([(
                "port".to_string(),
                Value::Concrete(ConcreteValue::Int(port)),
            )]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let resource = Resource::with_provider("mock", "listener.Listener", "listener", None)
            .with_attribute(
                "rules",
                Value::Concrete(ConcreteValue::List(vec![
                    rule(80, Some("web")),
                    rule(443, None),
                ])),
            );
        let id = resource.id.clone();
        let sibling = Resource::with_provider("mock", "test.Resource", "sibling", None)
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("before".into())),
            );
        let sibling_id = sibling.id.clone();
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    rule(443, Some("provider-default")),
                    rule(80, Some("web")),
                ])),
            )]),
        )
        .with_identifier("listener-id");
        let sibling_state = State::existing(
            sibling_id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("before".into())),
            )]),
        )
        .with_identifier("sibling-id");
        let resources = vec![resolved(resource.clone()), resolved(sibling.clone())];
        let applied_states = HashMap::from([
            (id.clone(), complete_create(applied_state)),
            (sibling_id.clone(), complete_create(sibling_state)),
        ]);
        let state = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: &resources,
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &applied_states,
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("apply writeback");

        let assert_aligned = |state: &StateFile| {
            let row = state
                .find_resource("mock", "listener.Listener", "listener")
                .expect("written listener row");
            let stored_json = PersistedValue::encode_provider_value(
                &row.persisted_attributes()
                    .expect("listener has applied attributes")
                    .get("rules")
                    .expect("stored rules"),
                None,
            )
            .expect("stored rules are JSON");
            let stored = stored_json.as_json().as_array().expect("stored rules");
            assert_eq!(stored.len(), 2);
            assert_eq!(stored[0]["port"], 443);
            assert_eq!(stored[1]["port"], 80);
            let ExplicitFields::Struct { children } = &row.explicit else {
                panic!("expected root Struct");
            };
            let ExplicitFields::ListElements { elements } = &children["rules"] else {
                panic!("expected aligned ListElements");
            };
            assert_eq!(elements.len(), stored.len());
            let ExplicitFields::Struct { children: first } = &elements[0] else {
                panic!("expected first Struct authoring");
            };
            assert_eq!(first.len(), 1);
            assert!(first.contains_key("port"));
            let ExplicitFields::Struct { children: second } = &elements[1] else {
                panic!("expected second Struct authoring");
            };
            assert_eq!(second.len(), 2);
            assert!(second.contains_key("port"));
            assert!(second.contains_key("description"));
        };

        assert_aligned(&state);
        let serialized = serde_json::to_string(&state).expect("serialize state");
        let round_tripped = carina_state::check_and_migrate(&serialized)
            .expect("deserialize state")
            .into_state();
        assert_aligned(&round_tripped);

        let listener_current = round_tripped.build_state_for_resource(&id);
        let sibling = Resource::with_provider("mock", "test.Resource", "sibling", None)
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("after".into())),
            );
        let sibling_state = State::existing(
            sibling_id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("after".into())),
            )]),
        )
        .with_identifier("sibling-id");
        let resources = vec![resolved(resource.clone()), resolved(sibling)];
        let managed_current = HashMap::from([(id.clone(), listener_current)]);
        let applied_states =
            HashMap::from([(sibling_id, complete_update(sibling_state, &["name"], &[]))]);
        let carried = build_state_after_apply(
            ApplyStateSave {
                state_file: round_tripped,
                sorted_resources: &resources,
                runtime_synthesized_resources: &[],
                managed_current: &managed_current,
                applied_states: &applied_states,
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("unrelated apply writeback");

        assert_aligned(&carried);
        let removed_description =
            Resource::with_provider("mock", "listener.Listener", "listener", None).with_attribute(
                "rules",
                Value::Concrete(ConcreteValue::List(vec![rule(80, None), rule(443, None)])),
            );
        let retry_plan =
            plan_after_writeback(&removed_description, &carried, &SchemaRegistry::new());
        assert!(
            retry_plan.effects().iter().any(|effect| matches!(
                effect,
                Effect::Update {
                    changed_attributes,
                    ..
                } if changed_attributes.iter().any(|key| key == "rules")
            )),
            "removing the authored nested field must still plan an update after unrelated writeback: {retry_plan:#?}"
        );
    }

    #[test]
    fn writeback_persists_typed_name_override_from_plan() {
        let mut resource = Resource::with_provider("mock", "test.resource", "thing", None);
        resource.set_attr(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("dsl-new".into())),
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
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let applied_states = HashMap::from([(
            id.clone(),
            complete_create(applied_name_state(&id, "new-temp")),
        )]);
        let plan = plan_with_name_override(&id, "new-temp", "dsl-new");

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: &ResolvedResource::collect_resolved(sorted_resources.clone())
                    .unwrap(),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &applied_states,
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &carina_core::schema::SchemaRegistry::new(),
            },
            None,
        )
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
            Value::Concrete(ConcreteValue::String("dsl-value".into())),
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
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let applied_states = HashMap::from([(
            id.clone(),
            complete_create(applied_name_state(&id, "legacy-temp")),
        )]);
        let plan = plan_with_name_override(&id, "legacy-temp", "dsl-value");

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: &ResolvedResource::collect_resolved(sorted_resources.clone())
                    .unwrap(),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &applied_states,
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &carina_core::schema::SchemaRegistry::new(),
            },
            None,
        )
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
                Value::Concrete(ConcreteValue::String("_dns_record_name_".into())),
            );
        let child_id = runtime_child.id.clone();
        let child_state = State::existing(
            child_id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("_dns_record_name_".into())),
            )]),
        )
        .with_identifier("record-id");

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(child_id.clone(), complete_create(child_state))]),
                runtime_synthesized_resources: std::slice::from_ref(&resolved(runtime_child)),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &carina_core::schema::SchemaRegistry::new(),
            },
            None,
        )
        .expect("state writeback should accept runtime-synthesized child");

        assert!(
            state
                .resources()
                .iter()
                .any(|row| row.provider == child_id.provider
                    && row.resource_type == child_id.resource_type
                    && row.identity.as_str() == "validation_records[0]"),
            "runtime-synthesized child must be persisted in state"
        );
    }

    #[test]
    fn successfully_deleted_uses_full_resource_id_not_identity_only() {
        let id_a = ResourceId::with_provider_identity("mock", "type.A", "shared", None);
        let id_b = ResourceId::with_provider_identity("mock", "type.B", "shared", None);
        let row_a = ResourceState::try_applied("type.A", "shared", "mock", "a-old")
            .expect("fixture identifier is non-empty");
        let row_b = ResourceState::try_applied("type.B", "shared", "mock", "b-old")
            .expect("fixture identifier is non-empty");
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row_a)
            .expect("test state setup must be valid");
        state_file
            .upsert_resource(row_b)
            .expect("test state setup must be valid");

        let mut plan = Plan::new();
        for (id, identifier) in [(&id_a, "a-old"), (&id_b, "b-old")] {
            plan.add(Effect::Delete {
                id: carina_core::resource::ResolvedResourceId::new(id.clone()),
                identifier: crate::fixture_applied_identifier(identifier),
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

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: &[],
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::new(),
                plan: &plan,
                successfully_deleted: &successfully_deleted,
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
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
        let mut row = ResourceState::try_applied(
            "route53.Record",
            "validation_records[0]",
            "aws",
            "new-record-id",
        )
        .expect("fixture identifier is non-empty");
        row.deposed.push(DeposedInstance::from_persisted_json(
            deposed_key.clone(),
            "old-record-id".to_string(),
            None,
            HashMap::from([("name".to_string(), serde_json::json!("old.example.com"))]),
            BTreeSet::from(["cert".to_string()]),
        ));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row)
            .expect("test state setup must be valid");

        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: crate::fixture_applied_identifier("old-record-id"),
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
            template: Box::new(DeferredForExpression {
                file: Some("main.crn".to_string()),
                line: 1,
                header: "for opt in cert.domain_validation_options".to_string(),
                resource_type: "aws.route53.Record".to_string(),
                attributes: vec![],
                target: carina_core::parser::DeferredForTarget::Bound {
                    binding: "validation_records".to_string(),
                },
                iterable: Value::resource_ref("cert", "domain_validation_options", Vec::new()),
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

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: &[],
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::new(),
                plan: &plan,
                successfully_deleted: &successfully_deleted,
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should remove successful top-level deposed delete");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("new-record-id")
        );
        assert!(
            row.deposed.is_empty(),
            "successful top-level deposed delete must remove its stored generation"
        );
    }

    fn deferred_replace_delete(id: &ResourceId) -> DeferredReplaceDelete {
        DeferredReplaceDelete {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: crate::fixture_applied_identifier("old-record-id"),
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
            template: Box::new(DeferredForExpression {
                file: Some("main.crn".to_string()),
                line: 1,
                header: "for opt in cert.domain_validation_options".to_string(),
                resource_type: "aws.route53.Record".to_string(),
                attributes: vec![],
                target: carina_core::parser::DeferredForTarget::Bound {
                    binding: "validation_records".to_string(),
                },
                iterable: Value::resource_ref("cert", "domain_validation_options", Vec::new()),
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
        let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(id.clone(), "old-record-id")]);
        let failed_refreshes = HashSet::new();
        let schemas = SchemaRegistry::new();

        let runtime_child = resolved(runtime_child);
        let wb = decompose(DecomposeInput {
            state_file: &StateFile::new(),
            sorted_resources: &[],
            runtime_synthesized_resources: std::slice::from_ref(&runtime_child),
            managed_current: &current_states,
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
                Value::Concrete(ConcreteValue::String("new.example.com".into())),
            )]),
        )
        .with_identifier("new-record-id");
        let current_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("old.example.com".into())),
            )]),
        )
        .with_identifier("old-record-id");
        let mut existing = ResourceState::try_applied(
            "route53.Record",
            "validation_records[0]",
            "aws",
            "old-record-id",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("name", serde_json::json!("old.example.com"));
        existing.directives.provider_instance = Some("west".to_string());
        existing.dependency_bindings.insert("cert".to_string());
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: &[],
                runtime_synthesized_resources: std::slice::from_ref(&resolved(runtime_child)),
                managed_current: &crate::managed_current_from_states(current_states.clone()),
                applied_states: &applied_states,
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &carina_core::schema::SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should preserve old absorbed delete as deposed");

        let saved = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("new child row should be persisted");
        assert_eq!(
            saved.identifier().map(|value| value.as_str()),
            Some("new-record-id")
        );
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
        let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: &[],
                runtime_synthesized_resources: std::slice::from_ref(&resolved(runtime_child)),
                managed_current: &crate::managed_current_from_states(current_states.clone()),
                applied_states: &applied_states,
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &carina_core::schema::SchemaRegistry::new(),
            },
            None,
        )
        .expect("same-identifier replacement should write only the current row");

        let saved = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("current row should be persisted");
        assert_eq!(
            saved.identifier().map(|value| value.as_str()),
            Some("old-record-id")
        );
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
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            );
        let id = desired.id.clone();
        let existing = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-canonical-old")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let from_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )]),
        )
        .with_identifier("vpc-canonical-old");
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            )]),
        )
        .with_identifier("vpc-canonical-new");
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            identifier: crate::fixture_applied_identifier("vpc-canonical-old"),
            from: Box::new(from_state),
            to: carina_core::resource::ResolvedResource::new(desired.clone()),
            changed_attributes: vec!["cidr_block".to_string()],
        });

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    id,
                    complete_update(applied_state, &["cidr_block"], &[]),
                )]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("update writeback should not depose canonicalized identifiers");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("vpc-canonical-new")
        );
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
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            );
        let id = desired.id.clone();
        let mut existing = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-old")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        existing.dependency_bindings.insert("igw".to_string());
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            )]),
        )
        .with_identifier("vpc-new");
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id, complete_create(applied_state))]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should depose the displaced current identifier");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("vpc-new")
        );
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].identifier, "vpc-old");
        assert_eq!(
            row.deposed[0].persisted_attributes().get("cidr_block"),
            carina_core::value::json_to_dsl_value(&serde_json::json!("10.0.0.0/16"))
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
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            );
        let id = desired.id.clone();
        let existing = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-old")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let current_state = State::not_found(id.clone()).with_identifier("vpc-old");
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            )]),
        )
        .with_identifier("vpc-new");
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::from([(id.clone(), Current::from_state(current_state))]),
                applied_states: &HashMap::from([(id, complete_create(applied_state))]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should treat the old identifier as already absent");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("vpc-new")
        );
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
                    Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
                );
        let id = desired.id.clone();
        let mut existing = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-old")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        existing.directives.provider_instance = Some("old-provider".to_string());
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            )]),
        )
        .with_identifier("vpc-new");
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(desired.clone()),
        ));

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id, complete_create(applied_state))]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should depose the displaced current identifier");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("vpc-new")
        );
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
                    Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
                );
        let create_id = desired.id.clone();
        let delete_id = ResourceId::with_provider_identity(
            "awscc",
            "ec2.Vpc",
            "main",
            Some("old-provider".to_string()),
        );
        let mut existing = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-old")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        existing.directives.provider_instance = Some("old-provider".to_string());
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let applied_state = State::existing(
            create_id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            )]),
        )
        .with_identifier("vpc-new");
        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(delete_id.clone()),
            identifier: crate::fixture_applied_identifier("vpc-old"),
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
        let previous_attributes: HashMap<String, Value> = HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
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
        let plan = serde_json::from_value::<carina_core::plan::SavedPlan>(plan_json)
            .expect("replacement metadata should deserialize");
        let saved_desired =
            carina_core::resource::SavedResolvedResource::from(resolved(desired.clone()));
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(delete_id, "vpc-old")]);

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&saved_desired),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(create_id, complete_create(applied_state))]),
                plan: &plan,
                successfully_deleted: &successfully_deleted,
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should treat the old routed delete as successful");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("vpc-new")
        );
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
                    Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
                );
        let create_id = desired.id.clone();
        let mut existing = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "shared-name")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        existing.directives.provider_instance = Some("old-provider".to_string());
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let applied_state = State::existing(
            create_id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
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

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(create_id, complete_create(applied_state))]),
                plan: &plan,
                successfully_deleted: &successfully_deleted,
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should not confuse provider-instance-local identifiers");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("vpc-new")
        );
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
            Value::Concrete(ConcreteValue::String("new-2.example.com".into())),
        );
        let id = desired.id.clone();
        let mut existing = ResourceState::try_applied(
            "route53.Record",
            "validation_records[0]",
            "aws",
            "new-1-record-id",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("name", serde_json::json!("new-1.example.com"));
        existing.directives.provider_instance = Some("west".to_string());
        existing.deposed.push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            "old-record-id".to_string(),
            Some("west".to_string()),
            HashMap::from([("name".to_string(), serde_json::json!("old.example.com"))]),
            BTreeSet::new(),
        ));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");

        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("new-2.example.com".into())),
            )]),
        )
        .with_identifier("new-2-record-id");
        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: crate::fixture_applied_identifier("old-record-id"),
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
        let previous_attributes: HashMap<String, Value> = HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("old.example.com".into())),
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
        let plan = serde_json::from_value::<carina_core::plan::SavedPlan>(plan_json)
            .expect("replacement metadata should deserialize");
        let saved_desired =
            carina_core::resource::SavedResolvedResource::from(resolved(desired.clone()));

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&saved_desired),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id, complete_create(applied_state))]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should preserve both failed old delete and displaced current");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("new-2-record-id")
        );
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
            Value::Concrete(ConcreteValue::String("new-2.example.com".into())),
        );
        let id = desired.id.clone();
        let mut existing = ResourceState::try_applied(
            "route53.Record",
            "validation_records[0]",
            "aws",
            "new-1-record-id",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("name", serde_json::json!("new-1.example.com"));
        existing.directives.provider_instance = Some("west".to_string());
        existing.deposed.push(DeposedInstance::from_persisted_json(
            DeposedKey::new_unique(),
            "old-record-id".to_string(),
            Some("west".to_string()),
            HashMap::from([("name".to_string(), serde_json::json!("old.example.com"))]),
            BTreeSet::new(),
        ));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");

        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("new-2.example.com".into())),
            )]),
        )
        .with_identifier("new-2-record-id");
        let mut plan = Plan::new();
        plan.add(Effect::Delete {
            id: carina_core::resource::ResolvedResourceId::new(id.clone()),
            identifier: crate::fixture_applied_identifier("old-record-id"),
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
        let previous_attributes: HashMap<String, Value> = HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("old.example.com".into())),
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
        let plan = serde_json::from_value::<carina_core::plan::SavedPlan>(plan_json)
            .expect("replacement metadata should deserialize");
        let saved_desired =
            carina_core::resource::SavedResolvedResource::from(resolved(desired.clone()));
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(id.clone(), "old-record-id")]);

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&saved_desired),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id, complete_create(applied_state))]),
                plan: &plan,
                successfully_deleted: &successfully_deleted,
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should depose displaced current and retain existing deposed entry");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("new-2-record-id")
        );
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
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            );
        let id = desired.id.clone();
        let existing = ResourceState::try_applied("ec2.Vpc", "main", "awscc", "vpc-old")
            .expect("fixture identifier is non-empty")
            .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let refreshed_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            )]),
        )
        .with_identifier("vpc-new");

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::from([(id, Current::from_state(refreshed_state))]),
                applied_states: &HashMap::new(),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("refresh writeback should update the current row only");

        let row = state
            .find_resource("awscc", "ec2.Vpc", "main")
            .expect("current row should remain");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("vpc-new")
        );
        assert!(
            row.deposed.is_empty(),
            "refresh-only current-state upserts must not depose the previous identifier"
        );
    }

    #[test]
    fn apply_writeback_secret_to_plain_demotion_converges() {
        let desired = Resource::with_provider("awscc", "db.Instance", "main", None).with_attribute(
            "password",
            Value::Concrete(ConcreteValue::String("plain-secret".into())),
        );
        let id = desired.id.clone();
        let existing = ResourceState::try_applied("db.Instance", "main", "awscc", "db-1")
            .expect("fixture identifier is non-empty")
            .with_attribute("password", structural_legacy_secret(&"ab".repeat(32)));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let from_state = State::existing(
            id.clone(),
            HashMap::from([(
                "password".to_string(),
                Value::Concrete(ConcreteValue::String("old-secret".into())),
            )]),
        )
        .with_identifier("db-1");
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "password".to_string(),
                Value::Concrete(ConcreteValue::String("plain-secret".into())),
            )]),
        )
        .with_identifier("db-1");
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            identifier: crate::fixture_applied_identifier("db-1"),
            from: Box::new(from_state),
            to: carina_core::resource::ResolvedResource::new(desired.clone()),
            changed_attributes: vec!["password".to_string()],
        });

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    id.clone(),
                    complete_update(applied_state, &["password"], &[]),
                )]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("secret-to-plain demotion should write state");

        let row = saved
            .find_resource("awscc", "db.Instance", "main")
            .expect("row should be saved");
        assert_eq!(
            row.persisted_attributes()
                .expect("row has applied attributes")
                .get("password"),
            carina_core::value::json_to_dsl_value(&serde_json::json!("plain-secret"))
        );

        let loaded = saved.build_state_for_resource(&id);
        let current_state = match loaded {
            carina_core::resource::Current::Absent(state) => state.into_state(),
            carina_core::resource::Current::Present(present) => present.into_parts().0,
            carina_core::resource::Current::MissingIdentifier(state) => state.into_state(),
            carina_core::resource::Current::CreateInFlight(pending) => {
                panic!("saved applied row unexpectedly remained in flight: {pending:?}")
            }
        };
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let second_plan = create_plan(
            std::slice::from_ref(&resolved(desired.clone())),
            &[],
            &ProviderRouter::new(),
            carina_core::differ::PlanPriorStates::new(
                &crate::plan_input_current_from_states(
                    current_states.clone(),
                    &SchemaRegistry::new(),
                    &[],
                ),
                &carina_core::differ::Orphans::new(),
            ),
            &HashMap::new(),
            &SchemaRegistry::new(),
            &crate::empty_lifted_saved_attrs(),
            &HashMap::new(),
            &[],
        )
        .expect("test provider hints should succeed");

        assert_eq!(
            second_plan.mutation_count(),
            0,
            "second plan after secret-to-plain demotion must be empty"
        );
    }

    #[test]
    fn apply_writeback_keeps_plain_raw_legacy_marker_in_write_only_attribute() {
        let marker = format!("{LEGACY_SECRET_PREFIX}{}", "ab".repeat(32));
        let desired = Resource::with_provider("mock", "test.resource", "plain-marker", None)
            .with_attribute(
                "password",
                Value::Concrete(ConcreteValue::String(marker.clone().into())),
            );
        let id = desired.id.clone();
        let applied_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("provider-id");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("test.resource").attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: std::slice::from_ref(&resolved(desired)),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(id.clone(), complete_create(applied_state))]),
                plan: &Plan::new(),
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("a marker-shaped plain string must remain an ordinary string");

        let row = saved
            .find_resource("mock", "test.resource", "plain-marker")
            .expect("applied row must be saved");
        assert_eq!(
            row.persisted_attributes()
                .expect("row has applied attributes")
                .get("password"),
            carina_core::value::json_to_dsl_value(&serde_json::json!(marker)),
        );
    }

    #[test]
    fn apply_writeback_write_only_secret_to_plain_demotion_converges() {
        let desired = Resource::with_provider("awscc", "db.Instance", "main", None).with_attribute(
            "password",
            Value::Concrete(ConcreteValue::String("plain-secret".into())),
        );
        let id = desired.id.clone();
        let existing = ResourceState::try_applied("db.Instance", "main", "awscc", "db-1")
            .expect("fixture identifier is non-empty")
            .with_attribute("password", structural_legacy_secret(&"ab".repeat(32)));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let from_state = State::existing(id.clone(), HashMap::new()).with_identifier("db-1");
        let applied_state = State::existing(id.clone(), HashMap::new()).with_identifier("db-1");
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            identifier: crate::fixture_applied_identifier("db-1"),
            from: Box::new(from_state),
            to: carina_core::resource::ResolvedResource::new(desired.clone()),
            changed_attributes: vec!["password".to_string()],
        });
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            (ResourceSchema::new("db.Instance").attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let saved = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: std::slice::from_ref(&resolved(desired.clone())),
                runtime_synthesized_resources: &[],
                managed_current: &HashMap::new(),
                applied_states: &HashMap::from([(
                    id.clone(),
                    complete_update(applied_state, &["password"], &[]),
                )]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("write-only secret-to-plain demotion should write state");

        let row = saved
            .find_resource("awscc", "db.Instance", "main")
            .expect("row should be saved");
        assert_eq!(
            row.persisted_attributes()
                .expect("row has applied attributes")
                .get("password"),
            carina_core::value::json_to_dsl_value(&serde_json::json!("plain-secret"))
        );
        assert_eq!(row.write_only_attributes(), ["password"]);

        let loaded = saved.build_state_for_resource(&id);
        let current_state = match loaded {
            carina_core::resource::Current::Absent(state) => state.into_state(),
            carina_core::resource::Current::Present(present) => present.into_parts().0,
            carina_core::resource::Current::MissingIdentifier(state) => state.into_state(),
            carina_core::resource::Current::CreateInFlight(pending) => {
                panic!("saved applied row unexpectedly remained in flight: {pending:?}")
            }
        };
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let second_plan = create_plan(
            std::slice::from_ref(&resolved(desired.clone())),
            &[],
            &ProviderRouter::new(),
            carina_core::differ::PlanPriorStates::new(
                &crate::plan_input_current_from_states(current_states.clone(), &schemas, &[]),
                &carina_core::differ::Orphans::new(),
            ),
            &HashMap::new(),
            &schemas,
            &crate::empty_lifted_saved_attrs(),
            &HashMap::new(),
            &[],
        )
        .expect("test provider hints should succeed");

        assert_eq!(
            second_plan.mutation_count(),
            0,
            "second plan after write-only secret-to-plain demotion must be empty"
        );
    }

    #[test]
    fn cbd_replacement_does_not_add_new_write_only_secret_to_deposed_old_generation() {
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
                    Value::Concrete(ConcreteValue::String("new.example.com".into())),
                )
                .with_attribute("password", secret_string("new-password"));
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("new.example.com".into())),
            )]),
        )
        .with_identifier("new-record-id");
        let current_state = State::existing(
            id.clone(),
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("old.example.com".into())),
            )]),
        )
        .with_identifier("old-record-id");
        let existing = ResourceState::try_applied(
            "route53.Record",
            "validation_records[0]",
            "aws",
            "old-record-id",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("name", serde_json::json!("old.example.com"));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("old current fixture must be valid");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            (ResourceSchema::new("route53.Record")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: &[],
                runtime_synthesized_resources: std::slice::from_ref(&resolved(runtime_child)),
                managed_current: &crate::managed_current_from_states(HashMap::from([(
                    id.clone(),
                    current_state,
                )])),
                applied_states: &HashMap::from([(id.clone(), complete_create(applied_state))]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("create-before-destroy replacement should write state");

        let deposed = state
            .find_resource_for_id(&id)
            .expect("replacement row should be persisted")
            .deposed
            .iter()
            .find(|generation| generation.identifier == "old-record-id")
            .expect("old generation should be deposed");
        assert!(
            !deposed.persisted_attributes().contains_key("password"),
            "the old generation never received the new write-only password"
        );
    }

    #[test]
    fn replacement_plan_fallback_does_not_persist_provider_echoed_write_only_value() {
        let mut desired = Resource::with_provider("mock", "test.resource", "main", None)
            .with_attribute(
                "replacement_key",
                Value::Concrete(ConcreteValue::String("new".into())),
            );
        desired.directives.create_before_destroy = true;
        let id = desired.id.clone();
        let current_state = State::existing(
            id.clone(),
            HashMap::from([
                (
                    "replacement_key".to_string(),
                    Value::Concrete(ConcreteValue::String("old".into())),
                ),
                (
                    "password".to_string(),
                    Value::Concrete(ConcreteValue::String("provider-echo".into())),
                ),
            ]),
        )
        .with_identifier("old-id");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            (ResourceSchema::new("test.resource")
                .attribute(
                    AttributeSchema::optional("replacement_key", AttributeType::string())
                        .create_only()
                        .build(),
                )
                .attribute(
                    AttributeSchema::optional("password", AttributeType::string())
                        .write_only()
                        .build(),
                )
                .with_coexisting_replacement())
            .validate()
            .expect("schema fixture must be valid"),
        );
        let sorted_resources = vec![resolved(desired.clone())];
        let current_states = HashMap::from([(id.clone(), current_state.clone())]);
        let plan = create_plan(
            &sorted_resources,
            &[],
            &ProviderRouter::new(),
            carina_core::differ::PlanPriorStates::new(
                &crate::plan_input_current_from_states(current_states.clone(), &schemas, &[]),
                &carina_core::differ::Orphans::new(),
            ),
            &HashMap::new(),
            &schemas,
            &crate::empty_lifted_saved_attrs(),
            &HashMap::new(),
            &[],
        )
        .expect("create-only change should plan a replacement");
        assert!(
            plan.replace_display_info()
                .next()
                .is_some_and(|replacement| replacement.create_before_destroy),
            "fixture must exercise the create-before-destroy replacement fallback"
        );
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "replacement_key".to_string(),
                Value::Concrete(ConcreteValue::String("new".into())),
            )]),
        )
        .with_identifier("new-id");

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: &sorted_resources,
                runtime_synthesized_resources: &[],
                managed_current: &crate::managed_current_from_states(current_states),
                applied_states: &HashMap::from([(id.clone(), complete_create(applied_state))]),
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("replacement writeback should succeed");

        let deposed = state
            .find_resource_for_id(&id)
            .expect("replacement row should be persisted")
            .deposed
            .iter()
            .find(|generation| generation.identifier == "old-id")
            .expect("failed old delete should leave a deposed generation");
        assert!(
            !deposed.persisted_attributes().contains_key("password"),
            "plan-time provider echoes are not previous persisted values"
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
                        ConcreteValue::String("plain-secret".into()),
                    )))),
                );
        let applied_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("new-record-id");
        let current_state = State::existing(
            id.clone(),
            HashMap::from([
                (
                    "password".to_string(),
                    Value::Concrete(ConcreteValue::String("plain-secret".into())),
                ),
                (
                    "unknown".to_string(),
                    Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
                        path: carina_core::resource::AccessPath::with_fields(
                            "bootstrap",
                            "network",
                            vec!["unknown".into()],
                        ),
                    })),
                ),
            ]),
        )
        .with_identifier("old-record-id");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: &[],
                runtime_synthesized_resources: std::slice::from_ref(&resolved(runtime_child)),
                managed_current: &crate::managed_current_from_states(current_states.clone()),
                applied_states: &applied_states,
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &carina_core::schema::SchemaRegistry::new(),
            },
            None,
        )
        .expect("unknown deposed fallback attributes should be skipped, not fail writeback");

        let saved = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("new child row should be persisted");
        let deposed = saved
            .deposed
            .first()
            .expect("old instance should be deposed");
        let persisted = deposed.persisted_attributes();
        let stored_secret = persisted
            .redacted_secret_hash("password")
            .expect("secret fallback should be stored as a hash");
        assert!(stored_secret.encoded_digest().starts_with("$argon2id$"));
        assert!(
            !stored_secret.encoded_digest().contains("plain-secret"),
            "deposed attributes must not persist plaintext secrets"
        );
        assert!(
            !deposed.persisted_attributes().contains_key("unknown"),
            "unserializable fallback attributes should be skipped"
        );
    }

    #[test]
    fn deferred_replace_depose_hashes_old_secret_demoted_in_new_desired() {
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
                    Value::Concrete(ConcreteValue::String("plain-new".into())),
                );
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "password".to_string(),
                Value::Concrete(ConcreteValue::String("plain-new".into())),
            )]),
        )
        .with_identifier("new-record-id");
        let current_state = State::existing(
            id.clone(),
            HashMap::from([(
                "password".to_string(),
                Value::Concrete(ConcreteValue::String("plain-old".into())),
            )]),
        )
        .with_identifier("old-record-id");
        let existing = ResourceState::try_applied(
            "route53.Record",
            "validation_records[0]",
            "aws",
            "stale-record-id",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("password", structural_legacy_secret(&"ab".repeat(32)));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: &[],
                runtime_synthesized_resources: std::slice::from_ref(&resolved(runtime_child)),
                managed_current: &crate::managed_current_from_states(current_states.clone()),
                applied_states: &applied_states,
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &SchemaRegistry::new(),
            },
            None,
        )
        .expect("deferred replacement should write state");

        let saved = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("new child row should be persisted");
        let deposed = saved
            .deposed
            .iter()
            .find(|entry| entry.identifier == "old-record-id")
            .expect("old instance should be deposed");
        let stored_secret = deposed
            .persisted_attributes()
            .redacted_secret_hash("password")
            .expect("old plaintext password should be re-hashed");
        assert!(stored_secret.encoded_digest().starts_with("$argon2id$"));
        assert!(!stored_secret.encoded_digest().contains("plain-old"));
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
                    Value::Concrete(ConcreteValue::String("new.example.com".into())),
                );
        let applied_state =
            State::existing(id.clone(), HashMap::new()).with_identifier("new-record-id");
        let current_state = State::existing(
            id.clone(),
            HashMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("old.example.com".into())),
                ),
                (
                    "password".to_string(),
                    Value::Concrete(ConcreteValue::String("plain-secret".into())),
                ),
            ]),
        )
        .with_identifier("old-record-id");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            (ResourceSchema::new("route53.Record").attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file: StateFile::new(),
                sorted_resources: &[],
                runtime_synthesized_resources: std::slice::from_ref(&resolved(runtime_child)),
                managed_current: &crate::managed_current_from_states(current_states.clone()),
                applied_states: &applied_states,
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("schema-protected removed attributes should not fail writeback");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("new child row should be persisted");
        let deposed = row.deposed.first().expect("old instance should be deposed");
        assert_eq!(
            deposed.persisted_attributes().get("name"),
            carina_core::value::json_to_dsl_value(&serde_json::json!("old.example.com"))
        );
        assert!(
            !deposed.persisted_attributes().contains_key("password"),
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
                id: ResolvedResourceId::new(id.clone()),
                instance: DeposedInstance::from_persisted_json(
                    DeposedKey::new_unique(),
                    "old-record-id".to_string(),
                    Some("west".to_string()),
                    HashMap::from([("name".to_string(), serde_json::json!("old.example.com"))]),
                    BTreeSet::from(["cert".to_string()]),
                ),
            },
        )
        .expect("test deposed setup must be valid");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("missing row should be created for deposed instance");
        assert_eq!(row.identifier(), None);
        assert!(row.persisted_attributes().is_none());
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
        row.deposed.push(DeposedInstance::from_persisted_json(
            east_key.clone(),
            "old-record-id".to_string(),
            Some("east".to_string()),
            HashMap::from([("name".to_string(), serde_json::json!("stale"))]),
            BTreeSet::from(["old".to_string()]),
        ));
        let mut state = StateFile::new();
        state
            .upsert_resource(row)
            .expect("test state setup must be valid");

        apply_planned_depose(
            &mut state,
            PlannedDepose {
                id: ResolvedResourceId::new(id.clone()),
                instance: DeposedInstance::from_persisted_json(
                    west_key.clone(),
                    "old-record-id".to_string(),
                    Some("west".to_string()),
                    HashMap::from([("name".to_string(), serde_json::json!("fresh"))]),
                    BTreeSet::from(["new".to_string()]),
                ),
            },
        )
        .expect("test deposed setup must be valid");

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
                id: ResolvedResourceId::new(id),
                instance: DeposedInstance::from_persisted_json(
                    replacement_west_key.clone(),
                    "old-record-id".to_string(),
                    Some("west".to_string()),
                    HashMap::from([("name".to_string(), serde_json::json!("newer"))]),
                    BTreeSet::from(["newer".to_string()]),
                ),
            },
        )
        .expect("test deposed setup must be valid");

        let row = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("row should still exist");
        assert_eq!(row.deposed.len(), 2);
        let deposed = &row.deposed[1];
        assert_eq!(deposed.identifier, "old-record-id");
        assert_eq!(deposed.key, replacement_west_key);
        assert_eq!(deposed.provider_instance.as_deref(), Some("west"));
        assert_eq!(
            deposed.persisted_attributes().get("name"),
            carina_core::value::json_to_dsl_value(&serde_json::json!("newer"))
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
                Value::Concrete(ConcreteValue::String("old.example.com".into())),
            )]),
        )
        .with_identifier("old-record-id");
        let first_key = DeposedKey::new_unique();
        let mut existing = ResourceState::try_applied(
            "route53.Record",
            "validation_records[0]",
            "aws",
            "old-record-id",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("name", serde_json::json!("old.example.com"));
        existing.deposed.push(DeposedInstance::from_persisted_json(
            first_key.clone(),
            "older-record-id".to_string(),
            None,
            HashMap::from([("name".to_string(), serde_json::json!("older"))]),
            BTreeSet::from(["cert".to_string()]),
        ));
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let mut plan = Plan::new();
        plan.add(deferred_replace_effect(&id));
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: &[],
                runtime_synthesized_resources: std::slice::from_ref(&resolved(runtime_child)),
                managed_current: &crate::managed_current_from_states(current_states.clone()),
                applied_states: &applied_states,
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &carina_core::schema::SchemaRegistry::new(),
            },
            None,
        )
        .expect("writeback should preserve existing deposed entries and append the new one");

        let saved = state
            .find_resource("aws", "route53.Record", "validation_records[0]")
            .expect("new child row should be persisted");
        assert_eq!(
            saved.identifier().map(|value| value.as_str()),
            Some("new-record-id")
        );
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
        let current_states: HashMap<ResourceId, Current> = HashMap::new();
        let applied_states = HashMap::new();
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(id.clone(), "old-record-id")]);
        let failed_refreshes = HashSet::new();
        let schemas = SchemaRegistry::new();

        let wb = decompose(DecomposeInput {
            state_file: &StateFile::new(),
            sorted_resources: &[],
            runtime_synthesized_resources: &[],
            managed_current: &current_states,
            applied_states: &applied_states,
            plan: &plan,
            successfully_deleted: &successfully_deleted,
            failed_refreshes: &failed_refreshes,
            schemas: &schemas,
        })
        .expect("delete-only half of DeferredReplace should still write cleanup");

        assert!(wb.upserts.is_empty());
        assert_eq!(wb.cleanups, HashSet::from([ResolvedResourceId::new(id)]));
    }

    #[test]
    fn plain_replacement_delete_success_create_failure_writes_cleanup() {
        let resource = Resource::new("ec2.Vpc", "main").with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
        );
        let id = resource.id.clone();
        let sorted_resources = vec![resolved(resource)];
        let current_state = State::existing(
            id.clone(),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )]),
        )
        .with_identifier("vpc-old");
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "",
            (ResourceSchema::new("ec2.Vpc").attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let plan = create_plan(
            &sorted_resources,
            &[],
            &ProviderRouter::new(),
            carina_core::differ::PlanPriorStates::new(
                &crate::plan_input_current_from_states(
                    current_states.clone(),
                    &SchemaRegistry::new(),
                    &[],
                ),
                &carina_core::differ::Orphans::new(),
            ),
            &HashMap::new(),
            &schemas,
            &crate::empty_lifted_saved_attrs(),
            &HashMap::new(),
            &[],
        )
        .expect("test provider hints should succeed");
        assert_eq!(plan.replace_display_info().count(), 1);
        let applied_states = HashMap::new();
        let successfully_deleted =
            HashSet::from([DeletedInstanceKey::current(id.clone(), "vpc-old")]);
        let failed_refreshes = HashSet::new();

        let managed_current = crate::managed_current_from_states(current_states.clone());
        let wb = decompose(DecomposeInput {
            state_file: &StateFile::new(),
            sorted_resources: &sorted_resources,
            runtime_synthesized_resources: &[],
            managed_current: &managed_current,
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
        assert_eq!(wb.cleanups, HashSet::from([ResolvedResourceId::new(id)]));
    }

    #[test]
    fn moved_target_replacement_delete_failure_deposes_old_instance() {
        let from_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "old_bucket", None);
        let mut resource = Resource::with_provider("awscc", "s3.Bucket", "new_bucket", None)
            .with_attribute(
                "bucket_name",
                Value::Concrete(ConcreteValue::String("new-name".into())),
            );
        resource.directives.create_before_destroy = true;
        let id = resource.id.clone();
        let sorted_resources = vec![resolved(resource)];
        let current_state = State::existing(
            id.clone(),
            HashMap::from([
                (
                    "bucket_name".to_string(),
                    Value::Concrete(ConcreteValue::String("old-name".into())),
                ),
                (
                    "comment".to_string(),
                    Value::Concrete(ConcreteValue::String("previously-secret".into())),
                ),
            ]),
        )
        .with_identifier("bucket-old-id");
        let current_states = HashMap::from([(id.clone(), current_state)]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            (ResourceSchema::new("s3.Bucket")
                .attribute(
                    AttributeSchema::optional("bucket_name", AttributeType::string())
                        .create_only()
                        .build(),
                )
                .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
                .with_coexisting_replacement())
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut plan = create_plan(
            &sorted_resources,
            &[],
            &ProviderRouter::new(),
            carina_core::differ::PlanPriorStates::new(
                &crate::plan_input_current_from_states(current_states.clone(), &schemas, &[]),
                &carina_core::differ::Orphans::new(),
            ),
            &HashMap::new(),
            &schemas,
            &crate::empty_lifted_saved_attrs(),
            &HashMap::new(),
            &[],
        )
        .expect("test provider hints should succeed");
        let replacement = plan
            .replace_display_info()
            .next()
            .expect("create-only diff should produce replacement metadata");
        assert!(replacement.create_before_destroy);
        let state_blocks: Vec<carina_core::parser::StateBlock> = Vec::new();
        let state_file_for_wiring = Some(StateFile::new());
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let unresolved_upstreams: HashSet<&str> = HashSet::new();

        let raw_sorted_resources: Vec<Resource> = sorted_resources
            .iter()
            .map(|resource| resource.as_resource().clone())
            .collect();
        crate::wiring::add_state_block_effects(
            &mut plan,
            &state_blocks,
            &state_file_for_wiring,
            &[(
                ResolvedResourceId::new(from_id.clone()),
                ResolvedResourceId::new(id.clone()),
            )],
            &raw_sorted_resources,
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

        let persisted_secret = PersistedValue::encode_provider_value(
            &Value::Deferred(carina_core::resource::DeferredValue::Secret(Box::new(
                Value::Concrete(ConcreteValue::String("previously-secret".into())),
            ))),
            None,
        )
        .expect("persist previous secret marker");
        let mut existing =
            ResourceState::try_applied("s3.Bucket", "old_bucket", "awscc", "bucket-old-id")
                .expect("fixture identifier is non-empty")
                .with_attribute("bucket_name", serde_json::json!("old-name"))
                .with_attribute("comment", persisted_secret.as_json().clone());
        existing.dependency_bindings.insert("logs".to_string());
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(existing)
            .expect("test state setup must be valid");
        let applied_state = State::existing(
            id.clone(),
            HashMap::from([(
                "bucket_name".to_string(),
                Value::Concrete(ConcreteValue::String("new-name".into())),
            )]),
        )
        .with_identifier("bucket-new-id");
        let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);

        let state = build_state_after_apply(
            ApplyStateSave {
                state_file,
                sorted_resources: &sorted_resources,
                runtime_synthesized_resources: &[],
                managed_current: &crate::managed_current_from_states(current_states.clone()),
                applied_states: &applied_states,
                plan: &plan,
                successfully_deleted: &HashSet::new(),
                failed_refreshes: &HashSet::new(),
                schemas: &schemas,
            },
            None,
        )
        .expect("moved replacement writeback should preserve the failed delete as deposed");

        let row = state
            .find_resource("awscc", "s3.Bucket", "new_bucket")
            .expect("moved target row should be persisted");
        assert_eq!(
            row.identifier().map(|value| value.as_str()),
            Some("bucket-new-id")
        );
        assert_eq!(row.deposed.len(), 1);
        assert_eq!(row.deposed[0].identifier, "bucket-old-id");
        assert_eq!(row.deposed[0].provider_instance, None);
        assert_eq!(
            row.deposed[0].persisted_attributes().get("bucket_name"),
            carina_core::value::json_to_dsl_value(&serde_json::json!("old-name"))
        );
        let deposed_attributes = serde_json::to_value(row.deposed[0].persisted_attributes())
            .expect("serialize deposed attributes");
        assert_eq!(
            &deposed_attributes["comment"],
            persisted_secret.as_json(),
            "Move + Replace must use the source row as previous-hash authority when deposing the old generation",
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
            origin: None,
        });
        let current_states = HashMap::new();
        let applied_states = HashMap::from([(id.clone(), complete_create(applied_state))]);
        let successfully_deleted = HashSet::new();
        let failed_refreshes = HashSet::new();
        let schemas = SchemaRegistry::new();

        let desired = resolved(desired);
        let result = decompose(DecomposeInput {
            state_file: &StateFile::new(),
            sorted_resources: std::slice::from_ref(&desired),
            runtime_synthesized_resources: &[],
            managed_current: &current_states,
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
mod export_writeback_tests {
    use super::*;
    use carina_core::parser::ExpandedInferredExportParam as InferredExportParam;
    use carina_core::resource::{
        AccessPath, ApplyTimeUnknown, ConcreteValue, DeferredValue, InterpolationPart,
        PartialReadMarker, PendingReRead, Value,
    };
    use carina_state::{ResourceState, StateFile};

    fn export_param(name: &str, value: Value) -> InferredExportParam {
        InferredExportParam {
            name: name.to_string(),
            static_type: carina_core::static_type::ExportStaticType::Inferred(
                carina_core::static_type::StaticType::Unchecked(
                    carina_core::static_type::InferredUncheckedReason::NotInferable(
                        carina_core::static_type::NotInferableReason::SchemaUnavailable {
                            binding: "test fixture".to_string(),
                        },
                    ),
                ),
            ),
            value: Some(value),
        }
    }

    fn resolve_export_parts(
        export_params: &[InferredExportParam],
    ) -> (HashMap<String, PersistedExportValue>, Vec<SkippedExport>) {
        let state = StateFile::new();
        let post_apply_states = PostApplyStates::from_current_and_state(
            &HashMap::new(),
            &DataSourceValues::empty(),
            &state,
        );
        let checked = carina_core::validation::check_export_values_pre_apply(
            export_params,
            &carina_core::binding_index::ResolvedBindings::default(),
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
        )
        .expect("test exports must pass the secret boundary");
        let values = resolve_export_values_post_apply(ResolveExportValuesPostApplyInput {
            export_params: &checked,
            sorted_resources: &[],
            pre_resolve_compositions: &[],
            post_apply_states: &post_apply_states,
            schemas: &carina_core::schema::SchemaRegistry::new(),
            wait_aliases: &[],
            provider_context: &carina_core::parser::LoadedProviderContext::empty_for_test(),
        })
        .expect("post-apply export resolution should succeed");
        materialize_exports(&values)
            .expect("unresolved exports should be skipped, not abort writeback")
            .into_parts()
    }

    #[test]
    fn partial_read_row_keeps_runtime_unknown_out_of_persisted_exports() {
        let id = ResourceId::with_provider_identity("mock", "test.Resource", "main", None);
        let provider_state = State::existing(id.clone(), HashMap::new()).with_identifier("main-id");
        let mut row =
            ResourceState::from_provider_state("test.Resource", "main", "mock", &provider_state)
                .expect("partial-read fixture must encode");
        row = crate::fixture_resource_state_metadata(
            row,
            None,
            Some(PartialReadMarker::new(
                PendingReRead::incomplete(
                    "provider read after create did not return every attribute",
                ),
                BTreeSet::from(["computed".to_string()]),
            )),
        );
        row.dependency_bindings = BTreeSet::from(["upstream".to_string()]);
        let mut state = StateFile::new();
        state
            .upsert_resource(row)
            .expect("partial-read fixture must be valid");

        let resource =
            Resource::with_provider("mock", "test.Resource", "main", None).with_binding("instance");
        let reference = Value::resource_ref("instance", "computed", vec![]);
        let export_params = [export_param("computed", reference.clone())];
        let checked = carina_core::validation::check_export_values_pre_apply(
            &export_params,
            &carina_core::binding_index::ResolvedBindings::default(),
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
        )
        .expect("test export must pass the secret boundary");
        let post_apply_states = PostApplyStates::from_current_and_state(
            &HashMap::new(),
            &DataSourceValues::empty(),
            &state,
        );
        let current = post_apply_states
            .managed_current()
            .get(&id)
            .expect("partial-read row must be in the post-apply view");
        match current {
            Current::Absent(state) => panic!("partial-read row was classified absent: {state:?}"),
            Current::Present(present) => {
                assert!(matches!(
                    &present.state().partial_read,
                    Some(marker) if marker.missing_attributes().contains("computed")
                ));
                assert_eq!(
                    present.state().dependency_bindings,
                    BTreeSet::from(["upstream".to_string()])
                );
            }
            Current::MissingIdentifier(state) => {
                panic!("partial-read row lost its identifier: {state:?}")
            }
            Current::CreateInFlight(pending) => {
                panic!("partial-read row unexpectedly remained in flight: {pending:?}")
            }
        }

        let values = resolve_export_values_post_apply(ResolveExportValuesPostApplyInput {
            export_params: &checked,
            sorted_resources: std::slice::from_ref(&resource),
            pre_resolve_compositions: &[],
            post_apply_states: &post_apply_states,
            schemas: &carina_core::schema::SchemaRegistry::new(),
            wait_aliases: &[],
            provider_context: &carina_core::parser::LoadedProviderContext::empty_for_test(),
        })
        .expect("post-apply export resolution should succeed");

        assert!(matches!(
            &values.as_slice()[0].value,
            Some(Value::Deferred(DeferredValue::Unknown(
                ApplyTimeUnknown::PendingReRead(pending)
            ))) if pending == &PendingReRead::incomplete(
                "provider read after create did not return every attribute"
            )
        ));
        let (_, skipped) = materialize_exports(&values)
            .expect("unresolved export must be skippable")
            .into_parts();
        assert_eq!(skipped.len(), 1);
        assert_eq!(
            skipped[0].reason,
            "value not yet known (read after apply was incomplete: provider read after create did not return every attribute)"
        );
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

        assert_eq!(
            resolved.get("ok_export").map(PersistedExportValue::as_json),
            Some(&serde_json::json!("ok"))
        );
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
        assert_eq!(
            resolved.get("ok_export").map(PersistedExportValue::as_json),
            Some(&serde_json::json!("ok"))
        );
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
                InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                    path: AccessPath::new("missing", "value"),
                })),
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
                name: carina_core::builtins::BuiltinFunction::join,
                args: vec![
                    Value::Concrete(ConcreteValue::String("-".into())).into(),
                    Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
                        DeferredValue::ResourceRef {
                            path: AccessPath::new("missing", "value"),
                        },
                    )]))
                    .into(),
                ],
                source_line: 0.into(),
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
        state.insert_export_json_for_test("ax", serde_json::json!("old-a"));
        state.insert_export_json_for_test("bx", serde_json::json!("old-b"));
        state.insert_export_json_for_test("orphan", serde_json::json!("old-orphan"));

        let skipped = ExportResolution {
            resolved: HashMap::from([(
                "ax".to_string(),
                serde_json::from_value(serde_json::json!("new-a")).unwrap(),
            )]),
            skipped: vec![SkippedExport {
                name: "bx".to_string(),
                reason: "unresolved reference b.name".to_string(),
            }],
        }
        .write_into(&mut state);

        assert_eq!(skipped.len(), 1);
        assert_eq!(state.exports.len(), 2);
        assert_eq!(
            state.exports.get("ax").map(PersistedExportValue::as_json),
            Some(&serde_json::json!("new-a"))
        );
        assert_eq!(
            state.exports.get("bx").map(PersistedExportValue::as_json),
            Some(&serde_json::json!("old-b"))
        );
        assert!(!state.exports.contains_key("orphan"));
    }

    #[test]
    fn skipped_exports_failure_message_names_each_export() {
        let skipped = SkippedExports::from_names_for_test(&["role_arn", "policy_arn"]);

        assert_eq!(
            skipped.failure_message(),
            "2 exports not written to state: role_arn, policy_arn (see '! export ...' lines above).",
        );
    }
}

#[cfg(test)]
mod stage4_unknown_err_tests {
    use super::*;
    use carina_core::resource::{
        AccessPath, ApplyTimeUnknown, ConcreteValue, DeferredValue, Value,
    };
    use carina_core::value::{SerializationContext, SerializationError};

    /// RFC #2371 stage 4 contract pin: the persisted export encoder returns
    /// `Err(SerializationError::UnknownNotAllowed)` for `Value::Deferred(DeferredValue::Unknown)`.
    /// State files must never carry the variant (constraint b); a
    /// silent fallback would re-introduce v1 corruption.
    #[test]
    fn unknown_returns_err_from_persisted_export_encode() {
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        let v = Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
            path: path.clone(),
        }));
        let err = PersistedExportValue::encode(&v).unwrap_err();
        match err {
            SerializationError::UnknownNotAllowed {
                reason: ApplyTimeUnknown::UpstreamExport { path: p },
                context: SerializationContext::StateWriteback,
            } => assert_eq!(p, path),
            other => {
                panic!("expected UnknownNotAllowed/UpstreamExport/StateWriteback, got: {other:?}")
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
        let err = PersistedExportValue::encode(&v).unwrap_err();
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

    #[test]
    fn coalesce_returns_unresolved_err() {
        let value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("net", "optional", vec![])),
            default: Box::new(Value::Concrete(ConcreteValue::String("fallback".into()))),
        });
        let error = PersistedExportValue::encode(&value).unwrap_err();
        assert!(matches!(
            error,
            SerializationError::UnresolvedCoalesce {
                context: SerializationContext::StateWriteback,
            }
        ));
    }

    /// `Value::Deferred(DeferredValue::Interpolation)` reaching apply-time is a resolver /
    /// canonicalize bug — surface as `UnresolvedInterpolation`. (#2386)
    #[test]
    fn interpolation_returns_unresolved_err() {
        use carina_core::resource::InterpolationPart;
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("x".into()),
        ]));
        let err = PersistedExportValue::encode(&v).unwrap_err();
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
            name: carina_core::builtins::BuiltinFunction::join,
            args: vec![],
            source_line: 0.into(),
        });
        let err = PersistedExportValue::encode(&v).unwrap_err();
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

    /// `Value::Deferred(DeferredValue::Secret)` must never be silently dropped. Export
    /// validation rejects it earlier, and this serialization boundary is the invariant
    /// backstop.
    #[test]
    fn secret_returns_an_error() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("password".into()),
        ))));
        let error = PersistedExportValue::encode(&v).expect_err("secret export must be rejected");
        assert!(
            matches!(
                error,
                SerializationError::SecretNotAllowed {
                    context: SerializationContext::StateWriteback,
                }
            ),
            "{error}",
        );
    }

    fn enum_shaped_user_map() -> Value {
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "Enum".to_string(),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
                (
                    "identity".to_string(),
                    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
                        (
                            "provider".to_string(),
                            Value::Concrete(ConcreteValue::String("aws".into())),
                        ),
                        (
                            "segments".to_string(),
                            Value::Concrete(ConcreteValue::List(vec![
                                Value::Concrete(ConcreteValue::String("ec2".into())),
                                Value::Concrete(ConcreteValue::String("Eip".into())),
                            ])),
                        ),
                        (
                            "kind".to_string(),
                            Value::Concrete(ConcreteValue::String("Domain".into())),
                        ),
                    ]))),
                ),
                (
                    "api_value".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc".into())),
                ),
            ]))),
        )])))
    }

    #[test]
    fn enum_shaped_user_map_round_trips_through_attribute_and_export_writeback() {
        let value = enum_shaped_user_map();
        let id = ResourceId::with_provider_identity("mock", "test.Resource", "main", None);
        let state = State::existing(id, HashMap::from([("payload".to_string(), value.clone())]));
        let row = ResourceState::from_provider_state("test.Resource", "main", "mock", &state)
            .expect("attribute writeback succeeds");

        assert_eq!(
            row.persisted_attributes()
                .expect("row has applied attributes")
                .get("payload"),
            Some(value.clone())
        );

        let export_json = PersistedExportValue::encode(&value).expect("export writeback succeeds");
        assert_eq!(export_json.decode(), Some(value));
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
            PersistedExportValue::encode(&v).unwrap().as_json(),
            &serde_json::json!({
                "$enum": {
                    "identity": {
                        "provider": "aws",
                        "segments": ["ec2", "Eip"],
                        "kind": "Domain"
                    },
                    "api_value": "vpc"
                }
            })
        );
    }

    /// Non-finite floats (NaN / +inf / -inf) cannot be represented in
    /// JSON. The persisted export encoder must surface them as
    /// `SerializationError::NonFiniteFloat` rather than silently dropping the export.
    /// (#2859)
    #[test]
    fn non_finite_float_returns_err() {
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let v = Value::Concrete(ConcreteValue::Float(f));
            let err = PersistedExportValue::encode(&v).unwrap_err();
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
