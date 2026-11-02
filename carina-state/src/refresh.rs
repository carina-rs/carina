//! Current-instance refresh eligibility is decided before desired/orphan splitting.
use std::collections::{HashMap, HashSet};

use carina_core::{
    differ::Orphan,
    effect::Effect,
    parser::{StateBlock, StateBlockAddress},
    provider::AppliedIdentifier,
    resource::{
        AbsoluteBindingAddress, Current, PersistedDirectives, ResolvedResourceId, ResourceId,
    },
};

use crate::{DeposedInstance, StateFile};

/// Effective, routed claims minted by state-block resolution or recorded Remove effects.
#[derive(Debug, Clone)]
pub struct ResolvedRemovedClaims(Vec<ResolvedResourceId>);

impl ResolvedRemovedClaims {
    pub fn resolve(blocks: &[StateBlock], state: Option<&StateFile>) -> Self {
        Self(
            blocks
                .iter()
                .filter_map(|block| {
                    let StateBlock::Removed { from } = block else {
                        return None;
                    };
                    let row = state?.find_resource_by_state_block_address(from)?;
                    Some(ResolvedResourceId::with_provider_identity(
                        row.provider.clone(),
                        row.resource_type.clone(),
                        row.identity.clone(),
                        row.directives.provider_instance.clone(),
                    ))
                })
                .collect(),
        )
    }

    pub fn from_effects<S>(effects: &[Effect<S>]) -> Self {
        Self(
            effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Remove { id } => Some(id.clone()),
                    _ => None,
                })
                .collect(),
        )
    }

    pub fn ids(&self) -> impl Iterator<Item = &ResolvedResourceId> {
        self.0.iter()
    }

    pub fn addresses(&self) -> impl Iterator<Item = StateBlockAddress> + '_ {
        self.0.iter().map(|id| {
            StateBlockAddress::new(
                id.provider.clone(),
                id.resource_type.clone(),
                id.identity().clone(),
            )
        })
    }

    fn partition(&self, rows: impl IntoIterator<Item = (ResourceId, RefreshRow)>) -> PreparedRows {
        let claims: HashSet<_> = self.0.iter().map(ResolvedResourceId::as_inner).collect();
        let mut refreshable = RefreshRows::default();
        let mut removed = RemovedRows::default();
        for (id, mut row) in rows {
            // Deposed generations retain their independent refresh lifecycle.
            for generation in std::mem::take(&mut row.deposed) {
                refreshable.deposed.push(RefreshableDeposed {
                    id: ResolvedResourceId::with_provider_identity(
                        id.provider.clone(),
                        id.resource_type.clone(),
                        id.identity().expect("persisted identity").clone(),
                        generation.provider_instance.clone(),
                    ),
                    row_id: ResolvedResourceId::new(id.clone()),
                    generation,
                });
            }
            // The single current-instance refresh eligibility decision.
            if claims.contains(&id) {
                removed.ids.push(ResolvedResourceId::new(id));
            } else {
                refreshable.rows.insert(id, row);
            }
        }
        PreparedRows {
            refreshable,
            removed,
        }
    }
}

#[derive(Debug, Clone)]
struct RefreshRow {
    deposed: Vec<DeposedInstance>,
    current: Current,
    binding: Option<AbsoluteBindingAddress>,
    directives: PersistedDirectives,
}

#[derive(Debug, Clone, Default)]
pub struct PreparedRows {
    pub refreshable: RefreshRows,
    pub removed: RemovedRows,
}

impl PreparedRows {
    pub fn from_state(state: Option<&StateFile>, claims: &ResolvedRemovedClaims) -> Self {
        claims.partition(state.into_iter().flat_map(|state| {
            state.resources().iter().map(|row| {
                let id = ResolvedResourceId::with_provider_identity(
                    row.provider.clone(),
                    row.resource_type.clone(),
                    row.identity.clone(),
                    row.directives.provider_instance.clone(),
                )
                .into_inner();
                let current = state.build_state_for_resource(&id);
                (
                    id,
                    RefreshRow {
                        deposed: row.deposed.clone(),
                        current,
                        binding: row.binding.clone(),
                        directives: row.directives.clone(),
                    },
                )
            })
        }))
    }

    pub fn from_snapshots(
        snapshots: impl IntoIterator<Item = (ResourceId, Current)>,
        claims: &ResolvedRemovedClaims,
    ) -> Self {
        claims.partition(snapshots.into_iter().map(|(id, current)| {
            (
                id,
                RefreshRow {
                    deposed: Vec::new(),
                    current,
                    binding: None,
                    directives: PersistedDirectives::default(),
                },
            )
        }))
    }
}

