//! Differ - Compare desired state with current state to generate a Plan
//!
//! Compares the "desired state" declared in DSL with the "current state" fetched
//! from the Provider, and generates a list of required Effects (Plan).

mod comparison;
mod plan;

use std::collections::{HashMap, HashSet};
use std::ops::Index;

use crate::binding_index::ManagedBindingStability;
use crate::effect::{MoveAuthority, ReleasedListKey, ReleasedListKeys};
use crate::provider::{
    LiftedSavedAttrs, NestedWriteOnlyRecord, ProviderError, ProviderResult, ProviderRouter,
    StateTransformMap,
};
use crate::resource::{
    AbsoluteBindingAddress, CanonicalComparisonAttributes, Current, Directives, PlanInputCurrent,
    PlanInputDesired, PresentResourceState, ResolvedResource, Resource, ResourceId, State,
    UnidentifiedState, Value, into_plan_input_current_map,
};
use crate::schema::{ResourceSchema, SchemaRegistry, ValidatedResourceSchema};

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub use plan::create_plan_for_test;
pub use plan::{
    CascadeDependents, DeferredCreateTargets, DeferredForExpansionChoice, PendingCascadePlan,
    block_deletes_on_prior_consumer_updates, create_plan, create_plan_with_cascades,
};

struct PlanInputSavedAttributes {
    attributes: HashMap<ResourceId, CanonicalComparisonAttributes>,
}

impl PlanInputSavedAttributes {
    fn new(saved: &LiftedSavedAttrs, registry: &SchemaRegistry) -> Self {
        let attributes = saved
            .resource_ids()
            .filter_map(|id| {
                let values = saved.get(id)?.clone();
                let schema = registry
                    .get(
                        &id.provider,
                        &id.resource_type,
                        crate::schema::SchemaKind::Resource,
                    )
                    .map(|schema| schema.schema().as_schema());
                let attributes = match schema {
                    Some(schema) => CanonicalComparisonAttributes::canonicalize(values, schema),
                    None => CanonicalComparisonAttributes::from_unschematized(
                        crate::resource::UnschematizedComparisonAttributes::new(values),
                    ),
                };
                Some((id.clone(), attributes))
            })
            .collect();
        Self { attributes }
    }

    fn get(&self, id: &ResourceId) -> Option<&CanonicalComparisonAttributes> {
        self.attributes.get(id)
    }
}

/// Project the differ's managed-resource decision into the attribute
/// definitiveness required by reference/coalesce resolution.
///
/// This deliberately reuses `diff_present` and the same non-removable-removal
/// filter as plan generation. It does not compare authored values inside the
/// binding layer: callers first resolve the desired resources, then the differ
/// says whether the refreshed snapshot has no effect, an in-place update, or
/// an unavailable/replacement result.
pub fn managed_binding_stability(
    managed: &[ResolvedResource],
    current_states: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    provider: &ProviderRouter,
) -> ProviderResult<HashMap<ResourceId, ManagedBindingStability>> {
    let mut result = HashMap::new();
    let comparison_saved = PlanInputSavedAttributes::new(saved_attrs, registry);
    for resource in managed {
        let Some(current) = current_states.get(&resource.id) else {
            result.insert(resource.id.clone(), ManagedBindingStability::Unavailable);
            continue;
        };
        let Current::Present(present) = current.current() else {
            result.insert(resource.id.clone(), ManagedBindingStability::Unavailable);
            continue;
        };
        let current_attributes = current.comparison_attributes();
        let resolved_comparison = provider.comparison_for(registry, &resource.id)?;
        let owner_schema = resolved_comparison.owner_schema();
        let context = resolved_comparison.context();
        let schema = owner_schema.schema();
        let desired = PlanInputDesired::with_resource_schema(resource, Some(schema.as_schema()));
        let changed_attributes = match diff_present_with_releases(
            &desired,
            present,
            ExistingComparisonInputs {
                current_attributes,
                saved: comparison_saved.get(&resource.id),
                nested_write_only: saved_attrs.nested_write_only(&resource.id),
                prev_explicit: prev_explicit.get(&resource.id),
                schema: Some(schema.as_schema()),
                context,
            },
            schema,
        )
        .diff
        {
            PresentDiff::NoChange { .. } => {
                result.insert(
                    resource.id.clone(),
                    ManagedBindingStability::NoPlannedEffect,
                );
                continue;
            }
            PresentDiff::Update {
                changed_attributes, ..
            } => changed_attributes,
        };
        let changed_attributes = plan::filter_non_removable_removals(
            &resource.id.provider,
            &resource.id.resource_type,
            resource.as_resource(),
            changed_attributes,
            registry,
        );
        if changed_attributes.is_empty() {
            result.insert(
                resource.id.clone(),
                ManagedBindingStability::NoPlannedEffect,
            );
            continue;
        }
        let create_only = schema.as_schema().create_only_attributes();
        if changed_attributes
            .iter()
            .any(|attribute| create_only.contains(&attribute.as_str()))
        {
            result.insert(resource.id.clone(), ManagedBindingStability::Unavailable);
            continue;
        }
        let changed_attributes = changed_attributes.into_iter().collect();
        let refreshable_attributes = schema
            .as_schema()
            .read_only_attributes()
            .into_iter()
            .map(str::to_string)
            .collect();
        result.insert(
            resource.id.clone(),
            ManagedBindingStability::Updating {
                changed_attributes,
                refreshable_attributes,
            },
        );
    }
    Ok(result)
}

/// Provider-observed state paired with all host-owned planning metadata for a
/// resource that is no longer present in the desired configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    current: Current,
    binding: Option<AbsoluteBindingAddress>,
    directives: Directives,
}

