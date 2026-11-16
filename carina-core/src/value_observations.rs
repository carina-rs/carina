//! Plan-owned observation protection. Callers supply complete inputs, never a
//! selected per-row secret authority.
use super::*;
use crate::data_source_value::{DataSourceValue, DataSourceValues};
use crate::plan::Plan;
use crate::resource::{Current, ResourceId, SnapshotState};

/// Binding annotation uses a separate lookup and never mints output products.
pub trait BindingState {
    fn observation_attributes(
        &self,
        id: &ResourceId,
    ) -> Option<&crate::provider::PersistedAttributes>;
}

/// A complete decoded state-file authority. Any crate may decode a complete
/// document; no caller can mint it from a lookup, selected rows or `None`.
/// The state crate uses this same decoder after its legacy wire migrations.
#[derive(Debug, Clone)]
pub struct ObservationState {
    document: crate::state_document::DecodedStateDocument,
    rows: HashMap<ResourceId, usize>,
}
impl ObservationState {
    /// The sole document decoder for both state loading and observation
    /// authority. Validates the state/export version gate and every typed row,
    /// generation, persisted value, dependency and export before minting authority.
    pub fn read_state_file(
        bytes: &[u8],
    ) -> Result<Self, crate::state_document::StateDocumentDecodeError> {
        let document = crate::state_document::decode_state_document(bytes)?;
        let rows = document
            .resources
            .iter()
            .enumerate()
            .map(|(index, row)| {
                // State loading canonicalizes legacy map-key addresses after
                // decoding. Index the same row under that spelling without
                // serializing or decoding its persisted values again.
                let identity = crate::resource::ResourceIdentity::try_from(
                    crate::utils::canonicalize_map_key_address(row.identity.as_str()),
                )
                .map_err(|source| {
                    crate::state_document::StateDocumentDecodeError::InvalidResourceIdentity {
                        resource_index: index,
                        provider: row.provider.clone(),
                        resource_type: row.resource_type.clone(),
                        identifier: None,
                        source,
                    }
                })?;
                Ok((
                    ResourceId::with_provider_identity(
                        &row.provider,
                        &row.resource_type,
                        identity,
                        row.directives.provider_instance.clone(),
                    ),
                    index,
                ))
            })
            .collect::<Result<_, crate::state_document::StateDocumentDecodeError>>()?;
        Ok(Self { document, rows })
    }
    /// Retain immutable authority alongside the mutable state model made from
    /// this decode. Field-order metadata is shared by both views.
    pub fn into_document_and_authority(
        self,
    ) -> (crate::state_document::DecodedStateDocument, Self) {
        (self.document.clone(), self)
    }
    /// Consume the decoded document for the state crate's mutable domain model.
    /// A caller-built document cannot be converted back into observation authority.
    pub fn into_document(self) -> crate::state_document::DecodedStateDocument {
        self.document
    }
    fn row(
        &self,
        id: &ResourceId,
    ) -> Option<(
        &crate::provider::PersistedAttributes,
        &crate::provider::NestedWriteOnlyRecord,
    )> {
        use crate::state_document::CurrentInstance;
        let row = &self.document.resources[*self.rows.get(id)?];
        match &row.current {
            CurrentInstance::Applied {
                attributes,
                nested_write_only,
                ..
            }
            | CurrentInstance::MissingIdentifier {
                attributes,
                nested_write_only,
                ..
            } => Some((attributes, nested_write_only)),
            CurrentInstance::None
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. } => None,
        }
    }
    fn generation_attributes(
        &self,
        id: &ResourceId,
        generation: &crate::effect::EffectGeneration,
        identifier: &str,
    ) -> Option<&crate::provider::PersistedAttributes> {
        match generation {
            crate::effect::EffectGeneration::Current => {
                self.row(id).map(|(attributes, _)| attributes)
            }
            crate::effect::EffectGeneration::Deposed(key) => self.document.resources
                [*self.rows.get(id)?]
            .deposed
            .iter()
            .find(|old| {
                old.key == *key
                    && old.identifier == identifier
                    && old.provider_instance == id.provider_instance
            })
            .map(|old| &old.attributes),
        }
    }
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn empty_for_test() -> Self {
        Self::read_state_file(
            &serde_json::to_vec(&serde_json::json!({
                "version": crate::state_document::CURRENT_STATE_VERSION,
                "serial": 0,
                "lineage": "test",
                "carina_version": "test",
                "exports_version": crate::state_document::CURRENT_EXPORTS_VERSION,
                "exports": {},
                "resources": [],
            }))
            .expect("empty fixture document encodes"),
        )
        .expect("empty fixture document decodes")
    }
    pub fn protect<S: Clone, V: crate::resource::PlanValueKind>(
        &self,
        inputs: ObservationInputs<'_, S, V>,
    ) -> Result<ProtectedObservations, SerializationError> {
        ProtectedObservations::build(inputs, self)
    }
}