/// The only authority that can mint current-instance refresh inputs.
#[derive(Debug, Clone, Default)]
pub struct RefreshRows {
    rows: HashMap<ResourceId, RefreshRow>,
    deposed: Vec<RefreshableDeposed>,
}

impl RefreshRows {
    pub fn deposed_targets<'a>(
        &'a self,
        state: &'a StateFile,
    ) -> impl Iterator<Item = RefreshableDeposed> + 'a {
        self.deposed.iter().filter_map(move |selected| {
            // Selection belongs to the partition. Reload the payload after
            // dependency migration so refresh cannot write back stale metadata.
            let row = state.find_resource_for_id(selected.row_id.as_inner())?;
            let generation = row
                .deposed
                .iter()
                .find(|generation| generation.key == selected.generation.key)?
                .clone();
            Some(RefreshableDeposed {
                row_id: selected.row_id.clone(),
                id: ResolvedResourceId::with_provider_identity(
                    row.provider.clone(),
                    row.resource_type.clone(),
                    row.identity.clone(),
                    generation.provider_instance.clone(),
                ),
                generation,
            })
        })
    }
    pub fn current(&self, id: &ResourceId) -> Option<RefreshableCurrent> {
        self.rows.get(id).map(|row| RefreshableCurrent {
            id: id.clone(),
            current: row.current.clone(),
        })
    }

    pub fn build_orphans(&self, desired: &HashSet<ResourceId>) -> RefreshableOrphans {
        RefreshableOrphans(
            self.rows
                .iter()
                .filter_map(|(id, row)| {
                    if desired.contains(id) {
                        return None;
                    }
                    let orphan = match row.current.clone() {
                        Current::Absent(_) => return None,
                        Current::Present(present) => {
                            Orphan::present(present, row.binding.clone(), row.directives.clone())
                        }
                        Current::CreateInFlight(pending) => Orphan::create_in_flight(
                            pending,
                            row.binding.clone(),
                            row.directives.clone(),
                        ),
                        Current::CreateOutcomeUnknown(unknown) => Orphan::create_outcome_unknown(
                            unknown,
                            row.binding.clone(),
                            row.directives.clone(),
                        ),
                        Current::MissingIdentifier(state) => Orphan::missing_identifier(
                            state,
                            row.binding.clone(),
                            row.directives.clone(),
                        ),
                    };
                    Some((
                        id.clone(),
                        RefreshableOrphan {
                            id: id.clone(),
                            orphan,
                        },
                    ))
                })
                .collect(),
        )
    }
}

/// State-only identities. There is deliberately no refresh-input conversion.
///
/// ```compile_fail
/// use carina_state::{RemovedRows, RefreshTarget};
/// fn refresh_removed(rows: RemovedRows) -> RefreshTarget { rows.into() }
/// ```
#[derive(Debug, Clone, Default)]
pub struct RemovedRows {
    ids: Vec<ResolvedResourceId>,
}