impl Orphan {
    pub fn absent(
        state: crate::resource::AbsentState,
        binding: Option<AbsoluteBindingAddress>,
        directives: Directives,
    ) -> Self {
        Self {
            current: Current::Absent(state),
            binding,
            directives,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_state(
        state: State,
        binding: Option<AbsoluteBindingAddress>,
        directives: Directives,
    ) -> Self {
        Self {
            current: Current::from_state(state),
            binding,
            directives,
        }
    }

    pub fn present(
        present: PresentResourceState,
        binding: Option<AbsoluteBindingAddress>,
        directives: Directives,
    ) -> Self {
        Self {
            current: Current::Present(present),
            binding,
            directives,
        }
    }

    pub fn missing_identifier(
        state: UnidentifiedState,
        binding: Option<AbsoluteBindingAddress>,
        directives: Directives,
    ) -> Self {
        Self {
            current: Current::MissingIdentifier(state),
            binding,
            directives,
        }
    }

    pub fn create_in_flight(
        pending: crate::provider::InFlightCreate,
        binding: Option<AbsoluteBindingAddress>,
        directives: Directives,
    ) -> Self {
        Self {
            current: Current::CreateInFlight(pending),
            binding,
            directives,
        }
    }

    pub fn create_outcome_unknown(
        unknown: crate::provider::UnknownCreateOutcome,
        binding: Option<AbsoluteBindingAddress>,
        directives: Directives,
    ) -> Self {
        Self {
            current: Current::CreateOutcomeUnknown(unknown),
            binding,
            directives,
        }
    }

    pub fn current(&self) -> &Current {
        &self.current
    }

    pub(crate) fn current_mut(&mut self) -> &mut Current {
        &mut self.current
    }

    pub fn binding(&self) -> Option<&str> {
        self.binding.as_ref().map(AbsoluteBindingAddress::as_str)
    }

    pub fn binding_address(&self) -> Option<&AbsoluteBindingAddress> {
        self.binding.as_ref()
    }

    pub fn directives(&self) -> &Directives {
        &self.directives
    }

    /// Project this persisted-only resource into the minimal desired-resource
    /// shape used by binding resolution. Provider state remains authoritative
    /// for its values; the projection exists only so a data-source input can
    /// resolve a binding that is no longer present in the configuration.
    pub fn binding_resource(&self) -> Resource {
        let (attributes, dependency_bindings) = match &self.current {
            Current::Absent(state) => (
                &state.state().attributes,
                &state.state().dependency_bindings,
            ),
            Current::Present(present) => (
                &present.state().attributes,
                &present.state().dependency_bindings,
            ),
            Current::CreateInFlight(_pending) => {
                return Resource {
                    id: self.current.id().clone(),
                    attributes: Default::default(),
                    directives: self.directives.clone(),
                    resolved_ignore_changes: Default::default(),
                    prefixes: Default::default(),
                    binding: self.binding.clone(),
                    dependency_bindings: Default::default(),
                    module_source: None,
                    default_tag_keys: Default::default(),
                    loop_provenance: Default::default(),
                    quoted_string_attrs: Default::default(),
                };
            }
            Current::CreateOutcomeUnknown(_unknown) => {
                return Resource {
                    id: self.current.id().clone(),
                    attributes: Default::default(),
                    directives: self.directives.clone(),
                    resolved_ignore_changes: Default::default(),
                    prefixes: Default::default(),
                    binding: self.binding.clone(),
                    dependency_bindings: Default::default(),
                    module_source: None,
                    default_tag_keys: Default::default(),
                    loop_provenance: Default::default(),
                    quoted_string_attrs: Default::default(),
                };
            }
            Current::MissingIdentifier(state) => (
                &state.state().attributes,
                &state.state().dependency_bindings,
            ),
        };
        Resource {
            id: self.current.id().clone(),
            attributes: attributes
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            directives: self.directives.clone(),
            resolved_ignore_changes: Default::default(),
            prefixes: Default::default(),
            binding: self.binding.clone(),
            dependency_bindings: dependency_bindings.clone(),
            module_source: None,
            default_tag_keys: Default::default(),
            loop_provenance: Default::default(),
            quoted_string_attrs: Default::default(),
        }
    }

    pub fn into_current(self) -> Current {
        self.current
    }

    fn rebound_for_move(mut self, authority: &MoveAuthority<'_>) -> Self {
        self.current.rebind_for_move(authority);
        self
    }

    /// Replace only provider-observed state while retaining the dependency
    /// bindings carried by the prior state and the orphan's typed binding and
    /// directive metadata.
    pub fn with_refreshed_state(mut self, mut fresh: State) -> Self {
        let prior = match &self.current {
            Current::Absent(state) => state.state(),
            Current::Present(present) => present.state(),
            Current::CreateInFlight(_pending) => return self,
            Current::CreateOutcomeUnknown(_unknown) => return self,
            Current::MissingIdentifier(state) => state.state(),
        };
        fresh.id = prior.id.clone();
        fresh.dependency_bindings = prior.dependency_bindings.clone();
        self.current = Current::from_state(fresh);
        self
    }
}

/// The single owner of resources that exist in persisted/provider state but
/// are absent from the desired configuration.
///
/// The map key is derived from the orphan's provider state on insertion, so a
/// mismatched key and `State::id` cannot be represented through this API.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Orphans {
    by_id: HashMap<ResourceId, Orphan>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot move current state for {actual}; expected source route {expected}")]
pub struct CurrentMoveRoutingMismatch {
    actual: Box<ResourceId>,
    expected: Box<ResourceId>,
}

/// Rebind current state through the explicit move seam while checking that
/// the caller named its actual source route.
///
/// A caller-supplied pair of ids is not move authority.
///
/// ```compile_fail
/// use carina_core::differ::rebind_current_for_move;
/// use carina_core::resource::{Current, ResourceId};
///
/// fn bypass(current: Current, from: &ResourceId, to: ResourceId) {
///     let _ = rebind_current_for_move(current, from, to);
/// }
/// ```
pub fn rebind_current_for_move(
    mut current: Current,
    authority: &MoveAuthority<'_>,
) -> Result<Current, CurrentMoveRoutingMismatch> {
    if current.id() != authority.from() {
        return Err(CurrentMoveRoutingMismatch {
            actual: Box::new(current.id().clone()),
            expected: Box::new(authority.from().clone()),
        });
    }
    current.rebind_for_move(authority);
    Ok(current)
}

/// Restore a saved post-move snapshot to the source side of the exact move
/// edge that will be replayed during current-configuration verification.
pub fn restore_current_move_source(
    mut current: Current,
    authority: &MoveAuthority<'_>,
) -> Result<Current, CurrentMoveRoutingMismatch> {
    if current.id() != authority.to() {
        return Err(CurrentMoveRoutingMismatch {
            actual: Box::new(current.id().clone()),
            expected: Box::new(authority.to().clone()),
        });
    }
    current.restore_move_source(authority);
    Ok(current)
}

/// Atomically move one keyed current-state entry through the checked move
/// seam. A missing source is not an error and leaves the map unchanged.
pub fn rekey_current_for_move(
    current: &mut HashMap<ResourceId, Current>,
    authority: &MoveAuthority<'_>,
) -> Result<bool, CurrentMoveRoutingMismatch> {
    let Some(mut value) = current.remove(authority.from()) else {
        return Ok(false);
    };
    if value.id() != authority.from() {
        let actual = value.id().clone();
        current.insert(authority.from().clone(), value);
        return Err(CurrentMoveRoutingMismatch {
            actual: Box::new(actual),
            expected: Box::new(authority.from().clone()),
        });
    }
    value.rebind_for_move(authority);
    current.insert(authority.to().clone(), value);
    Ok(true)
}

impl Orphans {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, orphan: Orphan) {
        let id = match &orphan.current {
            Current::Absent(state) => state.state().id.clone(),
            Current::Present(present) => present.state().id.clone(),
            Current::CreateInFlight(pending) => pending.id().clone(),
            Current::CreateOutcomeUnknown(unknown) => unknown.id().clone(),
            Current::MissingIdentifier(state) => state.state().id.clone(),
        };
        self.by_id.insert(id, orphan);
    }

    pub fn remove(&mut self, id: &ResourceId) -> Option<Orphan> {
        self.by_id.remove(id)
    }

    pub fn rekey(&mut self, authority: &MoveAuthority<'_>) -> bool {
        let Some(orphan) = self.remove(authority.from()) else {
            return false;
        };
        self.insert(orphan.rebound_for_move(authority));
        true
    }

    /// Consume an orphan under the move's source route and return its current
    /// instance already rebound to the destination route.
    pub fn take_rebound(&mut self, authority: &MoveAuthority<'_>) -> Option<Current> {
        self.remove(authority.from())
            .map(|orphan| orphan.rebound_for_move(authority).into_current())
    }

    pub fn get(&self, id: &ResourceId) -> Option<&Orphan> {
        self.by_id.get(id)
    }

    pub(crate) fn get_mut(&mut self, id: &ResourceId) -> Option<&mut Orphan> {
        self.by_id.get_mut(id)
    }

    pub fn contains_key(&self, id: &ResourceId) -> bool {
        self.by_id.contains_key(id)
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn keys(&self) -> impl Iterator<Item = &ResourceId> {
        self.by_id.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ResourceId, &Orphan)> {
        self.by_id.iter()
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (&ResourceId, &mut Orphan)> {
        self.by_id.iter_mut()
    }
}

impl FromIterator<Orphan> for Orphans {
    fn from_iter<T: IntoIterator<Item = Orphan>>(iter: T) -> Self {
        let mut orphans = Self::new();
        for orphan in iter {
            orphans.insert(orphan);
        }
        orphans
    }
}

impl<const N: usize> From<[Orphan; N]> for Orphans {
    fn from(orphans: [Orphan; N]) -> Self {
        orphans.into_iter().collect()
    }
}

impl IntoIterator for Orphans {
    type Item = (ResourceId, Orphan);
    type IntoIter = std::collections::hash_map::IntoIter<ResourceId, Orphan>;

    fn into_iter(self) -> Self::IntoIter {
        self.by_id.into_iter()
    }
}

impl<'a> IntoIterator for &'a Orphans {
    type Item = (&'a ResourceId, &'a Orphan);
    type IntoIter = std::collections::hash_map::Iter<'a, ResourceId, Orphan>;

    fn into_iter(self) -> Self::IntoIter {
        self.by_id.iter()
    }
}

impl Index<&ResourceId> for Orphans {
    type Output = Orphan;

    fn index(&self, index: &ResourceId) -> &Self::Output {
        &self.by_id[index]
    }
}

fn extend_disjoint_states<T>(
    current_states: &mut HashMap<ResourceId, T>,
    additional_states: HashMap<ResourceId, T>,
) {
    assert!(
        additional_states
            .keys()
            .all(|id| !current_states.contains_key(id)),
        "prior state cannot be owned by both managed_current and orphans"
    );
    current_states.extend(additional_states);
}

/// The complete prior-state input to planning.
///
/// Planning-ready managed current states and typed orphans remain separately
/// owned before the differ, but callers must pass both views to
/// [`create_plan`]. The differ materializes same-id orphans as current state
/// for managed resources and emits orphan deletes only for ids that remain
/// unmanaged.
#[derive(Clone, Copy)]
pub struct PlanPriorStates<'a> {
    current_states: &'a HashMap<ResourceId, PlanInputCurrent>,
    orphans: &'a Orphans,
}

impl<'a> PlanPriorStates<'a> {
    pub fn new(
        current_states: &'a HashMap<ResourceId, PlanInputCurrent>,
        orphans: &'a Orphans,
    ) -> Self {
        Self {
            current_states,
            orphans,
        }
    }

    fn materialize_current_states(
        self,
        managed: &[ResolvedResource],
        registry: &crate::schema::SchemaRegistry,
    ) -> HashMap<ResourceId, PlanInputCurrent> {
        let resources: Vec<Resource> = managed
            .iter()
            .map(|resource| resource.as_resource().clone())
            .collect();
        let mut current_states = self.current_states.clone();
        let orphan_states = into_plan_input_current_map(
            managed_orphan_provider_states(self.orphans, &resources),
            registry,
            &resources,
        );
        extend_disjoint_states(&mut current_states, orphan_states);
        current_states
    }

    fn orphans(self) -> &'a Orphans {
        self.orphans
    }
}

fn managed_orphan_provider_states(
    orphans: &Orphans,
    managed: &[Resource],
) -> HashMap<ResourceId, Current> {
    managed
        .iter()
        .filter_map(|resource| {
            orphans
                .get(&resource.id)
                .map(|orphan| (resource.id.clone(), orphan.current.clone()))
        })
        .collect()
}

/// Derive the provider-state view for the managed resource set, promoting
/// same-id orphans without changing either prior-state owner.
pub fn provider_states_for(
    current_states: &HashMap<ResourceId, Current>,
    orphans: &Orphans,
    managed: &[Resource],
) -> HashMap<ResourceId, Current> {
    let mut current = current_states.clone();
    extend_disjoint_states(
        &mut current,
        managed_orphan_provider_states(orphans, managed),
    );
    current
}

fn prior_managed_transform_states(
    current_states: &HashMap<ResourceId, Current>,
    orphans: &Orphans,
) -> StateTransformMap {
    let mut managed_states: HashMap<ResourceId, State> = current_states
        .iter()
        .filter_map(|(id, current)| {
            let state = match current {
                Current::Absent(state) => state.state(),
                Current::Present(present) => present.state(),
                Current::CreateInFlight(_pending) => return None,
                Current::CreateOutcomeUnknown(_unknown) => return None,
                Current::MissingIdentifier(state) => state.state(),
            };
            Some((id.clone(), state.clone()))
        })
        .collect();
    let orphan_states = orphans
        .iter()
        .filter_map(|(id, orphan)| {
            let state = match &orphan.current {
                Current::Absent(state) => state.state(),
                Current::Present(present) => present.state(),
                Current::CreateInFlight(_pending) => return None,
                Current::CreateOutcomeUnknown(_unknown) => return None,
                Current::MissingIdentifier(state) => state.state(),
            };
            Some((id.clone(), state.clone()))
        })
        .collect();
    extend_disjoint_states(&mut managed_states, orphan_states);
    StateTransformMap::from_states(managed_states)
}

fn install_prior_managed_transform_states(
    current_states: &mut HashMap<ResourceId, Current>,
    orphans: &mut Orphans,
    transformed: StateTransformMap,
) {
    let mut managed_states = transformed.into_states();
    for (id, orphan) in orphans.iter_mut() {
        if let Some(state) = managed_states.remove(id)
            && let Some(target) = orphan.current.state_bearing_mut()
        {
            target.apply_transformed_state(state);
        }
    }
    for (id, current) in current_states {
        if let Some(state) = managed_states.remove(id)
            && let Some(target) = current.state_bearing_mut()
        {
            target.apply_transformed_state(state);
        }
    }
}

/// Run a provider-state transform over managed and orphan state only.
///
/// This is the managed-only counterpart of [`transform_prior_provider_states`]
/// for call sites that have not read any data sources yet. It removes the need
/// to manufacture an empty data-source map merely to cross the hydration
/// boundary.
pub async fn transform_prior_managed_provider_states(
    current_states: &mut HashMap<ResourceId, Current>,
    orphans: &mut Orphans,
    transform: impl AsyncFnOnce(&mut StateTransformMap) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    let mut transformed = prior_managed_transform_states(current_states, orphans);
    let expected_ids = transformed
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<HashSet<_>>();
    transform(&mut transformed).await?;
    validate_state_transform_routes("managed/orphan", &expected_ids, &transformed)?;
    install_prior_managed_transform_states(current_states, orphans, transformed);
    Ok(())
}

fn validate_state_transform_routes(
    label: &str,
    expected: &HashSet<ResourceId>,
    transformed: &StateTransformMap,
) -> Result<(), ProviderError> {
    let actual = transformed
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<HashSet<_>>();
    if actual == *expected {
        return Ok(());
    }
    let mut missing = expected
        .difference(&actual)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut unexpected = actual
        .difference(expected)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    missing.sort();
    unexpected.sort();
    Err(ProviderError::internal(format!(
        "provider {label} state transform changed its address set (missing: [{}]; unexpected: [{}])",
        missing.join(", "),
        unexpected.join(", ")
    )))
}

/// Run an asynchronous provider-state transform over managed/orphan state and
/// data-source state while retaining their distinct key spaces and owners.
///
/// Managed resources and orphans share one call because a desired managed
/// resource can adopt a same-id orphan. Data sources use a second call because
/// their `ResourceId` namespace can legitimately overlap the managed/orphan
/// namespace during a managed-to-data-source configuration change. Each call
/// must preserve its own key set; transformed values are installed only after
/// both calls succeed.
pub async fn transform_prior_provider_states(
    current_states: &mut HashMap<ResourceId, Current>,
    data_source_states: &mut HashMap<ResourceId, State>,
    orphans: &mut Orphans,
    mut transform_managed_orphans: impl AsyncFnMut(&mut StateTransformMap) -> Result<(), ProviderError>,
    mut transform_data_sources: impl AsyncFnMut(&mut StateTransformMap) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    let mut managed_states = prior_managed_transform_states(current_states, orphans);
    let mut transformed_data_sources = StateTransformMap::from_states(data_source_states.clone());
    let expected_managed_ids = managed_states
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<HashSet<_>>();
    let expected_data_source_ids = transformed_data_sources
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<HashSet<_>>();

    transform_managed_orphans(&mut managed_states).await?;
    validate_state_transform_routes("managed/orphan", &expected_managed_ids, &managed_states)?;
    if !transformed_data_sources.is_empty() {
        transform_data_sources(&mut transformed_data_sources).await?;
        validate_state_transform_routes(
            "data-source",
            &expected_data_source_ids,
            &transformed_data_sources,
        )?;
    }

    install_prior_managed_transform_states(current_states, orphans, managed_states);
    *data_source_states = transformed_data_sources.into_states();
    Ok(())
}

/// Lift enum leaves for every managed provider state, regardless of which
/// prior-state owner currently carries it.
pub fn lift_prior_state_enum_leaves(
    current_states: &mut HashMap<ResourceId, Current>,
    orphans: &mut Orphans,
    resources: &[Resource],
    registry: &crate::schema::SchemaRegistry,
) {
    for resource in resources {
        let Some(schema) = registry.get_for(resource) else {
            continue;
        };
        let schema = schema.as_schema();
        let attributes = current_states
            .get_mut(&resource.id)
            .and_then(Current::attributes_mut)
            .or_else(|| {
                orphans
                    .get_mut(&resource.id)
                    .and_then(|orphan| orphan.current_mut().attributes_mut())
            });
        if let Some(attributes) = attributes {
            crate::utils::lift_state_enum_leaves(attributes, schema);
        }
    }
}

/// Consume orphan ownership into the unified prior-state view required by
/// display and saved-plan consumers after the differ has finished.
pub fn into_all_prior_states(
    orphans: Orphans,
    mut current_states: HashMap<ResourceId, Current>,
) -> HashMap<ResourceId, Current> {
    let orphan_states = orphans
        .into_iter()
        .map(|(id, orphan)| (id, orphan.into_current()))
        .collect();
    extend_disjoint_states(&mut current_states, orphan_states);
    current_states
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn create_plan_for_tests(
    managed: &[Resource],
    data_sources: &[crate::resource::DataSource],
    provider: &dyn crate::provider::Provider,
    current_states: &HashMap<ResourceId, crate::resource::PlanInputCurrent>,
    directives_map: &HashMap<ResourceId, crate::resource::Directives>,
    registry: &crate::schema::SchemaRegistry,
    saved_attrs: &crate::provider::LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    orphans: &Orphans,
    wait_bindings: &[crate::parser::WaitBinding<crate::resource::AbsoluteBindingAddress>],
) -> crate::plan::Plan {
    let managed =
        ResolvedResource::collect_resolved_with_schemas(managed.iter().cloned(), registry)
            .expect("differ test resources must have resolved identities and valid ignore paths");
    plan::create_plan_for_test(
        &managed,
        data_sources,
        provider,
        PlanPriorStates::new(current_states, orphans),
        directives_map,
        registry,
        saved_attrs,
        prev_explicit,
        wait_bindings,
    )
    .expect("differ test provider hints should succeed")
}

// Imports used by test submodules (accessible via `use super::*;`)
#[cfg(test)]
use crate::effect::Effect;
#[cfg(test)]
use crate::plan::Plan;
#[cfg(test)]
use crate::schema::AttributeType;
pub use comparison::ComparisonContext;

#[cfg(any(test, feature = "test-utils"))]
pub fn exact_comparison_context_for_tests() -> ComparisonContext<'static> {
    use std::sync::LazyLock;

    static SCOPE: crate::provider::ReferenceScope = crate::provider::ReferenceScope::Unavailable;
    static FORMS: LazyLock<crate::schema::ReferenceForms> =
        LazyLock::new(crate::schema::ReferenceForms::new);
    ComparisonContext::for_test(&SCOPE, &FORMS)
}

#[cfg(test)]
fn empty_lifted_saved_attrs() -> crate::provider::LiftedSavedAttrs {
    crate::provider::RawSavedAttrs::default().lift(&SchemaRegistry::new())
}

// Re-export comparison primitives for consumers that must agree with
// `find_changed_attributes`: detail rows render with the same equality,
// and executor patch construction uses the same key/value gate.
pub(crate) use comparison::{
    AttrComparison, LeafProvenance, TypedAttr, key_should_enter_patch,
    key_should_enter_patch_with_ignores, secret_grafted_comparison_view, type_aware_equal,
};

/// Returns true when `binding` is one direct child emitted by the deferred-for
/// template. List children use a numeric index (`items[0]`); map children use
/// the canonical address spelling (`items.key` or `items['key with space']`).
pub fn binding_matches_deferred_template<B: crate::resource::BindingAddress>(
    binding: &B,
    target: &crate::parser::DeferredForTarget<B>,
) -> bool {
    let Some(template_binding) = target.bound_binding_address() else {
        return target.matches_legacy_binding(binding.as_str());
    };
    let binding = binding.as_str();
    let template_binding_name = template_binding.as_str();
    let Some(suffix) = binding.strip_prefix(template_binding_name) else {
        return false;
    };
    if let Some(key) = suffix.strip_prefix('.') {
        return crate::utils::is_identifier_safe(key);
    }
    let Some(inner) = suffix.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return false;
    };
    (!inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()))
        || inner
            .strip_prefix('\'')
            .and_then(|key| key.strip_suffix('\''))
            .is_some()
}