/// Complete inputs at the two output boundaries. Saved-apply comparisons have
/// already been rekeyed by the verified move transition and retain their full
/// planned history, including placeholders for omitted desired attributes.
pub enum ObservationInputs<
    'a,
    S = crate::resource::ModuleSource,
    V: crate::resource::PlanValueKind = crate::resource::NoPlaceholder,
> {
    /// Historical rows use the desired inputs already retained in the plan.
    Display {
        plan: &'a Plan<S, V>,
        current: &'a HashMap<ResourceId, Current>,
        deleted: &'a HashMap<crate::effect::DeletedInstanceKey, SnapshotState>,
    },
    Plan {
        plan: &'a Plan<S, V>,
        current: &'a HashMap<ResourceId, Current>,
        deleted: &'a HashMap<crate::effect::DeletedInstanceKey, SnapshotState>,
        reads: &'a DataSourceValues<S>,
    },
    SavedApply {
        plan: &'a Plan<S, V>,
        history: &'a HashMap<ResourceId, Current>,
        planned: &'a SnapshotState,
        actual: &'a SnapshotState,
    },
}

#[derive(Debug)]
pub struct ProtectedObservations {
    plan_identity: std::sync::Arc<()>,
    managed: HashMap<ResourceId, ProtectedObservation>,
    updates: HashMap<ResourceId, ProtectedObservation>,
    deleted: HashMap<crate::effect::DeletedInstanceKey, ProtectedObservation>,
    old_instances: HashMap<crate::effect::DeletedInstanceKey, ProtectedObservation>,
    replacements: HashMap<usize, ProtectedObservation>,
    deferred: HashMap<crate::effect::DeletedInstanceKey, ProtectedObservation>,
    reads: HashMap<ResourceId, ProtectedObservation>,
    comparisons: Option<ProtectedComparisons>,
}

#[derive(Debug)]
pub struct ProtectedComparisons {
    pub planned: ProtectedObservation,
    pub actual: ProtectedObservation,
}