impl RemovedRows {
    pub fn ids(&self) -> impl Iterator<Item = &ResolvedResourceId> {
        self.ids.iter()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
    pub fn apply_to(&self, state: &mut StateFile) {
        for id in &self.ids {
            state.remove_resource(&id.provider, &id.resource_type, id.identity_str());
        }
    }
}

/// A classified current row, including lifecycle states that require no read.
///
/// ```compile_fail
/// use carina_core::resource::Current;
/// use carina_state::RefreshableCurrent;
/// fn unprepared(row: Current) -> RefreshableCurrent { row.into() }
/// ```
#[derive(Debug, Clone)]
pub struct RefreshableCurrent {
    id: ResourceId,
    current: Current,
}
impl RefreshableCurrent {
    pub fn current(&self) -> &Current {
        &self.current
    }
    pub fn into_current(self) -> Current {
        self.current
    }
    pub fn read_target(&self) -> Option<RefreshTarget> {
        target_for(&self.id, &self.current)
    }
}

/// An orphan selected from classified current rows.
///
/// ```compile_fail
/// use carina_core::differ::Orphan;
/// use carina_state::RefreshableOrphan;
/// fn unprepared(row: Orphan) -> RefreshableOrphan { row.into() }
/// ```
#[derive(Debug, Clone)]
pub struct RefreshableOrphan {
    id: ResourceId,
    orphan: Orphan,
}
impl RefreshableOrphan {
    pub fn current(&self) -> &Current {
        self.orphan.current()
    }
    pub fn into_orphan(self) -> Orphan {
        self.orphan
    }
    pub fn read_target(&self) -> Option<RefreshTarget> {
        target_for(&self.id, self.orphan.current())
    }
}

#[derive(Debug, Clone, Default)]
pub struct RefreshableOrphans(Vec<(ResourceId, RefreshableOrphan)>);
impl RefreshableOrphans {
    pub fn into_plan_orphans(self) -> carina_core::differ::Orphans {
        self.0
            .into_iter()
            .map(|(_, row)| row.into_orphan())
            .collect()
    }
}
impl IntoIterator for RefreshableOrphans {
    type Item = (ResourceId, RefreshableOrphan);
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Provider-read input, constructible only from a refreshable row.
///
/// ```compile_fail
/// use carina_core::{provider::AppliedIdentifier, resource::ResourceId};
/// use carina_state::RefreshTarget;
/// fn bypass(id: ResourceId, identifier: AppliedIdentifier) -> RefreshTarget {
///     RefreshTarget { id, identifier }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct RefreshTarget {
    id: ResourceId,
    identifier: AppliedIdentifier,
}
impl RefreshTarget {
    pub fn id(&self) -> &ResourceId {
        &self.id
    }
    pub fn identifier(&self) -> &AppliedIdentifier {
        &self.identifier
    }
}
fn target_for(id: &ResourceId, current: &Current) -> Option<RefreshTarget> {
    match current {
        Current::Present(present) => Some(RefreshTarget {
            id: id.clone(),
            identifier: present.identifier().clone(),
        }),
        _ => None,
    }
}

/// A historical generation is refreshable independently of its current row.
#[derive(Debug, Clone)]
pub struct RefreshableDeposed {
    row_id: ResolvedResourceId,
    id: ResolvedResourceId,
    generation: DeposedInstance,
}
impl RefreshableDeposed {
    pub fn row_id(&self) -> &ResolvedResourceId {
        &self.row_id
    }
    pub fn id(&self) -> &ResolvedResourceId {
        &self.id
    }
    pub fn generation(&self) -> &DeposedInstance {
        &self.generation
    }
    pub fn read_target(&self) -> Option<RefreshTarget> {
        Some(RefreshTarget {
            id: self.id.as_inner().clone(),
            identifier: AppliedIdentifier::new(self.generation.identifier.as_str()).ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResourceState;
    use carina_core::resource::State;

    fn id(identity: &'static str, route: Option<&str>) -> ResolvedResourceId {
        ResolvedResourceId::with_provider_identity(
            "mock",
            "test.resource",
            identity,
            route.map(str::to_owned),
        )
    }

    fn generation(key: &str, route: Option<&str>) -> DeposedInstance {
        DeposedInstance::from_persisted_json(
            serde_json::from_value(serde_json::json!(key)).unwrap(),
            format!("{key}-identifier"),
            route.map(str::to_owned),
            HashMap::new(),
            Default::default(),
        )
    }

    #[test]
    fn removed_partition_is_routed_and_precedes_desired_orphan_selection() {
        let mut state = StateFile::new();
        let mut removed =
            ResourceState::try_applied("test.resource", "removed", "mock", "current").unwrap();
        removed.directives.provider_instance = Some("alias".into());
        state.upsert_resource(removed).unwrap();
        state
            .upsert_resource(
                ResourceState::try_applied("test.resource", "unclaimed", "mock", "other").unwrap(),
            )
            .unwrap();
        let claims = ResolvedRemovedClaims::resolve(
            &[
                StateBlock::Removed {
                    from: StateBlockAddress::new("mock", "test.resource", "removed"),
                },
                StateBlock::Removed {
                    from: StateBlockAddress::new("mock", "test.resource", "absent"),
                },
            ],
            Some(&state),
        );
        assert_eq!(
            claims.ids().collect::<Vec<_>>(),
            [&id("removed", Some("alias"))]
        );
        let rows = PreparedRows::from_state(Some(&state), &claims);
        assert_eq!(
            rows.removed.ids().collect::<Vec<_>>(),
            [&id("removed", Some("alias"))]
        );
        assert!(
            rows.refreshable
                .current(id("removed", Some("alias")).as_inner())
                .is_none()
        );
        assert!(
            rows.refreshable
                .current(id("removed", None).as_inner())
                .is_none()
        );
        let unclaimed = rows
            .refreshable
            .current(id("unclaimed", None).as_inner())
            .unwrap();
        assert_eq!(
            unclaimed.read_target().unwrap().identifier().as_str(),
            "other"
        );
        let orphans: Vec<_> = rows
            .refreshable
            .build_orphans(&HashSet::new())
            .into_iter()
            .collect();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].0, *id("unclaimed", None).as_inner());
        assert_eq!(
            rows.refreshable
                .build_orphans(&HashSet::from([id("unclaimed", None).into_inner()]))
                .into_iter()
                .count(),
            0
        );
    }

    #[test]
    fn snapshots_use_recorded_routed_remove_effects() {
        let removed = id("old", Some("alias"));
        let other_route = id("old", None);
        let claims = ResolvedRemovedClaims::from_effects::<carina_core::resource::ModuleSource>(&[
            Effect::Remove {
                id: removed.clone(),
            },
        ]);
        let rows = PreparedRows::from_snapshots(
            [removed.clone(), other_route.clone()].map(|id| {
                let state = State::existing(id.as_inner().clone(), HashMap::new())
                    .with_identifier("identifier");
                (id.into_inner(), Current::from_state(state))
            }),
            &claims,
        );
        assert!(rows.refreshable.current(removed.as_inner()).is_none());
        assert_eq!(
            rows.refreshable
                .current(other_route.as_inner())
                .unwrap()
                .read_target()
                .unwrap()
                .id(),
            other_route.as_inner()
        );
    }

    #[test]
    fn removed_current_and_deposed_only_rows_keep_independent_deposed_targets() {
        let mut state = StateFile::new();
        let mut removed =
            ResourceState::try_applied("test.resource", "removed", "mock", "current").unwrap();
        removed.deposed.push(generation("old", Some("historical")));
        state.upsert_resource(removed).unwrap();
        let mut historical = ResourceState::new("test.resource", "historical", "mock");
        historical.deposed.push(generation("historical-only", None));
        state.upsert_resource(historical).unwrap();
        let claims = ResolvedRemovedClaims::resolve(
            &[
                StateBlock::Removed {
                    from: StateBlockAddress::new("mock", "test.resource", "removed"),
                },
                StateBlock::Removed {
                    from: StateBlockAddress::new("mock", "test.resource", "historical"),
                },
            ],
            Some(&state),
        );
        let rows = PreparedRows::from_state(Some(&state), &claims);
        assert_eq!(rows.removed.ids().count(), 2);
        assert!(
            rows.refreshable
                .current(id("removed", None).as_inner())
                .is_none()
        );
        assert!(
            rows.refreshable
                .current(id("historical", None).as_inner())
                .is_none()
        );
        assert_eq!(
            rows.refreshable
                .build_orphans(&HashSet::new())
                .into_iter()
                .count(),
            0
        );
        let targets: Vec<_> = rows
            .refreshable
            .deposed_targets(&state)
            .map(|generation| generation.read_target().unwrap())
            .collect();
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets[0].id().provider_instance.as_deref(),
            Some("historical")
        );
        assert_eq!(targets[0].identifier().as_str(), "old-identifier");
        rows.removed.apply_to(&mut state);
        assert!(
            state
                .resources()
                .iter()
                .all(|row| !row.has_current_instance() && row.deposed.len() == 1)
        );
    }

