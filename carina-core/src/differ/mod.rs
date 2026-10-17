//! Differ - Compare desired state with current state to generate a Plan
//!
//! Compares the "desired state" declared in DSL with the "current state" fetched
//! from the Provider, and generates a list of required Effects (Plan).

mod comparison;
mod plan;

use std::collections::{HashMap, HashSet};
use std::ops::Index;

use crate::resource::{
    Directives, PlanInputState, ResolvedResource, Resource, ResourceId, State, Value,
    into_plan_input_map,
};
use crate::schema::ResourceSchema;

pub use plan::{block_deletes_on_prior_consumer_updates, create_plan, create_plan_with_cascades};

/// Provider-observed state paired with all host-owned planning metadata for a
/// resource that is no longer present in the desired configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    state: State,
    binding: Option<String>,
    directives: Directives,
}

impl Orphan {
    pub fn new(state: State, binding: Option<String>, directives: Directives) -> Self {
        Self {
            state,
            binding,
            directives,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }

    pub fn directives(&self) -> &Directives {
        &self.directives
    }

    pub fn into_state(self) -> State {
        self.state
    }

    fn with_id(mut self, id: ResourceId) -> Self {
        self.state.id = id;
        self
    }

    pub fn into_state_with_id(self, id: ResourceId) -> State {
        self.with_id(id).into_state()
    }

    /// Replace only provider-observed state while retaining the dependency
    /// bindings carried by the prior state and the orphan's typed binding and
    /// directive metadata.
    pub fn with_refreshed_state(mut self, mut fresh: State) -> Self {
        fresh.id = self.state.id.clone();
        fresh.dependency_bindings = self.state.dependency_bindings;
        self.state = fresh;
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

impl Orphans {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, orphan: Orphan) {
        self.by_id.insert(orphan.state.id.clone(), orphan);
    }

    pub fn remove(&mut self, id: &ResourceId) -> Option<Orphan> {
        self.by_id.remove(id)
    }

    pub fn rekey(&mut self, from: &ResourceId, to: ResourceId) -> bool {
        let Some(orphan) = self.remove(from) else {
            return false;
        };
        self.insert(orphan.with_id(to));
        true
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
        "prior state cannot be owned by both current_states and orphans"
    );
    current_states.extend(additional_states);
}

/// The complete prior-state input to planning.
///
/// Planning-ready current states and typed orphans remain separately owned
/// before the differ, but callers cannot pass only one side to [`create_plan`].
/// The differ materializes same-id orphans as current state for managed
/// resources and emits orphan deletes only for ids that remain unmanaged.
#[derive(Clone, Copy)]
pub struct PlanPriorStates<'a> {
    current_states: &'a HashMap<ResourceId, PlanInputState>,
    orphans: &'a Orphans,
}

impl<'a> PlanPriorStates<'a> {
    pub fn new(
        current_states: &'a HashMap<ResourceId, PlanInputState>,
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
    ) -> HashMap<ResourceId, PlanInputState> {
        let resources: Vec<Resource> = managed
            .iter()
            .map(|resource| resource.as_resource().clone())
            .collect();
        let mut current_states = self.current_states.clone();
        let orphan_states = into_plan_input_map(
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
) -> HashMap<ResourceId, State> {
    managed
        .iter()
        .filter_map(|resource| {
            orphans
                .get(&resource.id)
                .map(|orphan| (resource.id.clone(), orphan.state.clone()))
        })
        .collect()
}

/// Derive the provider-state view for the managed resource set, promoting
/// same-id orphans without changing either prior-state owner.
pub fn provider_states_for(
    current_states: &HashMap<ResourceId, State>,
    orphans: &Orphans,
    managed: &[Resource],
) -> HashMap<ResourceId, State> {
    let mut states = current_states.clone();
    extend_disjoint_states(
        &mut states,
        managed_orphan_provider_states(orphans, managed),
    );
    states
}

/// Run an asynchronous provider-state transform over both prior-state owners.
///
/// The transform may change values but must preserve the joined key set. On
/// success the map is split back into its original owners. Callers propagate
/// errors immediately, so restoration is intentionally skipped on error.
pub async fn transform_prior_provider_states<E>(
    current_states: &mut HashMap<ResourceId, State>,
    orphans: &mut Orphans,
    transform: impl AsyncFnOnce(&mut HashMap<ResourceId, State>) -> Result<(), E>,
) -> Result<(), E> {
    let mut states = std::mem::take(current_states);
    let orphan_states = orphans
        .iter()
        .map(|(id, orphan)| (id.clone(), orphan.state.clone()))
        .collect();
    extend_disjoint_states(&mut states, orphan_states);
    let expected_ids: HashSet<ResourceId> = states.keys().cloned().collect();

    transform(&mut states).await?;

    assert_eq!(
        states.keys().cloned().collect::<HashSet<_>>(),
        expected_ids,
        "provider transform must preserve the complete prior-state key set"
    );
    for (id, orphan) in orphans.iter_mut() {
        if let Some(mut state) = states.remove(id) {
            state.id = orphan.state.id.clone();
            state.dependency_bindings = std::mem::take(&mut orphan.state.dependency_bindings);
            orphan.state = state;
        }
    }
    *current_states = states;
    Ok(())
}

/// Lift enum leaves for every managed provider state, regardless of which
/// prior-state owner currently carries it.
pub fn lift_prior_state_enum_leaves(
    current_states: &mut HashMap<ResourceId, State>,
    orphans: &mut Orphans,
    resources: &[Resource],
    registry: &crate::schema::SchemaRegistry,
) {
    for resource in resources {
        let Some(schema) = registry.get_for(resource) else {
            continue;
        };
        let state = current_states.get_mut(&resource.id).or_else(|| {
            orphans
                .get_mut(&resource.id)
                .map(|orphan| &mut orphan.state)
        });
        if let Some(state) = state {
            crate::utils::lift_state_enum_leaves(&mut state.attributes, schema);
        }
    }
}

/// Consume orphan ownership into the unified prior-state view required by
/// display and saved-plan consumers after the differ has finished.
pub fn into_all_prior_states(
    orphans: Orphans,
    mut current_states: HashMap<ResourceId, State>,
) -> HashMap<ResourceId, State> {
    let orphan_states = orphans
        .into_iter()
        .map(|(id, orphan)| (id, orphan.into_state()))
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
    current_states: &HashMap<ResourceId, crate::resource::PlanInputState>,
    directives_map: &HashMap<ResourceId, crate::resource::Directives>,
    registry: &crate::schema::SchemaRegistry,
    saved_attrs: &crate::provider::LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    orphans: &Orphans,
    wait_bindings: &[crate::parser::WaitBinding],
) -> crate::plan::Plan {
    let managed = ResolvedResource::collect_resolved(managed.iter().cloned())
        .expect("differ test resources must have resolved identities");
    plan::create_plan(
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
use crate::schema::{AttributeType, SchemaRegistry};
#[cfg(test)]
use comparison::find_changed_attributes;

#[cfg(test)]
fn empty_lifted_saved_attrs() -> crate::provider::LiftedSavedAttrs {
    crate::provider::RawSavedAttrs::default().lift(&SchemaRegistry::new())
}

// Re-export comparison primitives for consumers that must agree with
// `find_changed_attributes`: detail rows render with the same equality,
// and executor patch construction uses the same key/value gate.
pub(crate) use comparison::{
    AttrComparison, TypedAttr, key_should_enter_patch, secret_grafted_comparison_view,
    type_aware_equal,
};

/// Returns true when `binding` is `template_binding_name` with one numeric
/// deferred-for index suffix, e.g. `validation_records[0]`.
pub fn binding_matches_deferred_template(binding: &str, template_binding_name: &str) -> bool {
    let Some(suffix) = binding.strip_prefix(template_binding_name) else {
        return false;
    };
    let Some(inner) = suffix.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return false;
    };
    !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit())
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

/// Compare desired state with current state to compute a Diff.
/// If `saved` is provided, unmanaged nested fields from the saved state are merged
/// into desired before comparison, preventing false diffs when AWS returns extra fields.
/// If `prev_explicit` is provided, the actual-state side is projected through
/// the authoring tree before comparison so server-side default fields the
/// user never wrote do not surface as diffs (refs awscc#206). The same
/// tree drives explicit-removal detection for attributes the user
/// previously wrote but no longer mentions.
/// If `schema` is provided, type-aware comparison is used (e.g., Int/Float coercion,
/// case-insensitive enum matching).
///
/// Pending resources cannot enter the differ:
///
/// ```compile_fail
/// use carina_core::differ::diff;
/// use carina_core::resource::{Resource, State};
///
/// let resource = Resource::pending("test.Resource");
/// let state = State::not_found(resource.id.clone());
/// let _ = diff(&resource, &state, None, None, None);
/// ```
pub fn diff(
    desired: &ResolvedResource,
    current: &State,
    saved: Option<&HashMap<String, Value>>,
    prev_explicit: Option<&crate::explicit::ExplicitFields>,
    schema: Option<&ResourceSchema>,
) -> Diff {
    if !current.exists {
        return Diff::Create(desired.as_resource().clone());
    }

    let desired_id = desired.resolved_id();
    let changed = comparison::find_changed_attributes(
        &desired.resolved_attributes(),
        &current.attributes,
        saved,
        prev_explicit,
        schema,
        Some(&desired_id),
    );

    if changed.is_empty() {
        Diff::NoChange(desired.id.clone())
    } else {
        Diff::Update {
            id: desired.id.clone(),
            from: Box::new(current.clone()),
            to: desired.as_resource().clone(),
            changed_attributes: changed,
        }
    }
}

#[cfg(test)]
pub(crate) fn diff_test(
    desired: &Resource,
    current: &State,
    saved: Option<&HashMap<String, Value>>,
    prev_explicit: Option<&crate::explicit::ExplicitFields>,
    schema: Option<&ResourceSchema>,
) -> Diff {
    diff(
        &ResolvedResource::new(desired.clone()),
        current,
        saved,
        prev_explicit,
        schema,
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
