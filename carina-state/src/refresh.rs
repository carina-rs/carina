//! Current-instance refresh eligibility is decided before desired/orphan splitting.
#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;

use carina_core::{
    differ::Orphan,
    effect::Effect,
    parser::{StateBlock, StateBlockAddress},
    provider::AppliedIdentifier,
    resource::{
        AbsoluteBindingAddress, Current, DeposedKey, PersistedDirectives, ResolvedResourceId,
        ResourceId,
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

    pub fn from_effects<S, V: carina_core::resource::PlanValueKind>(
        effects: &[Effect<S, V>],
    ) -> Self {
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

    fn partition(
        &self,
        rows: impl IntoIterator<Item = (ResolvedResourceId, RefreshRow)>,
    ) -> PreparedRows {
        let claims: HashSet<_> = self.0.iter().map(ResolvedResourceId::as_inner).collect();
        let mut refreshable = RefreshRows::default();
        let mut rows_in_order = Vec::new();
        let mut removed = RemovedRows::default();
        for (id, mut row) in rows {
            // Deposed generations retain their independent refresh lifecycle.
            for key in std::mem::take(&mut row.deposed) {
                refreshable.deposed.push((id.clone(), key));
            }
            // The single current-instance refresh eligibility decision.
            if claims.contains(id.as_inner()) {
                removed.ids.push(id);
            } else {
                let orphan = row.into_orphan();
                let mut dependencies = orphan.destroy_dependencies().clone();
                dependencies.extend_projected(&orphan.directives().depends_on);
                rows_in_order.push((id, dependencies, orphan));
            }
        }
        refreshable.rows = carina_core::differ::CanonicalOrphanRows::new(rows_in_order);
        PreparedRows {
            refreshable,
            removed,
        }
    }
}

#[derive(Debug, Clone)]
struct RefreshRow {
    deposed: Vec<DeposedKey>,
    current: Current,
    binding: Option<AbsoluteBindingAddress>,
    directives: PersistedDirectives,
    dependencies: Option<carina_core::resource::PersistedDestroyDependencies>,
}

impl RefreshRow {
    fn into_orphan(self) -> Orphan {
        let Self {
            current,
            binding,
            directives,
            dependencies,
            ..
        } = self;
        let mut orphan = match current {
            Current::Absent(state) => Orphan::absent(state, binding, directives),
            Current::Present(present) => Orphan::present(present, binding, directives),
            Current::CreateInFlight(pending) => {
                Orphan::create_in_flight(pending, binding, directives)
            }
            Current::CreateOutcomeUnknown(unknown) => {
                Orphan::create_outcome_unknown(unknown, binding, directives)
            }
            Current::MissingIdentifier(state) => {
                Orphan::missing_identifier(state, binding, directives)
            }
        };
        if let Some(dependencies) = dependencies {
            let directives = orphan.directives().clone();
            orphan.set_destroy_metadata(dependencies, directives);
        }
        orphan
    }
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
                let (id, current) = state.orphan_row(row);
                (
                    id,
                    RefreshRow {
                        deposed: row
                            .deposed
                            .iter()
                            .map(|generation| generation.key.clone())
                            .collect(),
                        current,
                        binding: row.binding.clone(),
                        directives: row.directives.clone(),
                        dependencies: Some(row.dependency_bindings.clone()),
                    },
                )
            })
        }))
    }

    pub fn from_snapshots(
        snapshots: impl IntoIterator<Item = (ResolvedResourceId, Current)>,
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
                    dependencies: None,
                },
            )
        }))
    }
}

/// The only authority that can mint current-instance refresh inputs.
#[derive(Debug, Clone, Default)]
pub struct RefreshRows {
    rows: carina_core::differ::CanonicalOrphanRows<Orphan>,
    deposed: Vec<(ResolvedResourceId, DeposedKey)>,
}