    #[test]
    fn deposed_targets_reload_migrated_dependency_metadata() {
        let mut state = StateFile::new();
        let mut row = ResourceState::new("test.resource", "historical", "mock");
        row.deposed.push(generation("old", None));
        state.upsert_resource(row.clone()).unwrap();
        let rows =
            PreparedRows::from_state(Some(&state), &ResolvedRemovedClaims::resolve(&[], None));
        let migrated = carina_core::resource::PersistedDestroyDependencies::from_test_dependencies(
            [id("dependency", None)],
        );
        row.deposed[0].dependency_bindings = migrated.clone();
        state.upsert_resource(row).unwrap();
        let target = rows.refreshable.deposed_targets(&state).next().unwrap();
        assert_eq!(target.generation().dependency_bindings, migrated);
        assert_eq!(
            target.read_target().unwrap().identifier().as_str(),
            "old-identifier"
        );
    }

    #[test]
    fn absent_and_missing_identifier_rows_cannot_mint_provider_read_targets() {
        let absent = id("absent", None).into_inner();
        let missing = id("missing", None).into_inner();
        let rows = PreparedRows::from_snapshots(
            [
                (absent.clone(), Current::absent(absent.clone())),
                (
                    missing.clone(),
                    Current::from_state(State::existing(missing.clone(), HashMap::new())),
                ),
            ],
            &ResolvedRemovedClaims::resolve(&[], None),
        );
        assert!(
            rows.refreshable
                .current(&absent)
                .unwrap()
                .read_target()
                .is_none()
        );
        assert!(
            rows.refreshable
                .current(&missing)
                .unwrap()
                .read_target()
                .is_none()
        );
        assert!(
            rows.refreshable
                .current(id("unpersisted", None).as_inner())
                .is_none()
        );
        assert!(matches!(
            rows.refreshable
                .build_orphans(&HashSet::new())
                .into_iter()
                .next()
                .unwrap()
                .1
                .current(),
            Current::MissingIdentifier(_)
        ));
    }
}
