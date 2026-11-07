//! State projections used by core planning. These are state rows, never executable effects.
use crate::effect::deps::{DestroyWaitAlias, KeptDestroyTargets};
use crate::effect::{Effect, EffectGeneration};
use crate::plan::{AdmittedPlan, Plan, PlanError};
use crate::provider::AppliedIdentifier;
use crate::resource::{
    AbsoluteBindingAddress, Current, DeposedKey, ModuleSource, PersistedDestroyDependencies,
    PersistedDirectives, ResolvedDependencies, ResolvedResource, ResolvedResourceId, Resource,
    ResourceId, SnapshotValue,
};
use crate::schema::SchemaRegistry;
use crate::wait::ResolvedWaitBinding;
use std::collections::{HashMap, HashSet};

/// Immutable projection of a persisted resource row, including all old generations.
#[derive(Debug, Clone)]
pub struct PlanningStateRow {
    id: ResolvedResourceId,
    identifier: Option<AppliedIdentifier>,
    create_in_flight: bool,
    binding: Option<AbsoluteBindingAddress>,
    directives: PersistedDirectives,
    dependency_bindings: PersistedDestroyDependencies,
    deposed: Vec<PlanningDeposedRow>,
}

#[derive(Debug, Clone)]
struct PlanningDeposedRow {
    key: DeposedKey,
    identifier: String,
    provider_instance: Option<String>,
    dependency_bindings: PersistedDestroyDependencies,
    attributes: HashMap<String, SnapshotValue>,
}

/// State rows decoded by core from the persisted resource-row representation.
/// Rows have no public field constructor, deserializer or mutable access.
#[derive(Debug, Clone, Default)]
pub struct PlanningStateRows(Vec<PlanningStateRow>);
impl std::ops::Deref for PlanningStateRows {
    type Target = [PlanningStateRow];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(serde::Deserialize)]