impl RefreshRows {
    pub fn deposed_targets<'a>(
        &'a self,
        state: &'a StateFile,
    ) -> impl Iterator<Item = RefreshableDeposed> + 'a {
        self.deposed.iter().filter_map(move |(row_id, key)| {
            // Selection belongs to the partition. Reload the payload after
            // dependency migration so refresh cannot write back stale metadata.
            let row = state.find_resource_for_id(row_id.as_inner())?;
            let generation = row
                .deposed
                .iter()
                .find(|generation| &generation.key == key)?
                .clone();
            Some(RefreshableDeposed {
                row_id: row_id.clone(),
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
            current: row.current().clone(),
        })
    }

    pub fn build_orphans(&self, desired: &HashSet<ResourceId>) -> RefreshableOrphans {
        RefreshableOrphans(carina_core::differ::CanonicalOrphanRows::new(
            self.rows
                .iter()
                .filter(|(id, orphan)| {
                    !desired.contains(id.as_inner())
                        && !matches!(orphan.current(), Current::Absent(_))
                })
                .map(|(id, orphan)| {
                    let mut dependencies = orphan.destroy_dependencies().clone();
                    dependencies.extend_projected(&orphan.directives().depends_on);
                    (id.clone(), dependencies, orphan.clone())
                }),
        ))
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
pub struct RefreshableOrphans(carina_core::differ::CanonicalOrphanRows<Orphan>);
impl RefreshableOrphans {
    pub fn into_plan_orphans(self) -> carina_core::differ::Orphans {
        self.0.into_iter().collect()
    }
}

pub struct RefreshableOrphansIter(
    <carina_core::differ::CanonicalOrphanRows<Orphan> as IntoIterator>::IntoIter,
);
impl Iterator for RefreshableOrphansIter {
    type Item = (ResolvedResourceId, RefreshableOrphan);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(id, orphan)| {
            let row = RefreshableOrphan {
                id: id.as_inner().clone(),
                orphan,
            };
            (id, row)
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}
impl ExactSizeIterator for RefreshableOrphansIter {}
impl IntoIterator for RefreshableOrphans {
    type Item = (ResolvedResourceId, RefreshableOrphan);
    type IntoIter = RefreshableOrphansIter;
    fn into_iter(self) -> Self::IntoIter {
        RefreshableOrphansIter(self.0.into_iter())
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
    fn issue_4491_snapshot_orphan_read_uses_lookup_key_instead_of_payload_id() {
        let lookup = id("before_move", Some("source"));
        let payload = id("after_move", Some("destination"));
        let state = State::existing(payload.as_inner().clone(), HashMap::new())
            .with_identifier("provider-identifier");
        let rows = PreparedRows::from_snapshots(
            [(lookup.clone(), Current::from_state(state))],
            &ResolvedRemovedClaims::resolve(&[], None),
        );
        let current_target = rows
            .refreshable
            .current(lookup.as_inner())
            .unwrap()
            .read_target()
            .unwrap();
        assert_eq!(current_target.id(), lookup.as_inner());
        let mut orphans = rows.refreshable.build_orphans(&HashSet::new()).into_iter();
        let (_, orphan) = orphans.next().unwrap();
        let target = orphan.read_target().unwrap();
        assert_eq!(target.id(), lookup.as_inner());
        assert_eq!(target.identifier().as_str(), "provider-identifier");
        assert_eq!(orphan.current().id(), payload.as_inner());
        assert!(orphans.next().is_none());
    }

    #[test]
    fn snapshots_use_recorded_routed_remove_effects() {
        let removed = id("old", Some("alias"));
        let other_route = id("old", None);
        let claims = ResolvedRemovedClaims::from_effects::<
            carina_core::resource::ModuleSource,
            carina_core::resource::NoPlaceholder,
        >(&[Effect::Remove {
            id: removed.clone(),
        }]);
        let rows = PreparedRows::from_snapshots(
            [removed.clone(), other_route.clone()].map(|id| {
                let state = State::existing(id.as_inner().clone(), HashMap::new())
                    .with_identifier("identifier");
                (id, Current::from_state(state))
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
                (
                    ResolvedResourceId::new(absent.clone()),
                    Current::absent(absent.clone()),
                ),
                (
                    ResolvedResourceId::new(missing.clone()),
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

#[cfg(test)]
mod ordering_tests {
    use super::*;
    use crate::{LoadedOrphans, ResourceState};
    use carina_core::resource::{PersistedDestroyDependencies, ResourceIdentity};

    #[test]
    fn issue_4491_refresh_and_loaded_orphans_share_delete_order() {
        let id = |name: &str| {
            ResolvedResourceId::with_provider_identity(
                "mock",
                "test.resource",
                ResourceIdentity::try_from(name.to_owned()).unwrap(),
                None,
            )
        };
        let mut state = StateFile::new();
        for name in ["alpha", "theta", "beta", "delta", "zulu", "gamma"] {
            let mut row = ResourceState::try_applied("test.resource", name, "mock", name).unwrap();
            let parent = match name {
                "zulu" => Some("beta"),
                "beta" => Some("alpha"),
                _ => None,
            };
            let dependencies = PersistedDestroyDependencies::decode_persisted(
                parent.map(id).into_iter().collect(),
            )
            .unwrap();
            if name == "beta" {
                row.directives.depends_on = dependencies;
            } else {
                row.dependency_bindings = dependencies;
            }
            state.upsert_resource(row).unwrap();
        }
        let prepared = PreparedRows::from_state(
            Some(&state),
            &ResolvedRemovedClaims::resolve(&[], Some(&state)),
        );
        let expected = ["delta", "gamma", "theta", "zulu", "beta", "alpha"];
        let refreshable = prepared.refreshable.build_orphans(&HashSet::new());
        let names: Vec<_> = refreshable
            .clone()
            .into_iter()
            .map(|(id, _)| id.identity_str().to_owned())
            .collect();
        assert_eq!(names, expected);
        let mut loaded = LoadedOrphans::default();
        let mut entries: Vec<_> = refreshable.into_iter().collect();
        entries.reverse();
        for (id, row) in entries {
            loaded.insert(id, row.into_orphan());
        }
        assert_eq!(
            loaded
                .iter()
                .map(|(id, _)| id.identity_str().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            loaded
                .into_plan_orphans()
                .keys()
                .map(|id| id.identity_str().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }
}
