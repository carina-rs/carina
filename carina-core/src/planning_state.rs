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
    persisted_identifier: Option<AppliedIdentifier>,
    current_kind: PlanningCurrentKind,
    binding: Option<AbsoluteBindingAddress>,
    directives: PersistedDirectives,
    dependency_bindings: PersistedDestroyDependencies,
    deposed: Vec<PlanningDeposedRow>,
}

/// Lifecycle classification copied from the authoritative current state row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningCurrentKind {
    None,
    Applied,
    MissingIdentifier,
    CreateInFlight,
    CreateOutcomeUnknown,
}

#[derive(Debug, Clone)]
pub struct PlanningDeposedRow {
    key: DeposedKey,
    identifier: String,
    provider_instance: Option<String>,
    dependency_bindings: PersistedDestroyDependencies,
    attributes: HashMap<String, SnapshotValue>,
}

/// Immutable state-row values supplied by the state owner for core planning.
/// The CLI is responsible for authenticating their source. Core derives write
/// targets and persisted ordering from these values, not parallel arguments.
#[derive(Debug, Clone, Default)]
pub struct PlanningStateRows(Vec<PlanningStateRow>);
impl std::ops::Deref for PlanningStateRows {
    type Target = [PlanningStateRow];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromIterator<PlanningStateRow> for PlanningStateRows {
    fn from_iter<T: IntoIterator<Item = PlanningStateRow>>(rows: T) -> Self {
        Self(rows.into_iter().collect())
    }
}

impl PlanningStateRows {
    /// Join refresh observations by exact persisted address, including instance.
    /// The matching observation supplies the current identifier; an unmatched row
    /// retains its persisted identifier. Observations never replace row addresses.
    pub fn with_current_observations(self, current: &HashMap<ResourceId, Current>) -> Self {
        self.0
            .into_iter()
            .map(|row| row.with_current_observation(current))
            .collect()
    }
}

impl PlanningStateRow {
    /// Project typed persisted fields. Source authenticity is the state owner's responsibility.
    pub fn from_persisted(
        id: ResolvedResourceId,
        identifier: Option<AppliedIdentifier>,
        current_kind: PlanningCurrentKind,
        binding: Option<AbsoluteBindingAddress>,
        directives: PersistedDirectives,
        dependency_bindings: PersistedDestroyDependencies,
        deposed: Vec<PlanningDeposedRow>,
    ) -> Self {
        Self {
            id,
            persisted_identifier: identifier.clone(),
            identifier,
            current_kind,
            binding,
            directives,
            dependency_bindings,
            deposed,
        }
    }