/// Result of a diff operation
#[derive(Debug, Clone, PartialEq)]
pub enum Diff {
    /// Resource does not exist -> needs creation
    Create(Resource),
    /// Resource exists with differences -> needs update
    Update {
        id: ResourceId,
        from: Box<State>,
        to: Resource,
        changed_attributes: Vec<String>,
    },
    /// Resource exists with no differences -> no action needed
    NoChange(ResourceId),
    /// Resource exists but not in desired state -> needs deletion
    Delete(ResourceId),
}

impl Diff {
    /// Returns whether this Diff involves a change
    pub fn is_change(&self) -> bool {
        !matches!(self, Diff::NoChange(_))
    }
}

enum PresentDiff {
    Update {
        id: ResourceId,
        from: Box<State>,
        to: Box<Resource>,
        changed_attributes: Vec<String>,
        ignored_readable_differences: usize,
    },
    NoChange {
        ignored_readable_differences: usize,
    },
}

#[derive(Debug)]
pub(super) struct PlannedListKeyRelease {
    pub(super) attribute: String,
    pub(super) key_field: crate::schema::FieldName,
    pub(super) keys: ReleasedListKeys,
}

struct PresentDiffResult {
    diff: PresentDiff,
    releases: Vec<PlannedListKeyRelease>,
}

