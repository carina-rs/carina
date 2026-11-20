use std::collections::{BTreeSet, HashMap};
use std::ops::Index;
use std::sync::OnceLock;

use indexmap::IndexMap;

use super::Orphan;
use crate::effect::MoveAuthority;
use crate::resource::{
    Current, PersistedDestroyDependencies, ResolvedResourceId, ResourceId, SnapshotState,
    SnapshotValue,
};

/// Orphans in deletion order: dependents before dependencies, with full
/// resource address breaking ties. Mutations retain this order, including
/// updates to persisted destroy metadata. CanonicalOrphanRows assigns the order.
///
/// Insertion requires a resolved lookup key:
///
/// ```compile_fail,E0308
/// use carina_core::{differ::{Orphan, Orphans}, resource::ResourceId};
/// fn insert_pending(orphans: &mut Orphans, orphan: Orphan) {
///     orphans.insert(ResourceId::pending("test.resource"), orphan);
/// }
/// ```
#[derive(Clone, Default)]
pub struct Orphans {
    by_id: IndexMap<ResolvedResourceId, Orphan>,
    order: OnceLock<CanonicalOrphanRows<()>>,
}

impl std::fmt::Debug for Orphans {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct Entries<'a>(&'a Orphans);
        impl std::fmt::Debug for Entries<'_> {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.debug_map().entries(self.0.iter()).finish()
            }
        }
        formatter
            .debug_struct("Orphans")
            .field("by_id", &Entries(self))
            .finish()
    }
}

impl PartialEq for Orphans {
    fn eq(&self, other: &Self) -> bool {
        self.by_id == other.by_id
    }
}

#[derive(Clone)]
pub struct OrderedOrphanIter<'a> {
    by_id: &'a IndexMap<ResolvedResourceId, Orphan>,
    ids: indexmap::map::Keys<'a, ResolvedResourceId, ()>,
}
impl<'a> Iterator for OrderedOrphanIter<'a> {
    type Item = (&'a ResourceId, &'a Orphan);
    fn next(&mut self) -> Option<Self::Item> {
        self.ids.next().map(|id| {
            self.by_id
                .get_key_value(id)
                .map(|(id, orphan)| (id.as_inner(), orphan))
                .expect("canonical orphan is present")
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.ids.size_hint()
    }
}
impl ExactSizeIterator for OrderedOrphanIter<'_> {}

impl Orphans {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: ResolvedResourceId, orphan: Orphan) {
        self.by_id.insert(key, orphan);
        self.order.take();
    }

    pub fn remove(&mut self, id: &ResourceId) -> Option<Orphan> {
        let removed = self.by_id.swap_remove(id)?;
        self.order.take();
        Some(removed)
    }

    pub fn rekey(&mut self, authority: &MoveAuthority<'_>) -> bool {
        let Some(orphan) = self.remove(authority.from()) else {
            return false;
        };
        let orphan = orphan.rebound_for_move(authority);
        let destination = authority.destination_address();
        let id = ResolvedResourceId::with_provider_identity(
            destination.provider,
            destination.resource_type,
            destination.name,
            orphan.current().id().provider_instance.clone(),
        );
        self.insert(id, orphan);
        true
    }

    pub fn take_rebound(&mut self, authority: &MoveAuthority<'_>) -> Option<Current> {
        self.remove(authority.from())
            .map(|orphan| orphan.rebound_for_move(authority).into_current())
    }

    pub fn get(&self, id: &ResourceId) -> Option<&Orphan> {
        self.by_id.get(id)
    }

    pub(super) fn attributes_mut(
        &mut self,
        id: &ResourceId,
    ) -> Option<&mut std::collections::BTreeMap<String, SnapshotValue>> {
        self.by_id.get_mut(id)?.attributes_mut()
    }

    pub(super) fn apply_transformed_states(
        &mut self,
        states: &mut HashMap<ResourceId, SnapshotState>,
    ) {
        for (id, orphan) in &mut self.by_id {
            if let Some(state) = states.remove(id.as_inner())
                && let Some(target) = orphan.state_bearing_mut()
            {
                target.apply_transformed_state(state);
            }
        }
        self.order.take();
    }

    pub fn contains_key(&self, id: &ResourceId) -> bool {
        self.by_id.contains_key(id)
    }
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
    pub fn keys(&self) -> impl Iterator<Item = &ResourceId> {
        self.resolved_keys().map(ResolvedResourceId::as_inner)
    }
    pub fn resolved_keys(&self) -> impl Iterator<Item = &ResolvedResourceId> {
        self.canonical_order().keys()
    }
    pub fn iter(&self) -> OrderedOrphanIter<'_> {
        OrderedOrphanIter {
            by_id: &self.by_id,
            ids: self.canonical_order().by_id.keys(),
        }
    }