    fn with_current_observation(mut self, current: &HashMap<ResourceId, Current>) -> Self {
        let identifier = match current.get(self.id.as_inner()) {
            Some(Current::Present(present)) => Some(present.identifier().clone()),
            Some(Current::Absent(_)) => None,
            _ => return self,
        };
        self.current_kind = if identifier.is_some() {
            PlanningCurrentKind::Applied
        } else {
            PlanningCurrentKind::None
        };
        self.identifier = identifier;
        self
    }
}

impl PlanningDeposedRow {
    pub fn from_persisted(
        key: DeposedKey,
        identifier: String,
        provider_instance: Option<String>,
        dependency_bindings: PersistedDestroyDependencies,
        attributes: HashMap<String, SnapshotValue>,
    ) -> Self {
        Self {
            key,
            identifier,
            provider_instance,
            dependency_bindings,
            attributes,
        }
    }
}

pub type StateRowKey = (String, String, String);
pub fn state_row_key_from_parts(
    provider: &str,
    resource_type: &str,
    identity: &str,
) -> StateRowKey {
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
    #[error("refreshed identifier for {id} does not match its authoritative state row")]
    IdentifierMismatch { id: Box<ResolvedResourceId> },
}

impl DestroyPlan {
    /// Select state-row addresses for deletion. Resource fields other than the
    /// address are ignored: targets come from rows and ordering from rows and
    /// the retained configuration graph. The CLI authenticates these sources.
    /// Refreshed identifiers win only after an exact state-row observation join.
    /// A requested address whose refreshed identifier differs from the joined row
    /// is rejected as an identifier mismatch, including observations from another instance.
    pub fn from_state<E, D>(
        resources: &[&Resource],
        current: &HashMap<ResourceId, Current>,
        rows: &PlanningStateRows,
        graph: &crate::resolved_graph::ResolvedExpandedGraph<E, D>,
        protected: &HashSet<StateRowKey>,
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
        let mut resolved = Vec::with_capacity(resources.len());
        for resource in resources {
            let row = rows
                .iter()
                .find(|row| {
                    row.id.provider == resource.id.provider
                        && row.id.resource_type == resource.id.resource_type
                        && Some(row.id.identity_str()) == resource.id.identity_str()
                })
                .expect("target rows were checked above");
            if let Some(Current::Present(present)) = current.get(&resource.id)
                && row.identifier.as_ref() != Some(present.identifier())
            {
                return Err(DestroyPlanError::IdentifierMismatch {
                    id: Box::new(row.id.clone()),
                });
            }
            let mut source = graph
                .configuration()
                .resources
                .iter()
                .find(|candidate| {
                    candidate.id.provider == row.id.provider
                        && candidate.id.resource_type == row.id.resource_type
                        && candidate.id.identity_str() == Some(row.id.identity_str())
                })
                .cloned()
                .unwrap_or_else(|| {
                    let mut orphan = Resource::from_id(row.id.as_inner().clone());
                    orphan.directives = (&row.directives).into();
                    orphan.binding = row.binding.clone();
                    orphan
                });
            source.id = row.id.as_inner().clone();
            source.directives.provider_instance = row.id.provider_instance.clone();
            resolved.push(graph.resolve_resource(source, schemas)?);
        }
        let destroyed_ids = resolved
            .iter()
            .map(ResolvedResource::resolved_id)
            .collect::<HashSet<_>>();
        let kept_targets = graph
            .configuration()
            .resources
            .iter()
            .map(|resource| &resource.id)
            .chain(
                graph
                    .configuration()
                    .data_sources
                    .iter()
                    .map(|source| &source.id),
            )
            .filter_map(|id| ResolvedResourceId::try_new(id.clone()))
            .chain(
                rows.iter()
                    .filter(|row| row.current_kind != PlanningCurrentKind::None)
                    .map(|row| row.id.clone()),
            )
            .filter(|id| !destroyed_ids.contains(id))
            .collect();
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
        let Some(row) = rows.iter().find(|row| row.id == resource.resolved_id()) else {
            errors.push(PlanError::new(
                resource.id.clone(),
                crate::plan::PlanErrorKind::MissingProviderIdentifier,
            ));
            continue;
        };
        let identifier = match managed_current.get(&resource.id) {
            Some(Current::Present(_)) => match &row.identifier {
                Some(identifier) => identifier.clone(),
                None => {
                    errors.push(PlanError::new(
                        resource.id.clone(),
                        crate::plan::PlanErrorKind::MissingProviderIdentifier,
                    ));
                    continue;
                }
            },
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
        dependencies.extend_projected(&row.dependency_bindings);
        dependencies.extend_projected(&row.directives.depends_on);
        explicit_dependencies.extend(row.directives.depends_on.iter().cloned());
        delete_effects.push(Effect::Delete {
            id: row.id.clone(),
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
    let blocked_by_current_resume = if row.current_kind == PlanningCurrentKind::CreateInFlight {
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
            row.persisted_identifier
                .as_ref()
                .map(AppliedIdentifier::as_str),
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
    #[cfg(any(test, feature = "test-utils"))]
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

impl StatePlanEdits for PlanFinalization<'_> {
    fn add_state_effect(&mut self, effect: StateOnlyEffect) {
        self.plan.add(effect.0);
    }
    fn add_error(&mut self, error: PlanError) {
        self.plan.add_error(error);
    }
    fn retain(&mut self, keep: impl FnMut(&Effect) -> bool) {
        self.plan.retain(keep);
    }
    fn retain_errors(&mut self, keep: impl FnMut(&PlanError) -> bool) {
        self.plan.retain_errors(keep);
    }
    fn suppress_current_deletes(&mut self, ids: &HashSet<ResourceId>) {
        self.plan.suppress_current_deletes(ids);
    }
    fn abandon_deposed_generation(&mut self, id: ResolvedResourceId, key: DeposedKey) {
        self.plan.abandon_deposed_generation(id, key);
    }
    fn add_deposed_rows(
        &mut self,
        rows: &PlanningStateRows,
        moved: &[(ResolvedResourceId, ResolvedResourceId)],
    ) {
        for row in rows.iter() {
            let (effects, errors) = deposed_delete_effects_for_row(row, moved);
            for effect in effects {
                self.plan.add_delete(effect);
            }
            for error in errors {
                self.plan.add_error(error);
            }
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl StatePlanEdits for Plan {
    fn add_state_effect(&mut self, effect: StateOnlyEffect) {
        self.add(effect.0);
    }
    fn add_error(&mut self, error: PlanError) {
        self.add_error(error);
    }
    fn retain(&mut self, keep: impl FnMut(&Effect) -> bool) {
        self.retain(keep);
    }
    fn retain_errors(&mut self, keep: impl FnMut(&PlanError) -> bool) {
        self.retain_errors(keep);
    }
    fn suppress_current_deletes(&mut self, ids: &HashSet<ResourceId>) {
        self.suppress_current_deletes(ids);
    }
    fn abandon_deposed_generation(&mut self, id: ResolvedResourceId, key: DeposedKey) {
        self.abandon_deposed_generation(id, key);
    }
    fn add_deposed_rows(
        &mut self,
        rows: &PlanningStateRows,
        moved: &[(ResolvedResourceId, ResolvedResourceId)],
    ) {
        for row in rows.iter() {
            let (effects, errors) = deposed_delete_effects_for_row(row, moved);
            for effect in effects {
                self.add_delete(effect);
            }
            for error in errors {
                self.add_error(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied_rows(resource: &Resource) -> PlanningStateRows {
        PlanningStateRows(vec![PlanningStateRow {
            id: ResolvedResourceId::try_new(resource.id.clone()).unwrap(),
            identifier: Some(AppliedIdentifier::new("persisted-id").unwrap()),
            persisted_identifier: Some(AppliedIdentifier::new("persisted-id").unwrap()),
            current_kind: PlanningCurrentKind::Applied,
            binding: resource.binding.clone(),
            directives: Default::default(),
            dependency_bindings: Default::default(),
            deposed: Vec::new(),
        }])
    }

    #[test]
    fn destroy_rejects_current_identifier_different_from_authoritative_row() {
        let resource = Resource::with_provider("mock", "test.Bucket", "stored", None);
        let rows = applied_rows(&resource);
        let graph = crate::resolved_graph::resolve_expanded_graph(
            crate::parser::ExpandedInferredFile {
                resources: vec![resource.clone()],
                ..Default::default()
            }
            .into(),
        )
        .unwrap();
        let state = crate::resource::SnapshotState::existing(resource.id.clone(), HashMap::new())
            .with_identifier("other-physical-id");
        let result = DestroyPlan::from_state(
            &[&resource],
            &HashMap::from([(resource.id.clone(), Current::from_snapshot(state))]),
            &rows,
            &graph,
            &Default::default(),
            &SchemaRegistry::new(),
        );
        assert!(
            result.is_err(),
            "a genuine row must not authorize deleting a parallel current's other identifier"
        );
    }

    #[test]
    fn destroy_uses_graph_dependencies_instead_of_parallel_resource() {
        let mut resource = Resource::with_provider("mock", "test.Bucket", "stored", None);
        resource.binding = Some(crate::resource::absolute_binding_address_for_test("stored"));
        let rows = applied_rows(&resource);
        let mut configuration = crate::parser::ExpandedInferredFile::default();
        configuration.resources.push(resource.clone());
        configuration.resources.push(
            Resource::with_provider("mock", "test.Bucket", "kept", None).with_root_binding("kept"),
        );
        let graph = crate::resolved_graph::resolve_expanded_graph(configuration.into()).unwrap();
        resource.dependencies.depends_on =
            [crate::resource::absolute_binding_address_for_test("kept")]
                .into_iter()
                .collect();
        let state = crate::resource::SnapshotState::existing(resource.id.clone(), HashMap::new())
            .with_identifier("persisted-id");
        let plan = DestroyPlan::from_state(
            &[&resource],
            &HashMap::from([(resource.id.clone(), Current::from_snapshot(state))]),
            &rows,
            &graph,
            &Default::default(),
            &SchemaRegistry::new(),
        )
        .unwrap();
        let [
            Effect::Delete {
                dependencies,
                explicit_dependencies,
                ..
            },
        ] = plan.plan().effects()
        else {
            panic!("expected one delete");
        };
        assert!(
            dependencies.is_empty(),
            "caller-authored ordering must not enter the core plan"
        );
        assert!(explicit_dependencies.is_empty());
        assert_eq!(
            plan.kept_targets,
            [ResolvedResourceId::with_provider_identity(
                "mock",
                "test.Bucket",
                "kept",
                None,
            )]
            .into_iter()
            .collect(),
            "kept targets must come from the graph"
        );
    }

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
            &schemas,
        );
        assert!(
            matches!(result, Err(DestroyPlanError::State(_))),
            "refreshed values without an owned state row must not authorize deletion"
        );
    }
}