fn concrete_list(value: &Value) -> Option<&[Value]> {
    let Value::Concrete(crate::resource::ConcreteValue::List(items)) = value else {
        return None;
    };
    Some(items)
}

fn keyed_element_key<'a>(element: &'a Value, key_field: &str) -> Option<&'a str> {
    let Value::Concrete(crate::resource::ConcreteValue::Map(fields)) = element else {
        return None;
    };
    let Value::Concrete(crate::resource::ConcreteValue::String(key)) = fields.get(key_field)?
    else {
        return None;
    };
    Some(key.expose_plaintext())
}

fn retained_key_value(element: &Value, key_field: &str) -> Value {
    let Value::Concrete(crate::resource::ConcreteValue::Map(fields)) = element else {
        return element.clone();
    };
    let mut retained = fields.clone();
    retained.shift_remove(key_field);
    if retained.len() == 1 {
        retained.into_values().next().expect("length checked")
    } else {
        Value::Concrete(crate::resource::ConcreteValue::Map(retained))
    }
}

fn unique_elements_by_key<'a>(
    value: &'a Value,
    key_field: &str,
) -> Option<std::collections::BTreeMap<&'a str, &'a Value>> {
    let mut by_key = std::collections::BTreeMap::new();
    for element in concrete_list(value)? {
        let key = keyed_element_key(element, key_field)?;
        if by_key.insert(key, element).is_some() {
            return None;
        }
    }
    Some(by_key)
}