    fn canonical_order(&self) -> &CanonicalOrphanRows<()> {
        self.order.get_or_init(|| {
            CanonicalOrphanRows::new(self.by_id.iter().map(|(id, orphan)| {
                let mut dependencies = orphan.destroy_dependencies().clone();
                dependencies.extend_projected(&orphan.directives().depends_on);
                (id.clone(), dependencies, ())
            }))
        })
    }
}

/// Rows whose sole construction path assigns reverse dependency order and
/// full-address tie breaking. The lookup key can retain a refresh source
/// address while its payload carries a saved post-move snapshot.
/// Pending identities cannot enter this owner:
///
/// ```compile_fail,E0271
/// use carina_core::{differ::CanonicalOrphanRows, resource::ResourceId};
/// let _ = CanonicalOrphanRows::new([(ResourceId::pending("test.resource"), Default::default(), ())]);
/// ```
///
/// The consuming iterator does not expose the backing map implementation:
///
/// ```compile_fail,E0308
/// use carina_core::{differ::CanonicalOrphanRows, resource::ResolvedResourceId};
/// let rows = CanonicalOrphanRows::<()>::default();
/// let _: indexmap::map::IntoIter<ResolvedResourceId, ()> = rows.into_iter();
/// ```
#[derive(Debug, Clone)]
pub struct CanonicalOrphanRows<T> {
    by_id: IndexMap<ResolvedResourceId, T>,
}

impl<T> Default for CanonicalOrphanRows<T> {
    fn default() -> Self {
        Self {
            by_id: IndexMap::new(),
        }
    }
}

impl<T> CanonicalOrphanRows<T> {
    pub fn new(
        entries: impl IntoIterator<Item = (ResolvedResourceId, PersistedDestroyDependencies, T)>,
    ) -> Self {
        let mut by_id = IndexMap::new();
        let mut dependencies = IndexMap::new();
        for (id, parents, row) in entries {
            dependencies.insert(id.clone(), parents);
            by_id.insert(id, row);
        }
        let mut ids: Vec<_> = by_id.keys().cloned().collect();
        ids.sort_by_cached_key(|id| (id.to_string(), id.provider_instance.clone(), id.clone()));
        // Lookup-only: the ordered index is assigned above, never by this map.
        let indices: HashMap<_, _> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id, index))
            .collect();
        let parents: Vec<BTreeSet<usize>> = ids
            .iter()
            .map(|id| {
                dependencies[id]
                    .iter()
                    .filter_map(|id| indices.get(id).copied())
                    .collect()
            })
            .collect();
        let components = orphan_components(&parents);
        let mut component_of = vec![0; ids.len()];
        for (component, members) in components.iter().enumerate() {
            for &member in members {
                component_of[member] = component;
            }
        }
        let component_parents: Vec<BTreeSet<_>> = components
            .iter()
            .enumerate()
            .map(|(component, members)| {
                members
                    .iter()
                    .flat_map(|&member| &parents[member])
                    .map(|&parent| component_of[parent])
                    .filter(|&parent| parent != component)
                    .collect()
            })
            .collect();
        let mut children = vec![0; components.len()];
        for dependencies in &component_parents {
            for &parent in dependencies {
                children[parent] += 1;
            }
        }
        // Components and their members are ranked by smallest full address.
        // Ready cycles and singletons share the same address tie break.
        let mut ready: BTreeSet<_> = (0..components.len())
            .filter(|&component| children[component] == 0)
            .collect();
        let mut ordered = Vec::with_capacity(ids.len());
        while let Some(component) = ready.pop_first() {
            ordered.extend(
                components[component]
                    .iter()
                    .map(|&index| ids[index].clone()),
            );
            for &parent in &component_parents[component] {
                children[parent] -= 1;
                if children[parent] == 0 {
                    ready.insert(parent);
                }
            }
        }
        Self {
            by_id: ordered
                .into_iter()
                .map(|id| {
                    let row = by_id.swap_remove(&id).expect("canonical row is present");
                    (id, row)
                })
                .collect(),
        }
    }

    pub fn get(&self, id: &ResourceId) -> Option<&T> {
        self.by_id.get(id)
    }
    pub fn keys(&self) -> impl Iterator<Item = &ResolvedResourceId> {
        self.by_id.keys()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&ResolvedResourceId, &T)> {
        self.by_id.iter()
    }
}

