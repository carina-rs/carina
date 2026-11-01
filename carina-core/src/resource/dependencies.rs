//! Dependency phases and the managed-row persistence boundary.
use super::*;

impl PartialOrd for ResolvedResourceId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ResolvedResourceId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            &self.provider,
            &self.resource_type,
            self.identity_str(),
            &self.provider_instance,
        )
            .cmp(&(
                &other.provider,
                &other.resource_type,
                other.identity_str(),
                &other.provider_instance,
            ))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResolvedDependencies(BTreeSet<ResolvedResourceId>);

impl ResolvedDependencies {
    pub fn iter(&self) -> impl Iterator<Item = &ResolvedResourceId> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn contains(&self, dependency: &ResolvedResourceId) -> bool {
        self.0.contains(dependency)
    }

    pub fn insert(&mut self, dependency: ResolvedResourceId) -> bool {
        self.0.insert(dependency)
    }

    pub fn extend(&mut self, dependencies: impl IntoIterator<Item = ResolvedResourceId>) {
        self.0.extend(dependencies);
    }
}

/// Parser-phase dependency fields that must be consumed by graph resolution.
///
/// The dependency phase is a type parameter on [`Resource`] and
/// [`DataSource`]. Resolved wrappers hold the same payload with `D = ()`, so
/// parser spellings cannot survive into post-resolution code as silently empty
/// collections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnexpandedDependencyFields<B: BindingAddress = AbsoluteBindingAddress> {
    pub value_bindings: BTreeSet<B>,
    pub depends_on: crate::parser::UnexpandedDependencies<B>,
}

impl<B: BindingAddress> Default for UnexpandedDependencyFields<B> {
    fn default() -> Self {
        Self {
            value_bindings: BTreeSet::new(),
            depends_on: Default::default(),
        }
    }
}
impl UnexpandedDependencyFields<RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> UnexpandedDependencyFields<AbsoluteBindingAddress> {
        UnexpandedDependencyFields {
            value_bindings: self.value_bindings.into_iter().map(&mut *resolve).collect(),
            depends_on: self.depends_on.resolve_bindings(resolve),
        }
    }
}
impl UnexpandedDependencyFields<AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        self.value_bindings = std::mem::take(&mut self.value_bindings)
            .into_iter()
            .map(|binding| map(&binding))
            .collect();
        self.depends_on.map_absolute_bindings(map);
    }
}

impl FromIterator<ResolvedResourceId> for ResolvedDependencies {
    fn from_iter<T: IntoIterator<Item = ResolvedResourceId>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for ResolvedDependencies {
    type Item = ResolvedResourceId;
    type IntoIter = std::collections::btree_set::IntoIter<ResolvedResourceId>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

pub(crate) const SYNTHETIC_TYPE_PREFIX: &str = "__";

/// Managed-resource rows that may be persisted as destroy-ordering edges.
///
/// Unlike [`ResolvedDependencies`], this set can never contain plan-scoped
/// nodes such as data sources or waits. Non-empty values are minted only by
/// the expanded-graph projection in [`crate::resolved_graph`]. Persisted data
/// must cross [`Self::decode_persisted`]; no public insertion, direct
/// deserialization, or `FromIterator` route exists.
/// Nonempty projections are private to graph resolution:
///
/// ```compile_fail,E0624
/// use carina_core::resource::PersistedDestroyDependencies;
/// let _ = PersistedDestroyDependencies::from_graph_projection(std::iter::empty());
/// ```
///
/// Raw decoding must validate managed-row shape:
///
/// ```compile_fail,E0277
/// use carina_core::resource::PersistedDestroyDependencies;
/// let _: PersistedDestroyDependencies = serde_json::from_str("[]").unwrap();
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PersistedDestroyDependencies(BTreeSet<ResolvedResourceId>);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("dependency `{0}` is not a managed state row")]
pub struct NotAManagedRow(pub ResolvedResourceId);

impl PersistedDestroyDependencies {
    pub(crate) fn from_graph_projection(
        dependencies: impl IntoIterator<Item = ResolvedResourceId>,
    ) -> Self {
        Self(dependencies.into_iter().collect())
    }