impl ProtectedObservations {
    /// The batch mint for protected observations. Lookup by observed
    /// identity happens here against the whole desired set and loaded history.
    fn build<S: Clone, V: crate::resource::PlanValueKind>(
        inputs: ObservationInputs<'_, S, V>,
        persisted: &ObservationState,
    ) -> Result<Self, SerializationError> {
        let plan_identity = match &inputs {
            ObservationInputs::Display { plan, .. }
            | ObservationInputs::Plan { plan, .. }
            | ObservationInputs::SavedApply { plan, .. } => {
                std::sync::Arc::clone(&plan.observation_identity)
            }
        };
        let mut output = Self {
            plan_identity,
            managed: HashMap::new(),
            updates: HashMap::new(),
            deleted: HashMap::new(),
            old_instances: HashMap::new(),
            replacements: HashMap::new(),
            deferred: HashMap::new(),
            reads: HashMap::new(),
            comparisons: None,
        };
        match inputs {
            ObservationInputs::Display {
                plan,
                current,
                deleted,
            } => {
                return Self::build(
                    ObservationInputs::Plan {
                        plan,
                        current,
                        deleted,
                        reads: &DataSourceValues::empty(),
                    },
                    persisted,
                );
            }
            ObservationInputs::Plan {
                plan,
                current,
                deleted,
                reads,
            } => {
                let protect = |id: &ResourceId, state: &SnapshotState| {
                    check_identity(id, &state.id)?;
                    let previous = persisted.row(observation_origin_for_plan(plan, id));
                    Ok::<_, SerializationError>(
                        ProtectedObservation::from_authority(
                            state,
                            plan.observation_desired.get(id),
                            previous.map(|(attributes, _)| attributes),
                        )
                        .with_nested_write_only(previous.map(|(_, nested)| nested)),
                    )
                };
                let protect_deleted =
                    |key: &crate::effect::DeletedInstanceKey, state: &SnapshotState| {
                        check_identity(key.id(), &state.id)?;
                        let origin = observation_origin_for_plan(plan, key.id());
                        let previous = persisted.generation_attributes(
                            origin,
                            key.generation(),
                            key.identifier(),
                        );
                        let nested = if key.generation().is_current() {
                            persisted.row(origin).map(|(_, nested)| nested)
                        } else {
                            None
                        };
                        Ok::<_, SerializationError>(
                            ProtectedObservation::from_authority(
                                state,
                                plan.observation_desired.get(key.id()),
                                previous,
                            )
                            .with_nested_write_only(nested),
                        )
                    };
                #[allow(clippy::iter_over_hash_type)]
                // All supplied observations are validated independently of iteration order.
                for (key, state) in deleted {
                    check_identity(key.id(), &state.id)?;
                }
                #[allow(clippy::iter_over_hash_type)]
                // Keyed rebuild: each current observation is classified independently.
                for (id, current) in current {
                    if let Some(state) = current_snapshot(current) {
                        output.managed.insert(id.clone(), protect(id, state)?);
                    }
                }
                for effect in plan.effects() {
                    if let crate::effect::Effect::Update { from, to, .. } = effect {
                        output.updates.insert(to.id.clone(), protect(&to.id, from)?);
                    }
                    for key in effect.deleted_resource_attribute_keys() {
                        let state = deleted
                            .get(&key)
                            .cloned()
                            .or_else(|| match key.generation() {
                                crate::effect::EffectGeneration::Current => {
                                    current.get(key.id()).and_then(current_snapshot).cloned()
                                }
                                crate::effect::EffectGeneration::Deposed(_) => None,
                            });
                        if let Some(state) = state {
                            output
                                .deleted
                                .insert(key.clone(), protect_deleted(&key, &state)?);
                        }
                    }
                }
                // Recorded replacement history remains owned even when a generation
                // has no ordinary Delete detail row (including deferred replacements).
                for old in &plan.replacement_inputs.old_instances {
                    let state =
                        SnapshotState::existing(old.instance.id().clone(), old.attributes.clone());
                    let observation = protect_deleted(&old.instance, &state)?;
                    output
                        .old_instances
                        .insert(old.instance.clone(), observation);
                }
                for replacement in plan.replace_display_info() {
                    if let Some(crate::effect::Effect::Delete {
                        id,
                        generation,
                        identifier,
                        ..
                    }) = plan.effects().get(replacement.delete_idx)
                    {
                        let key = crate::effect::DeletedInstanceKey::new(
                            id.as_inner().clone(),
                            generation.clone(),
                            identifier.to_string(),
                        );
                        let state = SnapshotState::existing(
                            key.id().clone(),
                            replacement.previous_attributes.clone(),
                        );
                        let observation = protect_deleted(&key, &state)?;
                        output
                            .replacements
                            .insert(replacement.delete_idx, observation.clone());
                        output.deleted.entry(key).or_insert(observation);
                    }
                }
                for effect in plan.effects() {
                    if let crate::effect::Effect::DeferredReplace(payload) = effect {
                        for old in &payload.deletes {
                            let key = old.instance_key();
                            let state = SnapshotState::existing(
                                old.id.as_inner().clone(),
                                old.previous_attributes.clone(),
                            );
                            let observation = protect_deleted(&key, &state)?;
                            if key.generation().is_current() {
                                output
                                    .managed
                                    .entry(key.id().clone())
                                    .or_insert_with(|| observation.clone());
                            }
                            output.deferred.insert(key, observation);
                        }
                    }
                }
                for (source, value) in reads.iter() {
                    if let DataSourceValue::Read(state) = value {
                        output.reads.insert(
                            source.id.clone(),
                            ProtectedObservation::for_data_source(state, source)?,
                        );
                    }
                }
            }
            ObservationInputs::SavedApply {
                plan,
                history,
                planned,
                actual,
            } => {
                let protect = |state: &SnapshotState| {
                    let desired = plan.observation_desired.get(&state.id).ok_or_else(|| {
                        SerializationError::MissingObservationResource {
                            id: Box::new(state.id.clone()),
                        }
                    })?;
                    let previous = persisted.row(observation_origin_for_plan(plan, &state.id));
                    let previous_attributes = previous.map(|(attributes, _)| attributes);
                    let mut observation = ProtectedObservation::from_authority(
                        state,
                        Some(desired),
                        previous_attributes,
                    );
                    if let Some(previous) = history.get(&state.id).and_then(current_snapshot) {
                        check_identity(&state.id, &previous.id)?;
                        let previous = ProtectedObservation::from_authority(
                            previous,
                            Some(desired),
                            previous_attributes,
                        );
                        observation = observation.with_history(&previous)?;
                    }
                    Ok::<_, SerializationError>(observation)
                };
                output.comparisons = Some(ProtectedComparisons {
                    planned: protect(planned)?,
                    actual: protect(actual)?,
                });
            }
        }
        // Repeated locations share one lazy snapshot, including its fresh salts.
        // This never substitutes a recorded view for a different refreshed view.
        let mut snapshots: HashMap<ResourceId, Vec<&ProtectedObservation>> = HashMap::new();
        let comparisons = output
            .comparisons
            .iter_mut()
            .flat_map(|pair| [&mut pair.planned, &mut pair.actual]);
        #[allow(clippy::iter_over_hash_type)]
        // Keyed sharing: identical products have the same values and authority.
        for observation in output
            .managed
            .values_mut()
            .chain(output.updates.values_mut())
            .chain(output.deleted.values_mut())
            .chain(output.old_instances.values_mut())
            .chain(output.replacements.values_mut())
            .chain(output.deferred.values_mut())
            .chain(output.reads.values_mut())
            .chain(comparisons)
        {
            let previous = snapshots.entry(observation.id().clone()).or_default();
            if !previous
                .iter()
                .any(|previous| observation.share_snapshot_with(previous))
            {
                previous.push(observation);
            }
        }
        Ok(output)
    }
    /// Record the complete batch: output callers cannot select a historical subset.
    pub(crate) fn recorded_history(
        &self,
    ) -> Result<HashMap<ResourceId, SnapshotState>, SerializationError> {
        let mut history = HashMap::new();
        #[allow(clippy::iter_over_hash_type)]
        // Keyed snapshots are independent; their iteration order is not serialized.
        for (id, observation) in &self.managed {
            history.insert(id.clone(), observation.snapshot()?);
        }
        #[allow(clippy::iter_over_hash_type)]
        // Update sources not in the current map still retain their authority.
        for (id, observation) in &self.updates {
            if let std::collections::hash_map::Entry::Vacant(entry) = history.entry(id.clone()) {
                entry.insert(observation.snapshot()?);
            }
        }
        Ok(history)
    }
    pub(crate) fn check_plan<S, V: crate::resource::PlanValueKind>(
        &self,
        plan: &Plan<S, V>,
    ) -> Result<(), SerializationError> {
        if !std::sync::Arc::ptr_eq(&self.plan_identity, &plan.observation_identity) {
            return Err(SerializationError::ObservationPlanMismatch);
        }
        Ok(())
    }
    pub(crate) fn protect_plan_history<S: Clone>(
        &self,
        plan: &Plan<S>,
    ) -> Result<Plan<S>, SerializationError> {
        self.check_plan(plan)?;
        let mut protected = plan.clone();
        for effect in protected.effects_mut() {
            match effect {
                crate::effect::Effect::Update { from, to, .. } => {
                    from.attributes = self.update_from(&to.id)?.snapshot()?.attributes;
                }
                crate::effect::Effect::DeferredReplace(payload) => {
                    for old in payload.unadmitted_request_mut().deletes.iter_mut() {
                        let key = crate::effect::DeletedInstanceKey::new(
                            old.id.as_inner().clone(),
                            old.generation.clone(),
                            old.identifier.to_string(),
                        );
                        let observation = self.deferred.get(&key).ok_or_else(|| {
                            SerializationError::MissingObservationResource {
                                id: Box::new(old.id.as_inner().clone()),
                            }
                        })?;
                        old.previous_attributes = observation.snapshot()?.attributes;
                    }
                }
                _ => {}
            }
        }
        for old in &mut protected.replacement_inputs.old_instances {
            old.attributes = self
                .old_instances
                .get(&old.instance)
                .ok_or_else(|| SerializationError::MissingObservationResource {
                    id: Box::new(old.instance.id().clone()),
                })?
                .snapshot()?
                .attributes;
        }
        for metadata in &mut protected.replace_display {
            if let Some(crate::effect::Effect::Delete {
                id,
                identifier,
                generation,
                ..
            }) = plan.effects().get(metadata.delete_idx)
            {
                let key = crate::effect::DeletedInstanceKey::new(
                    id.as_inner().clone(),
                    generation.clone(),
                    identifier.to_string(),
                );
                metadata.previous_attributes = self
                    .replacements
                    .get(&metadata.delete_idx)
                    .ok_or_else(|| SerializationError::MissingObservationResource {
                        id: Box::new(key.id().clone()),
                    })?
                    .snapshot()?
                    .attributes;
            }
        }
        Ok(protected)
    }
    pub fn into_managed(self) -> HashMap<ResourceId, ProtectedObservation> {
        self.managed
    }
    pub fn managed(&self) -> &HashMap<ResourceId, ProtectedObservation> {
        &self.managed
    }
    pub fn managed_for(
        &self,
        id: &ResourceId,
    ) -> Result<&ProtectedObservation, SerializationError> {
        self.managed
            .get(id)
            .ok_or_else(|| SerializationError::MissingObservationResource {
                id: Box::new(id.clone()),
            })
    }
    pub fn update_from(
        &self,
        id: &ResourceId,
    ) -> Result<&ProtectedObservation, SerializationError> {
        self.updates
            .get(id)
            .ok_or_else(|| SerializationError::MissingObservationResource {
                id: Box::new(id.clone()),
            })
    }
    pub(crate) fn old_instance_history(
        &self,
    ) -> &HashMap<crate::effect::DeletedInstanceKey, ProtectedObservation> {
        &self.old_instances
    }
    pub fn deleted(&self) -> &HashMap<crate::effect::DeletedInstanceKey, ProtectedObservation> {
        &self.deleted
    }
    pub(crate) fn replacement_history(&self) -> &HashMap<usize, ProtectedObservation> {
        &self.replacements
    }
    pub(crate) fn deferred_history(
        &self,
    ) -> &HashMap<crate::effect::DeletedInstanceKey, ProtectedObservation> {
        &self.deferred
    }
    pub fn reads(&self) -> &HashMap<ResourceId, ProtectedObservation> {
        &self.reads
    }
    pub fn comparisons(&self) -> Result<&ProtectedComparisons, SerializationError> {
        self.comparisons
            .as_ref()
            .ok_or(SerializationError::MissingObservationComparisons)
    }
}