/// Strongly connected groups, each sorted by address rank, then ranked by
/// their smallest member. Iterative walks also handle long dependency chains.
fn orphan_components(parents: &[BTreeSet<usize>]) -> Vec<Vec<usize>> {
    let mut children = vec![Vec::new(); parents.len()];
    for (child, dependencies) in parents.iter().enumerate() {
        for &parent in dependencies {
            children[parent].push(child);
        }
    }
    let mut visited = vec![false; parents.len()];
    let mut finished = Vec::with_capacity(parents.len());
    for start in 0..parents.len() {
        let mut pending = vec![(start, false)];
        while let Some((index, finish)) = pending.pop() {
            if finish {
                finished.push(index);
            } else if !std::mem::replace(&mut visited[index], true) {
                pending.push((index, true));
                pending.extend(parents[index].iter().rev().map(|&parent| (parent, false)));
            }
        }
    }
    visited.fill(false);
    let mut components = Vec::new();
    for start in finished.into_iter().rev() {
        if visited[start] {
            continue;
        }
        let mut members = Vec::new();
        let mut pending = vec![start];
        visited[start] = true;
        while let Some(index) = pending.pop() {
            members.push(index);
            for &child in &children[index] {
                if !std::mem::replace(&mut visited[child], true) {
                    pending.push(child);
                }
            }
        }
        members.sort_unstable();
        components.push(members);
    }
    components.sort_unstable_by_key(|members| members[0]);
    components
}

/// Consuming iteration in canonical orphan order.
#[derive(Debug, Clone)]
pub struct CanonicalOrphanRowsIntoIter<T>(indexmap::map::IntoIter<ResolvedResourceId, T>);

impl<T> Iterator for CanonicalOrphanRowsIntoIter<T> {
    type Item = (ResolvedResourceId, T);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<T> DoubleEndedIterator for CanonicalOrphanRowsIntoIter<T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back()
    }
}

impl<T> ExactSizeIterator for CanonicalOrphanRowsIntoIter<T> {}
impl<T> std::iter::FusedIterator for CanonicalOrphanRowsIntoIter<T> {}

impl<T> IntoIterator for CanonicalOrphanRows<T> {
    type Item = (ResolvedResourceId, T);
    type IntoIter = CanonicalOrphanRowsIntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        CanonicalOrphanRowsIntoIter(self.by_id.into_iter())
    }
}

impl FromIterator<(ResolvedResourceId, Orphan)> for Orphans {
    fn from_iter<T: IntoIterator<Item = (ResolvedResourceId, Orphan)>>(iter: T) -> Self {
        Self {
            by_id: iter.into_iter().collect(),
            order: OnceLock::new(),
        }
    }
}
impl<const N: usize> From<[(ResolvedResourceId, Orphan); N]> for Orphans {
    fn from(orphans: [(ResolvedResourceId, Orphan); N]) -> Self {
        orphans.into_iter().collect()
    }
}
impl IntoIterator for Orphans {
    type Item = (ResourceId, Orphan);
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        let ids = self.canonical_order().keys().cloned().collect::<Vec<_>>();
        let mut by_id = self.by_id;
        ids.into_iter()
            .map(|id| {
                let orphan = by_id.swap_remove(&id).expect("canonical orphan is present");
                (id.into_inner(), orphan)
            })
            .collect::<Vec<_>>()
            .into_iter()
    }
}
impl<'a> IntoIterator for &'a Orphans {
    type Item = (&'a ResourceId, &'a Orphan);
    type IntoIter = OrderedOrphanIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl Index<&ResourceId> for Orphans {
    type Output = Orphan;
    fn index(&self, index: &ResourceId) -> &Self::Output {
        &self.by_id[index]
    }
}