fn authored_saved_keys(
    saved_value: &Value,
    authoring: &crate::explicit::ExplicitFields,
    key_field: &str,
) -> std::collections::BTreeSet<String> {
    let Some(elements) = concrete_list(saved_value) else {
        return std::collections::BTreeSet::new();
    };
    match authoring {
        crate::explicit::ExplicitFields::Unrecorded => std::collections::BTreeSet::new(),
        crate::explicit::ExplicitFields::ListElements { elements: records }
            if records.len() != elements.len() =>
        {
            std::collections::BTreeSet::new()
        }
        crate::explicit::ExplicitFields::ListElements { elements: records } => records
            .iter()
            .zip(elements)
            .filter(|(record, _)| key_field_was_authored(record, key_field))
            .filter_map(|(_, element)| keyed_element_key(element, key_field).map(str::to_owned))
            .collect(),
        crate::explicit::ExplicitFields::List { element }
            if !key_field_was_authored(element, key_field) =>
        {
            std::collections::BTreeSet::new()
        }
        crate::explicit::ExplicitFields::Struct { .. }
            if !key_field_was_authored(authoring, key_field) =>
        {
            std::collections::BTreeSet::new()
        }
        crate::explicit::ExplicitFields::Leaf
        | crate::explicit::ExplicitFields::Struct { .. }
        | crate::explicit::ExplicitFields::List { .. }
        | crate::explicit::ExplicitFields::WholeValue { .. } => elements
            .iter()
            .filter_map(|element| keyed_element_key(element, key_field).map(str::to_owned))
            .collect(),
    }
}