fn current_snapshot(current: &Current) -> Option<&SnapshotState> {
    match current {
        Current::Absent(state) => Some(state.state()),
        Current::Present(state) => Some(state.state()),
        Current::MissingIdentifier(state) => Some(state.state()),
        Current::CreateInFlight(_) | Current::CreateOutcomeUnknown(_) => None,
    }
}
fn check_identity(expected: &ResourceId, actual: &ResourceId) -> Result<(), SerializationError> {
    if expected != actual {
        return Err(SerializationError::ObservationIdentityMismatch {
            expected: Box::new(expected.clone()),
            actual: Box::new(actual.clone()),
        });
    }
    Ok(())
}

/// The recorded move is the only authority for selecting an origin state row.
pub(crate) fn observation_history_for_plan<'a, S>(
    plan: &Plan<S>,
    rows: Option<&'a dyn BindingState>,
    id: &ResourceId,
) -> Option<&'a crate::provider::PersistedAttributes> {
    rows.and_then(|rows| rows.observation_attributes(observation_origin_for_plan(plan, id)))
}

fn observation_origin_for_plan<'a, S, V: crate::resource::PlanValueKind>(
    plan: &'a Plan<S, V>,
    id: &'a ResourceId,
) -> &'a ResourceId {
    plan.effects()
        .iter()
        .find_map(|effect| match effect {
            crate::effect::Effect::Move { from, to, .. } if to.as_inner() == id => plan
                .move_authority(from.as_inner(), to.as_inner())
                .map(|_| from.as_inner()),
            _ => None,
        })
        .unwrap_or(id)
}