    /// Decode the persisted set after proving every address has managed-row
    /// shape. Data-source exclusion remains a writer-side graph invariant;
    /// unlike scheduler nodes, a dangling data-source address is not
    /// distinguishable from a managed row that left the configuration.
    pub fn decode_persisted(
        dependencies: BTreeSet<ResolvedResourceId>,
    ) -> Result<Self, NotAManagedRow> {
        if let Some(dependency) = dependencies.iter().find(|dependency| {
            dependency.provider().is_empty()
                || dependency
                    .resource_type()
                    .starts_with(SYNTHETIC_TYPE_PREFIX)
        }) {
            return Err(NotAManagedRow(dependency.clone()));
        }
        Ok(Self(dependencies))
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResolvedResourceId> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn contains(&self, dependency: &ResolvedResourceId) -> bool {
        self.0.contains(dependency)
    }

    /// Union two values that have already crossed the graph-projection seam.
    pub fn extend_projected(&mut self, dependencies: &Self) {
        self.0.extend(dependencies.0.iter().cloned());
    }

    /// Project persisted row edges back to scheduler edges for delete plans.
    pub fn to_resolved_dependencies(&self) -> ResolvedDependencies {
        self.0.iter().cloned().collect()
    }

    /// Rewrite only the address spelling of already-projected rows.
    ///
    /// This supports state-format canonicalization without opening a route to
    /// insert data-source or wait IDs into persisted dependencies.
    pub fn try_map_addresses<E: From<NotAManagedRow>>(
        self,
        mut map: impl FnMut(ResolvedResourceId) -> Result<ResolvedResourceId, E>,
    ) -> Result<Self, E> {
        self.0
            .into_iter()
            .map(&mut map)
            .collect::<Result<BTreeSet<_>, _>>()
            .and_then(|dependencies| Self::decode_persisted(dependencies).map_err(E::from))
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn from_test_dependencies(
        dependencies: impl IntoIterator<Item = ResolvedResourceId>,
    ) -> Self {
        Self::from_graph_projection(dependencies)
    }
}

pub(crate) fn deserialize_persisted_destroy_dependencies<'de, D>(
    deserializer: D,
) -> Result<PersistedDestroyDependencies, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let dependencies = BTreeSet::<ResolvedResourceId>::deserialize(deserializer)?;
    PersistedDestroyDependencies::decode_persisted(dependencies).map_err(serde::de::Error::custom)
}

/// Exhaustive result of projecting one expanded graph source to state rows.
///
/// External/upstream values and literal module arguments deliberately produce
/// `NoLocalDestroyEdge`; they are not represented by an accidental empty
/// lookup or a silently dropped scheduler node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedDestroyDependencyProjection {
    Managed(
        #[serde(deserialize_with = "deserialize_persisted_destroy_dependencies")]
        PersistedDestroyDependencies,
    ),
    NoLocalDestroyEdge,
}

impl PersistedDestroyDependencyProjection {
    #[cfg(any(test, feature = "test-utils"))]
    pub fn into_dependencies(self) -> PersistedDestroyDependencies {
        match self {
            Self::Managed(dependencies) => dependencies,
            Self::NoLocalDestroyEdge => PersistedDestroyDependencies::default(),
        }
    }
}

/// Attribute-level provenance for value-reference dependency edges.
///
/// This is scheduler metadata, never a provider/state value. `Unknown` is
/// conservative and prevents update/update edge relaxation; `Known` records
/// the exact top-level attributes read from the dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ResolvedDependencyRead {
    Known(BTreeSet<String>),
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResolvedDependencyReads(BTreeMap<ResolvedResourceId, ResolvedDependencyRead>);

#[derive(Serialize)]
struct ResolvedDependencyReadWireRef<'a> {
    id: &'a ResolvedResourceId,
    read: &'a ResolvedDependencyRead,
}

#[derive(Deserialize)]
struct ResolvedDependencyReadWire {
    id: ResolvedResourceId,
    read: ResolvedDependencyRead,
}

impl Serialize for ResolvedDependencyReads {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;

        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for (id, read) in &self.0 {
            sequence.serialize_element(&ResolvedDependencyReadWireRef { id, read })?;
        }
        sequence.end()
    }
}

impl<'de> Deserialize<'de> for ResolvedDependencyReads {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let entries = Vec::<ResolvedDependencyReadWire>::deserialize(deserializer)?;
        let mut reads = BTreeMap::new();
        for entry in entries {
            if reads.insert(entry.id.clone(), entry.read).is_some() {
                return Err(D::Error::custom(format!(
                    "duplicate resolved dependency-read address {}",
                    entry.id,
                )));
            }
        }
        Ok(Self(reads))
    }
}