fn key_field_was_authored(record: &crate::explicit::ExplicitFields, key_field: &str) -> bool {
    match record {
        crate::explicit::ExplicitFields::Unrecorded => false,
        crate::explicit::ExplicitFields::Struct { children } => children
            .get(key_field)
            .is_some_and(|field| !matches!(field, crate::explicit::ExplicitFields::Unrecorded)),
        crate::explicit::ExplicitFields::List { element } => {
            key_field_was_authored(element, key_field)
        }
        // Leaf is the legacy whole-value record. WholeValue deliberately
        // retains whole-list authoring when identity could not be transferred.
        crate::explicit::ExplicitFields::Leaf
        | crate::explicit::ExplicitFields::WholeValue { .. } => true,
        // A nested list cannot be the scalar string key proven by schema
        // validation. Treat malformed legacy metadata as no evidence rather
        // than releasing a key on a guess.
        crate::explicit::ExplicitFields::ListElements { .. } => false,
    }
}

fn find_keyed_partial_update_releases(
    desired: &CanonicalComparisonAttributes,
    current: &CanonicalComparisonAttributes,
    saved: Option<&CanonicalComparisonAttributes>,
    prev_explicit: Option<&crate::explicit::ExplicitFields>,
    schema: &ValidatedResourceSchema,
) -> Vec<PlannedListKeyRelease> {
    let (Some(saved), Some(crate::explicit::ExplicitFields::Struct { children })) =
        (saved, prev_explicit)
    else {
        return Vec::new();
    };

    schema
        .effective_attributes()
        .filter_map(|attribute| {
            let semantics = attribute.keyed_partial_update()?;
            let key_field = semantics.key_field().as_str();
            let name = attribute.name();
            let saved_value = saved.get(name)?;
            let authoring = children.get(name)?;
            let authored = authored_saved_keys(saved_value, authoring, key_field);
            if authored.is_empty() {
                return None;
            }

            let desired_by_key = match desired.get(name) {
                Some(value) => unique_elements_by_key(value, key_field)?,
                None => std::collections::BTreeMap::new(),
            };
            let current_by_key = current
                .get(name)
                .and_then(|value| unique_elements_by_key(value, key_field))?;

            let keys = authored
                .into_iter()
                .filter(|key| !desired_by_key.contains_key(key.as_str()))
                .filter_map(|key| {
                    current_by_key.get(key.as_str()).map(|element| {
                        ReleasedListKey::new(key, retained_key_value(element, key_field))
                    })
                })
                .collect::<Vec<_>>();
            ReleasedListKeys::try_new(keys)
                .ok()
                .map(|keys| PlannedListKeyRelease {
                    attribute: name.to_string(),
                    key_field: semantics.key_field().clone(),
                    keys,
                })
        })
        .collect()
}