struct EncodedPlanningRow {
    provider: String,
    resource_type: String,
    identity: crate::resource::ResourceIdentity,
    current: EncodedPlanningCurrent,
    #[serde(default)]
    binding: Option<AbsoluteBindingAddress>,
    #[serde(default)]
    directives: PersistedDirectives,
    #[serde(default)]
    dependency_bindings: std::collections::BTreeSet<ResolvedResourceId>,
    #[serde(default)]
    deposed: Vec<EncodedPlanningDeposed>,
}
#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EncodedPlanningCurrent {
    None,
    Applied { identifier: AppliedIdentifier },
    CreateInFlight,
    CreateOutcomeUnknown,
    MissingIdentifier,
}
#[derive(serde::Deserialize)]
struct EncodedPlanningDeposed {
    key: DeposedKey,
    identifier: String,
    #[serde(default)]
    provider_instance: Option<String>,
    #[serde(default)]
    dependency_bindings: std::collections::BTreeSet<ResolvedResourceId>,
    attributes: HashMap<String, serde_json::Value>,
}
impl PlanningStateRows {
    /// Decode the authoritative stored rows, retaining old-generation routing,
    /// dependency projections and typed persisted attributes.
    pub fn from_state_rows_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        use serde::de::Error as _;
        let rows: Vec<EncodedPlanningRow> = serde_json::from_slice(bytes)?;
        rows.into_iter()
            .map(|row| {
                let id = ResolvedResourceId::try_new(ResourceId::with_provider_identity(
                    row.provider,
                    row.resource_type,
                    row.identity,
                    row.directives.provider_instance.clone(),
                ))
                .ok_or_else(|| {
                    serde_json::Error::custom("persisted resource row requires a complete address")
                })?;
                let create_in_flight =
                    matches!(row.current, EncodedPlanningCurrent::CreateInFlight);
                let identifier = match row.current {
                    EncodedPlanningCurrent::Applied { identifier } => Some(identifier),
                    _ => None,
                };
                let deposed = row
                    .deposed
                    .into_iter()
                    .map(|old| {
                        Ok(PlanningDeposedRow {
                            key: old.key,
                            identifier: old.identifier,
                            provider_instance: old.provider_instance,
                            dependency_bindings: PersistedDestroyDependencies::decode_persisted(
                                old.dependency_bindings,
                            )
                            .map_err(serde_json::Error::custom)?,
                            attributes: crate::provider::PersistedAttributes::from_encoded(
                                old.attributes,
                            )
                            .map_err(serde_json::Error::custom)?
                            .to_values(),
                        })
                    })
                    .collect::<Result<Vec<_>, serde_json::Error>>()?;
                Ok(PlanningStateRow {
                    id,
                    identifier,
                    create_in_flight,
                    binding: row.binding,
                    directives: row.directives,
                    dependency_bindings: PersistedDestroyDependencies::decode_persisted(
                        row.dependency_bindings,
                    )
                    .map_err(serde_json::Error::custom)?,
                    deposed,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

pub type StateRowKey = (String, String, String);
fn state_row_key_from_parts(provider: &str, resource_type: &str, identity: &str) -> StateRowKey {
    (
        provider.to_owned(),
        resource_type.to_owned(),
        identity.to_owned(),
    )
}

/// Destroy intent sealed by core from current state, persisted generations and its graph.
/// It has no raw-plan constructor, deserializer or mutable effect access.
pub struct DestroyPlan<S = ModuleSource> {
    pub(crate) admitted: AdmittedPlan<S>,
    pub(crate) aliases: Vec<DestroyWaitAlias>,
    pub(crate) kept_targets: KeptDestroyTargets,
}

#[derive(Debug, thiserror::Error)]
pub enum DestroyPlanError {
    #[error(transparent)]
    Graph(#[from] crate::resolved_graph::ExpandedGraphResolutionError),
    #[error(transparent)]
    Admission(#[from] crate::replacement::ReplacementAdmissionError),
    #[error("invalid destroy state")]
    State(Vec<PlanError>),
}

impl DestroyPlan {
    #[allow(clippy::too_many_arguments)]
    pub fn from_state<E, D>(
        resources: &[&Resource],
        current: &HashMap<ResourceId, Current>,
        rows: &PlanningStateRows,
        graph: &crate::resolved_graph::ResolvedExpandedGraph<E, D>,
        protected: &HashSet<StateRowKey>,
        kept_targets: KeptDestroyTargets,
        schemas: &SchemaRegistry,
    ) -> Result<Self, DestroyPlanError> {
        let missing_rows = resources
            .iter()
            .filter(|resource| {
                !rows.iter().any(|row| {
                    row.id.provider == resource.id.provider
                        && row.id.resource_type == resource.id.resource_type
                        && Some(row.id.identity_str()) == resource.id.identity_str()
                })
            })
            .map(|resource| {
                PlanError::new(
                    resource.id.clone(),
                    crate::plan::PlanErrorKind::MissingProviderIdentifier,
                )
            })
            .collect::<Vec<_>>();
        if !missing_rows.is_empty() {
            return Err(DestroyPlanError::State(missing_rows));
        }
        let resolved = resources
            .iter()
            .map(|resource| graph.resolve_resource((*resource).clone(), schemas))
            .collect::<Result<Vec<_>, _>>()?;
        let (effects, errors) = build_destroy_delete_effects(&resolved, current, rows, protected);
        if !errors.is_empty() {
            return Err(DestroyPlanError::State(errors));
        }
        let waits = graph
            .configuration()
            .wait_bindings
            .iter()
            .map(|wait| graph.resolve_wait_binding(wait))
            .collect::<Result<Vec<_>, _>>()?;
        let aliases = build_destroy_wait_aliases(&waits, &effects);
        let mut plan = Plan::new();
        for effect in effects {
            plan.add(effect);
        }
        Ok(Self {
            admitted: plan.admit(schemas)?,
            aliases,
            kept_targets,
        })
    }
}
impl<S> DestroyPlan<S> {
    pub fn plan(&self) -> &AdmittedPlan<S> {
        &self.admitted
    }
}

pub fn build_destroy_wait_aliases(
    wait_bindings: &[ResolvedWaitBinding],
    delete_effects: &[Effect],
) -> Vec<DestroyWaitAlias> {
    let delete_dependencies: Vec<(ResolvedResourceId, ResolvedDependencies)> = delete_effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Delete {
                id, dependencies, ..
            } => Some((id.clone(), dependencies.to_resolved_dependencies())),
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => None,
        })
        .collect();
    let destroy_targets: HashSet<&ResolvedResourceId> =
        delete_dependencies.iter().map(|(id, _)| id).collect();
    let referenced_bindings: HashSet<&ResolvedResourceId> = delete_dependencies
        .iter()
        .flat_map(|(_, dependencies)| dependencies.iter())
        .collect();

    wait_bindings
        .iter()
        .filter(|wait| referenced_bindings.contains(&wait.id))
        .filter_map(|wait| {
            if !destroy_targets.contains(wait.target_id.id()) {
                return None;
            }
            let mut consumers = delete_dependencies
                .iter()
                .filter(|(_, dependencies)| dependencies.contains(&wait.id))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            consumers.sort();
            consumers.dedup();

            DestroyWaitAlias::new(
                wait.id.clone(),
                wait.target_id.id().clone(),
                wait.explicit_dependencies.clone(),
                consumers,
            )
        })
        .collect()
}

pub fn build_destroy_delete_effects(
    resources_to_destroy: &[ResolvedResource],
    managed_current: &HashMap<ResourceId, Current>,
    rows: &[PlanningStateRow],
    protected_row_keys: &HashSet<StateRowKey>,
) -> (Vec<Effect>, Vec<PlanError>) {
    let mut delete_effects = Vec::with_capacity(resources_to_destroy.len());
    let mut errors = Vec::new();
    for resource in resources_to_destroy {
        let identifier = match managed_current.get(&resource.id) {
            Some(Current::Present(present)) => present.identifier().clone(),
            Some(Current::Absent(_state)) => {
                errors.push(PlanError::new(
                    resource.id.clone(),
                    crate::plan::PlanErrorKind::MissingProviderIdentifier,
                ));
                continue;
            }
            Some(Current::MissingIdentifier(_state)) => {
                errors.push(PlanError::new(
                    resource.id.clone(),
                    crate::plan::PlanErrorKind::MissingProviderIdentifier,
                ));
                continue;
            }
            Some(Current::CreateInFlight(_pending)) => {
                errors.push(PlanError::new(
                    resource.id.clone(),
                    crate::plan::PlanErrorKind::DestroyCreateInFlight,
                ));
                continue;
            }
            Some(Current::CreateOutcomeUnknown(unknown)) => {
                errors.push(PlanError::create_outcome_unknown(unknown.id().clone()));
                continue;
            }
            None => {
                errors.push(PlanError::new(
                    resource.id.clone(),
                    crate::plan::PlanErrorKind::MissingProviderIdentifier,
                ));
                continue;
            }
        };
        let mut dependencies = resource.persisted_value_destroy_dependencies().clone();
        dependencies.extend_projected(resource.persisted_explicit_destroy_dependencies());
        let mut explicit_dependencies = resource.explicit_dependencies().clone();
        if let Some(row) = rows.iter().find(|row| {
            row.id.provider == resource.id.provider
                && row.id.resource_type == resource.id.resource_type
                && row.id.identity_str() == resource.identity_str()
        }) {
            dependencies.extend_projected(&row.dependency_bindings);
            dependencies.extend_projected(&row.directives.depends_on);
            explicit_dependencies.extend(row.directives.depends_on.iter().cloned());
        }
        delete_effects.push(Effect::Delete {
            id: resource.resolved_id(),
            identifier,
            generation: EffectGeneration::Current,
            directives: resource.directives.clone(),
            binding: resource.binding.clone(),
            dependencies,
            explicit_dependencies,
            blocked_by_updates: ResolvedDependencies::default(),
        });
    }
    {
        for row in rows {
            if protected_row_keys.contains(&state_row_key_from_parts(
                &row.id.provider,
                &row.id.resource_type,
                row.id.identity_str(),
            )) {
                continue;
            }
            let (effects, row_errors) = deposed_delete_effects_for_row(row, &[]);
            delete_effects.extend(effects.iter().map(|old| old.to_delete_effect()));
            errors.extend(row_errors);
        }
    }
    (delete_effects, errors)
}

pub fn deposed_delete_effects_for_row(
    row: &PlanningStateRow,
    moved_pairs: &[(ResolvedResourceId, ResolvedResourceId)],
) -> (Vec<crate::effect::DeferredReplaceDelete>, Vec<PlanError>) {
    let mut effects = Vec::with_capacity(row.deposed.len());
    let mut errors = Vec::new();
    let row_id = row.id.clone();
    let blocked_by_current_resume = if row.create_in_flight {
        std::iter::once(
            moved_pairs
                .iter()
                .find_map(|(from, to)| (from == &row_id).then(|| to.clone()))
                .unwrap_or_else(|| row_id.clone()),
        )
        .collect()
    } else {
        ResolvedDependencies::default()
    };
    for deposed in &row.deposed {
        let id = ResolvedResourceId::with_provider_identity(
            row.id.provider.clone(),
            row.id.resource_type.clone(),
            row.id.identity().clone(),
            deposed.provider_instance.clone(),
        );
        match PlanError::require_deposed_provider_identifier(
            id.as_inner(),
            &deposed.key,
            deposed.identifier.as_str(),
            row.identifier.as_ref().map(AppliedIdentifier::as_str),
        ) {
            Ok(identifier) => effects.push(crate::effect::DeferredReplaceDelete {
                id,
                identifier,
                generation: EffectGeneration::Deposed(deposed.key.clone()),
                directives: Default::default(),
                binding: row.binding.clone(),
                dependencies: deposed.dependency_bindings.clone(),
                explicit_dependencies: ResolvedDependencies::default(),
                blocked_by_updates: blocked_by_current_resume.clone(),
                previous_attributes: deposed.attributes.clone(),
            }),
            Err(error) => errors.push(*error),
        }
    }
    (effects, errors)
}

mod sealed {
    pub trait StatePlanEdits {}
    impl StatePlanEdits for crate::plan::Plan {}
    impl StatePlanEdits for super::PlanFinalization<'_> {}
}

/// An ancillary operation that cannot dispatch a provider write.
pub struct StateOnlyEffect(Effect);
impl StateOnlyEffect {
    pub fn try_new(effect: Effect) -> Option<Self> {
        matches!(
            effect,
            Effect::Import { .. } | Effect::Remove { .. } | Effect::Move { .. }
        )
        .then_some(Self(effect))
    }
}

/// State-only ancillary edits. Implementations cannot append a caller's write effect.
pub trait StatePlanEdits: sealed::StatePlanEdits {
    fn add_state_effect(&mut self, effect: StateOnlyEffect);
    fn add_error(&mut self, error: PlanError);
    fn retain(&mut self, keep: impl FnMut(&Effect) -> bool);
    fn retain_errors(&mut self, keep: impl FnMut(&PlanError) -> bool);
    fn suppress_current_deletes(&mut self, ids: &HashSet<ResourceId>);
    fn abandon_deposed_generation(&mut self, id: ResolvedResourceId, key: DeposedKey);
    fn add_deposed_rows(
        &mut self,
        rows: &PlanningStateRows,
        moved: &[(ResolvedResourceId, ResolvedResourceId)],
    );
}

/// A finalization callback borrows this view; it cannot replace or mutate write targets.
pub struct PlanFinalization<'a> {
    pub(crate) plan: &'a mut Plan,
}
impl std::ops::Deref for PlanFinalization<'_> {
    type Target = Plan;
    fn deref(&self) -> &Plan {
        self.plan
    }
}
impl PlanFinalization<'_> {
    pub fn block_deletes_on_prior_consumer_updates(
        &mut self,
        prior: &HashMap<ResourceId, PersistedDirectives>,
    ) {
        crate::differ::block_deletes_on_prior_consumer_updates(self.plan, prior);
    }
}

macro_rules! state_edits {
    ($ty:ty, $this:ident, $plan:expr) => {
        impl StatePlanEdits for $ty {
            fn add_state_effect(&mut self, effect: StateOnlyEffect) {
                let $this = self;
                ($plan).add(effect.0);
            }
            fn add_error(&mut self, error: PlanError) {
                let $this = self;
                ($plan).add_error(error);
            }
            fn retain(&mut self, keep: impl FnMut(&Effect) -> bool) {
                let $this = self;
                ($plan).retain(keep);
            }
            fn retain_errors(&mut self, keep: impl FnMut(&PlanError) -> bool) {
                let $this = self;
                ($plan).retain_errors(keep);
            }
            fn suppress_current_deletes(&mut self, ids: &HashSet<ResourceId>) {
                let $this = self;
                ($plan).suppress_current_deletes(ids);
            }
            fn abandon_deposed_generation(&mut self, id: ResolvedResourceId, key: DeposedKey) {
                let $this = self;
                ($plan).abandon_deposed_generation(id, key);
            }
            fn add_deposed_rows(
                &mut self,
                rows: &PlanningStateRows,
                moved: &[(ResolvedResourceId, ResolvedResourceId)],
            ) {
                let $this = self;
                let plan: &mut Plan = $plan;
                for row in rows.iter() {
                    let (effects, errors) = deposed_delete_effects_for_row(row, moved);
                    for effect in effects {
                        plan.add_delete(effect);
                    }
                    for error in errors {
                        plan.add_error(error);
                    }
                }
            }
        }
    };
}
state_edits!(Plan, plan, plan);
state_edits!(PlanFinalization<'_>, view, &mut *view.plan);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destroy_requires_persisted_source_row() {
        let schemas = SchemaRegistry::new();
        let graph = crate::resolved_graph::resolve_expanded_graph(
            crate::parser::ExpandedInferredFile::default().into(),
        )
        .unwrap();
        let resource = Resource::with_provider("mock", "test.Bucket", "chosen", None);
        let mut state =
            crate::resource::SnapshotState::existing(resource.id.clone(), HashMap::new());
        state.identifier = Some("caller-chosen-id".into());
        let current = HashMap::from([(resource.id.clone(), Current::from_snapshot(state))]);
        let result = DestroyPlan::from_state(
            &[&resource],
            &current,
            &Default::default(),
            &graph,
            &Default::default(),
            Default::default(),
            &schemas,
        );
        assert!(
            matches!(result, Err(DestroyPlanError::State(_))),
            "refreshed values without an owned state row must not authorize deletion"
        );
    }
}