impl ResolvedDependencyReads {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn unknown_for(dependencies: &ResolvedDependencies) -> Self {
        Self(
            dependencies
                .iter()
                .cloned()
                .map(|id| (id, ResolvedDependencyRead::Unknown))
                .collect(),
        )
    }

    pub(crate) fn insert_known(&mut self, id: ResolvedResourceId, attribute: impl Into<String>) {
        let attribute = attribute.into();
        self.0
            .entry(id)
            .and_modify(|read| {
                if let ResolvedDependencyRead::Known(attributes) = read {
                    attributes.insert(attribute.clone());
                }
            })
            .or_insert_with(|| ResolvedDependencyRead::Known(BTreeSet::from([attribute])));
    }

    pub(crate) fn insert_unknown(&mut self, id: ResolvedResourceId) {
        self.0.insert(id, ResolvedDependencyRead::Unknown);
    }

    pub(crate) fn ensure_unknown(&mut self, id: ResolvedResourceId) {
        self.0.entry(id).or_insert(ResolvedDependencyRead::Unknown);
    }

    pub(crate) fn known_attributes(&self, id: &ResolvedResourceId) -> Option<&BTreeSet<String>> {
        match self.0.get(id) {
            Some(ResolvedDependencyRead::Known(attributes)) => Some(attributes),
            Some(ResolvedDependencyRead::Unknown) | None => None,
        }
    }
}

mod deferred_phase {
    use super::*;
    pub trait Sealed<B: BindingAddress> {
        fn retain(&mut self, bindings: BTreeSet<B>);
    }
    impl<B: BindingAddress> Sealed<B> for UnexpandedDependencyFields<B> {
        fn retain(&mut self, bindings: BTreeSet<B>) {
            self.value_bindings.extend(bindings);
        }
    }
    impl<B: BindingAddress> Sealed<B> for () {
        fn retain(&mut self, _bindings: BTreeSet<B>) {}
    }
}
pub trait DeferredDependencyPhase<B: BindingAddress>: Clone + deferred_phase::Sealed<B> {}
impl<B: BindingAddress> DeferredDependencyPhase<B> for UnexpandedDependencyFields<B> {}
impl<B: BindingAddress> DeferredDependencyPhase<B> for () {}
pub(crate) fn retain_deferred_iterable_dependencies<
    B: BindingAddress,
    D: DeferredDependencyPhase<B>,
>(
    dependencies: &mut D,
    bindings: BTreeSet<B>,
) {
    deferred_phase::Sealed::retain(dependencies, bindings);
}

/// Compiled graph metadata for children that will materialize during apply.
/// It contains addresses and their managed-row projections, never Values or
/// live handles. Only graph resolution can mint a nonempty template.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeferredDependencyTemplate {
    value: ResolvedDependencies,
    explicit: ResolvedDependencies,
    #[serde(deserialize_with = "deserialize_persisted_destroy_dependencies")]
    persisted_value: PersistedDestroyDependencies,
    #[serde(deserialize_with = "deserialize_persisted_destroy_dependencies")]
    persisted_explicit: PersistedDestroyDependencies,
    reads: ResolvedDependencyReads,
}
impl DeferredDependencyTemplate {
    pub(crate) fn from_graph(
        value: ResolvedDependencies,
        explicit: ResolvedDependencies,
        persisted_value: PersistedDestroyDependencies,
        persisted_explicit: PersistedDestroyDependencies,
        reads: ResolvedDependencyReads,
    ) -> Self {
        Self {
            value,
            explicit,
            persisted_value,
            persisted_explicit,
            reads,
        }
    }
    pub fn dependencies(&self) -> ResolvedDependencies {
        self.value
            .iter()
            .chain(self.explicit.iter())
            .cloned()
            .collect()
    }
    pub(crate) fn materialize<S>(
        &self,
        mut resource: Resource<S, NoPlaceholder, AbsoluteBindingAddress, ()>,
        schemas: &crate::schema::SchemaRegistry,
    ) -> Result<ResolvedResource<S>, ResolvedResourceError> {
        resolve_resource_ignore_changes(&mut resource, schemas)?;
        Ok(ResolvedResource::from_resolved_parts_with_reads(
            resource,
            self.value.clone(),
            self.explicit.clone(),
            self.reads.clone(),
        )
        .with_persisted_destroy_dependencies(
            self.persisted_value.clone(),
            self.persisted_explicit.clone(),
        ))
    }
    #[cfg(any(test, feature = "test-utils", feature = "test-support"))]
    pub fn empty_for_test() -> Self {
        Self::from_graph(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        )
    }
}