fn remove_keys_from_list(value: &mut Value, key_field: &str, released: &HashSet<&str>) {
    let Value::Concrete(crate::resource::ConcreteValue::List(elements)) = value else {
        return;
    };
    elements.retain(|element| {
        keyed_element_key(element, key_field).is_none_or(|key| !released.contains(key))
    });
}

fn filter_explicit_list_elements(
    explicit: &mut crate::explicit::ExplicitFields,
    saved_value: &Value,
    key_field: &str,
    released: &HashSet<&str>,
) {
    let crate::explicit::ExplicitFields::ListElements { elements: records } = explicit else {
        return;
    };
    let Some(saved_elements) = concrete_list(saved_value) else {
        return;
    };
    if records.len() != saved_elements.len() {
        return;
    }
    let mut index = 0usize;
    records.retain(|_| {
        let keep = keyed_element_key(&saved_elements[index], key_field)
            .is_none_or(|key| !released.contains(key));
        index += 1;
        keep
    });
}

#[derive(Clone, Copy)]
struct ExistingComparisonInputs<'a, 'context> {
    current_attributes: &'a CanonicalComparisonAttributes,
    saved: Option<&'a CanonicalComparisonAttributes>,
    nested_write_only: &'a NestedWriteOnlyRecord,
    prev_explicit: Option<&'a crate::explicit::ExplicitFields>,
    schema: Option<&'a ResourceSchema>,
    context: &'a ComparisonContext<'context>,
}

fn diff_present_with_releases(
    desired: &PlanInputDesired<'_>,
    current: &PresentResourceState,
    inputs: ExistingComparisonInputs<'_, '_>,
    schema: &ValidatedResourceSchema,
) -> PresentDiffResult {
    let releases = find_keyed_partial_update_releases(
        desired.comparison_attributes(),
        inputs.current_attributes,
        inputs.saved,
        inputs.prev_explicit,
        schema,
    );
    if releases.is_empty() {
        return PresentDiffResult {
            diff: diff_present(desired, current, inputs),
            releases,
        };
    }

    let mut filtered_current = inputs.current_attributes.clone();
    let mut filtered_saved = inputs.saved.cloned();
    let mut filtered_explicit = inputs.prev_explicit.cloned();
    for release in &releases {
        let attribute = schema
            .effective_attribute(&release.attribute)
            .and_then(|attribute| attribute.keyed_partial_update())
            .expect("release detection only returns validated keyed attributes");
        let key_field = attribute.key_field().as_str();
        let released = release
            .keys
            .iter()
            .map(ReleasedListKey::key)
            .collect::<HashSet<_>>();
        if let Some(value) = filtered_current.get_mut(&release.attribute) {
            remove_keys_from_list(value, key_field, &released);
        }
        if let (Some(saved), Some(crate::explicit::ExplicitFields::Struct { children })) =
            (filtered_saved.as_mut(), filtered_explicit.as_mut())
            && let (Some(saved_value), Some(explicit)) = (
                saved.get(&release.attribute),
                children.get_mut(&release.attribute),
            )
        {
            filter_explicit_list_elements(explicit, saved_value, key_field, &released);
        }
        if let Some(value) = filtered_saved
            .as_mut()
            .and_then(|saved| saved.get_mut(&release.attribute))
        {
            remove_keys_from_list(value, key_field, &released);
        }
    }

    let mut diff = diff_existing_state(
        desired,
        current.state(),
        ExistingComparisonInputs {
            current_attributes: &filtered_current,
            saved: filtered_saved.as_ref(),
            prev_explicit: filtered_explicit.as_ref(),
            ..inputs
        },
    );
    if let PresentDiff::Update { from, .. } = &mut diff {
        // Filtering exists only to answer whether omission itself is a
        // provider change. Provider requests still need the complete refreshed
        // `from` state for read-modify-write implementations.
        **from = current.state().clone();
    }
    PresentDiffResult { diff, releases }
}

fn diff_present(
    desired: &PlanInputDesired<'_>,
    current: &PresentResourceState,
    inputs: ExistingComparisonInputs<'_, '_>,
) -> PresentDiff {
    diff_existing_state(desired, current.state(), inputs)
}