/// Authority cannot be minted by a default value or a struct literal.
///
/// ```compile_fail,E0599
/// use carina_core::value::ObservationState;
/// let _ = ObservationState::default();
/// ```
///
/// ```compile_fail,E0451
/// use carina_core::value::ObservationState;
/// fn forge(loaded: ObservationState) -> ObservationState {
///     ObservationState { ..loaded }
/// }
/// ```
///
/// Desired inputs belong to the plan; output callers cannot choose a slice.
///
/// ```compile_fail,E0559
/// use carina_core::{value::ObservationInputs, plan::Plan, data_source_value::DataSourceValues};
/// fn choose(plan: &Plan) {
///     let current = Default::default();
///     let reads = DataSourceValues::empty();
///     let _ = ObservationInputs::Plan { deleted: &std::collections::HashMap::new(),  plan, desired: &[], current: &current, reads: &reads };
/// }
/// ```
///
/// The builder is private: callers must obtain products through an authority.
///
/// ```compile_fail,E0624
/// use carina_core::{value::{ObservationInputs, ObservationState, ProtectedObservations}, plan::Plan};
/// fn bypass(plan: &Plan, state: &ObservationState) {
///     let current = Default::default();
///     let _ = ProtectedObservations::build(ObservationInputs::Display { deleted: &std::collections::HashMap::new(),  plan, current: &current }, state);
/// }
/// ```
#[cfg(doctest)]
const OBSERVATION_AUTHORITY_API: () = ();