fn diff_existing_state(
    desired: &PlanInputDesired<'_>,
    current: &State,
    inputs: ExistingComparisonInputs<'_, '_>,
) -> PresentDiff {
    let authored = desired.authored();
    let changes = comparison::find_changed_attributes_with_ignores(
        comparison::AttributeValueViews::new(
            desired.comparison_attributes(),
            inputs.current_attributes,
            inputs.saved,
            inputs.nested_write_only,
        ),
        inputs.prev_explicit,
        inputs.schema,
        authored.ignore_changes(),
        inputs.context,
    );

    if changes.changed.is_empty() {
        PresentDiff::NoChange {
            ignored_readable_differences: changes.ignored_readable_differences,
        }
    } else {
        PresentDiff::Update {
            id: authored.id.clone(),
            from: Box::new(current.clone()),
            to: Box::new(authored.as_resource().clone()),
            changed_attributes: changes.changed,
            ignored_readable_differences: changes.ignored_readable_differences,
        }
    }
}

/// Compare desired state with current state to compute a Diff.
/// `saved_attrs` carries both persisted attributes and nested write-only leaf
/// provenance. Unmanaged nested fields from the saved state are merged into
/// desired before comparison, preventing false diffs when AWS returns extra
/// fields. Requiring the paired input prevents callers from silently dropping
/// nested write-only provenance.
/// If the resource has `prev_explicit`, the actual-state side is projected
/// through that authoring context before comparison so server-side defaults
/// the user never wrote do not surface as diffs (refs awscc#206 and
/// carina-provider-awscc#511). Saved attributes anchor per-element list
/// records when present; otherwise projection uses their legacy union. The
/// authoring tree also drives explicit-removal detection for attributes the
/// user previously wrote but no longer mentions.
/// If `schema` is provided, type-aware comparison is used (e.g., Int/Float coercion,
/// case-insensitive enum matching).
///
/// Pending resources cannot enter the differ:
///
/// ```compile_fail
/// use carina_core::differ::diff;
/// use carina_core::provider::RawSavedAttrs;
/// use carina_core::resource::{Resource, State};
/// use carina_core::schema::SchemaRegistry;
///
/// let resource = Resource::pending("test.Resource");
/// let state = State::not_found(resource.id.clone());
/// let saved = RawSavedAttrs::default().lift(&SchemaRegistry::new());
/// let _ = diff(&resource, &state, &saved, None, None);
/// ```
///
/// Nested write-only provenance is a required planning input:
///
/// ```compile_fail
/// use carina_core::differ::diff;
/// use carina_core::resource::{ResolvedResource, Resource, ResourceId, State};
///
/// let resource = ResolvedResource::new(Resource::new("test.Resource", "example"));
/// let state = State::not_found(ResourceId::with_identity("test.Resource", "example"));
/// let _ = diff(&resource, &state, None, None, None);
/// ```
pub fn diff(
    desired: &ResolvedResource,
    current: &State,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: Option<&crate::explicit::ExplicitFields>,
    schema: Option<&ResourceSchema>,
    context: &ComparisonContext<'_>,
) -> Diff {
    let current_attributes = match schema {
        Some(schema) => {
            CanonicalComparisonAttributes::canonicalize(current.attributes.clone(), schema)
        }
        None => CanonicalComparisonAttributes::from_unschematized(
            crate::resource::UnschematizedComparisonAttributes::new(current.attributes.clone()),
        ),
    };
    let saved = saved_attrs
        .get(&desired.id)
        .cloned()
        .map(|saved| match schema {
            Some(schema) => CanonicalComparisonAttributes::canonicalize(saved, schema),
            None => CanonicalComparisonAttributes::from_unschematized(
                crate::resource::UnschematizedComparisonAttributes::new(saved),
            ),
        });
    diff_with_saved_parts(
        desired,
        current,
        ExistingComparisonInputs {
            current_attributes: &current_attributes,
            saved: saved.as_ref(),
            nested_write_only: saved_attrs.nested_write_only(&desired.id),
            prev_explicit,
            schema,
            context,
        },
    )
}

fn diff_with_saved_parts(
    desired: &ResolvedResource,
    current: &State,
    inputs: ExistingComparisonInputs<'_, '_>,
) -> Diff {
    if !current.exists {
        return Diff::Create(desired.as_resource().clone());
    }
    let comparison_desired = PlanInputDesired::with_resource_schema(desired, inputs.schema);
    match diff_existing_state(&comparison_desired, current, inputs) {
        PresentDiff::Update {
            id,
            from,
            to,
            changed_attributes,
            ..
        } => Diff::Update {
            id,
            from,
            to: *to,
            changed_attributes,
        },
        PresentDiff::NoChange { .. } => Diff::NoChange(desired.id.clone()),
    }
}

#[cfg(test)]
pub(crate) fn diff_test(
    desired: &Resource,
    current: &State,
    saved: Option<&HashMap<String, Value>>,
    prev_explicit: Option<&crate::explicit::ExplicitFields>,
    schema: Option<&ResourceSchema>,
    context: &ComparisonContext<'_>,
) -> Diff {
    let current_attributes = match schema {
        Some(schema) => {
            CanonicalComparisonAttributes::canonicalize(current.attributes.clone(), schema)
        }
        None => CanonicalComparisonAttributes::from_unschematized(
            crate::resource::UnschematizedComparisonAttributes::new(current.attributes.clone()),
        ),
    };
    let saved = saved.cloned().map(|saved| match schema {
        Some(schema) => CanonicalComparisonAttributes::canonicalize(saved, schema),
        None => CanonicalComparisonAttributes::from_unschematized(
            crate::resource::UnschematizedComparisonAttributes::new(saved),
        ),
    });
    diff_with_saved_parts(
        &ResolvedResource::new(desired.clone()),
        current,
        ExistingComparisonInputs {
            current_attributes: &current_attributes,
            saved: saved.as_ref(),
            nested_write_only: &NestedWriteOnlyRecord::new(),
            prev_explicit,
            schema,
            context,
        },
    )
}

#[cfg(test)]
mod cascade_tests;
#[cfg(test)]
mod comparison_tests;
#[cfg(test)]
mod diff_tests;
#[cfg(test)]
mod plan_tests;
