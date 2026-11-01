//! `DataSource` — a read-only resource that is queried but not
//! managed.
//!
//! Part of the resource typestate split (#3169). A `DataSource`
//! carries the same fields as a [`Resource`](super::Resource)
//! minus `prefixes` (auto-generated names do not apply to read-only
//! lookups). `directives` is retained because `depends_on` and
//! `provider_instance` are still meaningful when ordering reads
//! against other resources.

use std::collections::HashSet;
use std::ops::Deref;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::{
    AbsoluteBindingAddress, BindingAddress, Directives, LeftoverPlaceholder, ModuleSource,
    NoPlaceholder, ParserPlaceholder, PlaceholderKind, ProviderModuleSource, RelativeBindingName,
    ResolvedDependencies, ResolvedDependencyReads, ResolvedResourceId, ResourceId,
    ResourceIdentity, ResourceIdentityState, SavedModuleSource, UnexpandedDependencyFields, Value,
};

/// A read-only resource (data source).
///
/// # Dropped fields (compile-time invariants)
///
/// `prefixes` is dropped (auto-generated names do not apply to
/// read-only lookups):
///
/// ```compile_fail
/// use carina_core::resource::DataSource;
/// fn _f(d: &DataSource) -> &std::collections::HashMap<String, String> {
///     &d.prefixes
/// }
/// ```
///
/// Saved module metadata cannot be placed on the live data-source type:
///
/// ```compile_fail,E0308
/// use carina_core::resource::{DataSource, SavedModuleSource};
///
/// let mut data_source = DataSource::new("mock.Lookup", "example");
/// data_source.module_source = Some(SavedModuleSource::Module {
///     name: "lookup_module".to_string(),
///     instance: "example".to_string(),
/// });
/// ```
///
/// A live data source cannot be deserialized from saved-plan JSON because the
/// persisted module variant has no live call path:
///
/// ```compile_fail,E0277
/// use carina_core::resource::DataSource;
/// let _: DataSource = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "S: Serialize, P: PlaceholderKind + Serialize, B: BindingAddress + Serialize, D: Serialize",
    deserialize = "S: Deserialize<'de>, P: PlaceholderKind + Deserialize<'de>, B: BindingAddress + Deserialize<'de>, D: Deserialize<'de>"
))]
pub struct DataSource<
    S = ModuleSource,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
    D = UnexpandedDependencyFields<B>,
> {
    pub id: ResourceId,
    /// Source-order preserving map of attribute name → expression.
    pub attributes: IndexMap<String, Value<P, B>>,
    /// `directives` meta-argument block — `depends_on` and
    /// `provider_instance` are meaningful for data sources too.
    #[serde(default)]
    pub directives: Directives,
    /// Binding name from `let` bindings in DSL.
    #[serde(default)]
    pub binding: Option<B>,
    /// Binding names this data source depends on.
    pub dependencies: D,
    /// Module source info for data sources from modules.
    #[serde(default)]
    pub module_source: Option<S>,
    /// Parser-level: attributes whose value was written as a quoted
    /// string literal. Parse-time only; `#[serde(skip)]` keeps it out
    /// of state — mirrors [`Resource::quoted_string_attrs`](super::Resource).
    #[serde(default, skip)]
    pub quoted_string_attrs: HashSet<String>,
}

/// Persisted data-source representation used by saved plans.
pub type SavedDataSource = DataSource<SavedModuleSource, NoPlaceholder, AbsoluteBindingAddress, ()>;

pub type AuthoredDataSource<S = ModuleSource> =
    DataSource<S, ParserPlaceholder, RelativeBindingName>;
pub type ExpandedDataSource<S = ModuleSource> =
    DataSource<S, NoPlaceholder, AbsoluteBindingAddress>;

/// Source-neutral data-source payload exposed to providers.
pub type ProviderDataSource =
    DataSource<ProviderModuleSource, NoPlaceholder, AbsoluteBindingAddress, ()>;

impl<P: PlaceholderKind, B: BindingAddress, D> From<DataSource<ModuleSource, P, B, D>>
    for DataSource<SavedModuleSource, P, B, D>
{
    fn from(data_source: DataSource<ModuleSource, P, B, D>) -> Self {
        let DataSource {
            id,
            attributes,
            directives,
            binding,
            dependencies,
            module_source,
            quoted_string_attrs,
        } = data_source;
        Self {
            id,
            attributes,
            directives,
            binding,
            dependencies,
            module_source: module_source.as_ref().map(SavedModuleSource::from),
            quoted_string_attrs,
        }
    }
}

impl<P: PlaceholderKind, B: BindingAddress>
    DataSource<ModuleSource, P, B, crate::resource::UnexpandedDependencyFields<B>>
{
    /// Create a pending data source with an empty attribute map.
    pub fn pending(resource_type: impl Into<String>) -> Self {
        Self {
            id: ResourceId::pending(resource_type),
            attributes: IndexMap::new(),
            directives: Directives::default(),
            binding: None,
            dependencies: Default::default(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }

    /// Create a resolved data source with an empty attribute map.
    pub fn new(resource_type: impl Into<String>, identity: impl Into<ResourceIdentity>) -> Self {
        Self {
            id: ResourceId::with_identity(resource_type, identity),
            attributes: IndexMap::new(),
            directives: Directives::default(),
            binding: None,
            dependencies: Default::default(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }

    /// The data source's id wrapped as a [`PersistentId`] — i.e., an
    /// id that may legally enter state-load APIs.
    ///
    /// `DataSource` is a leaf node and persists in state (as a
    /// snapshot), so its id is `PersistentId`-typed.
    pub fn persistent_id(&self) -> super::PersistentId {
        super::PersistentId::new(self.id.clone())
    }

    /// Create a data source with a provider-qualified id.
    pub fn with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
        provider_instance: Option<String>,
    ) -> Self {
        Self {
            id: ResourceId::with_provider_identity(
                provider,
                resource_type,
                identity,
                provider_instance,
            ),
            attributes: IndexMap::new(),
            directives: Directives::default(),
            binding: None,
            dependencies: Default::default(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }

    /// Create a pending data source with a provider-qualified id.
    pub fn pending_with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        provider_instance: Option<String>,
    ) -> Self {
        Self {
            id: ResourceId::pending_with_provider(provider, resource_type, provider_instance),
            attributes: IndexMap::new(),
            directives: Directives::default(),
            binding: None,
            dependencies: Default::default(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }
}

impl<S: Clone, P: PlaceholderKind, B: BindingAddress, D: Clone> DataSource<S, P, B, D> {
    pub(crate) fn instantiate(&self) -> Self {
        let mut data_source = self.clone();
        data_source.id = self.id.instantiate_pending();
        data_source
    }

    pub(crate) fn instantiate_with_identity(&self, identity: ResourceIdentity) -> Self {
        let mut data_source = self.clone();
        data_source.id = self.id.instantiate_with_identity(identity);
        data_source
    }
}

impl<S, B: BindingAddress, D> DataSource<S, ParserPlaceholder, B, D> {
    pub(crate) fn into_expanded(
        self,
    ) -> Result<DataSource<S, NoPlaceholder, B, D>, LeftoverPlaceholder> {
        let DataSource {
            id,
            attributes,
            directives,
            binding,
            dependencies,
            module_source,
            quoted_string_attrs,
        } = self;
        let attributes = super::into_expanded_value_map(attributes, Some(&id))?;
        Ok(DataSource {
            id,
            attributes,
            directives,
            binding,
            dependencies,
            module_source,
            quoted_string_attrs,
        })
    }
}

impl<S, D> DataSource<S, NoPlaceholder, AbsoluteBindingAddress, D> {
    /// Remove module provenance while crossing into provider-owned code.
    pub(crate) fn into_provider_data_source(self) -> ProviderDataSource {
        let DataSource {
            id,
            attributes,
            directives,
            binding,
            dependencies: _,
            module_source: _,
            quoted_string_attrs,
        } = self;
        ProviderDataSource {
            id,
            attributes,
            directives,
            binding,
            dependencies: (),
            module_source: None,
            quoted_string_attrs,
        }
    }
}

impl<S, P: PlaceholderKind, B: BindingAddress, D> DataSource<S, P, B, D> {
    pub fn get_attr(&self, key: &str) -> Option<&Value<P, B>> {
        self.attributes.get(key)
    }

    pub fn set_attr(&mut self, key: impl Into<String>, value: Value<P, B>) {
        self.attributes.insert(key.into(), value);
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: Value<P, B>) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }
}

impl<S, P: PlaceholderKind> DataSource<S, P, RelativeBindingName> {
    pub fn with_binding(mut self, binding: impl Into<String>) -> Self {
        self.binding = Some(RelativeBindingName::new(binding.into()));
        self
    }

    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> DataSource<S, P, AbsoluteBindingAddress> {
        let Self {
            id,
            attributes,
            directives,
            binding,
            dependencies,
            module_source,
            quoted_string_attrs,
        } = self;
        DataSource {
            id,
            attributes: attributes
                .into_iter()
                .map(|(name, value)| (name, value.resolve_bindings(resolve)))
                .collect(),
            directives,
            binding: binding.map(&mut *resolve),
            dependencies: dependencies.resolve_bindings(resolve),
            module_source,
            quoted_string_attrs,
        }
    }
}

impl<P: PlaceholderKind> DataSource<ModuleSource, P, AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        let Self {
            id: _id,
            attributes,
            directives: _,
            binding,
            dependencies,
            module_source,
            quoted_string_attrs: _quoted_string_attrs,
        } = self;
        for value in attributes.values_mut() {
            value.map_absolute_bindings(map);
        }
        if let Some(binding) = binding {
            *binding = map(binding);
        }
        dependencies.map_absolute_bindings(map);
        if let Some(source) = module_source {
            source.map_absolute_bindings(map);
        }
    }
}

impl<S, P: PlaceholderKind, B: BindingAddress, D> DataSource<S, P, B, D> {
    /// Value-only saved origin; graph metadata belongs to ResolvedDataSource.
    pub fn into_value_snapshot(self) -> DataSource<S, P, B, ()> {
        self.with_dependency_phase(())
    }

    pub(crate) fn with_dependency_phase<E>(self, dependencies: E) -> DataSource<S, P, B, E> {
        let DataSource {
            id,
            attributes,
            directives,
            binding,
            dependencies: _,
            module_source,
            quoted_string_attrs,
        } = self;
        DataSource {
            id,
            attributes,
            directives,
            binding,
            dependencies,
            module_source,
            quoted_string_attrs,
        }
    }
}

/// A [`DataSource`] whose identity has been resolved.
///
/// The inner field is private so callers cannot bypass the constructor
/// invariant:
///
/// ```compile_fail
/// use carina_core::resource::{DataSource, ResolvedDataSource};
///
/// let resource = DataSource::pending("test");
/// let _ = ResolvedDataSource(resource);
/// ```
///
/// ```compile_fail
/// use carina_core::resource::{DataSource, ResolvedDataSource};
///
/// let resource = DataSource::pending("test");
/// let _: ResolvedDataSource = resource.into();
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "S: Serialize + Clone",
    deserialize = "S: Deserialize<'de>"
))]
#[serde(
    try_from = "ResolvedDataSourceWire<S>",
    into = "ResolvedDataSourceWire<S>"
)]
pub struct ResolvedDataSource<S = ModuleSource> {
    resource: DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>,
    value_dependencies: ResolvedDependencies,
    explicit_dependencies: ResolvedDependencies,
    value_dependency_reads: ResolvedDependencyReads,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "S: Serialize", deserialize = "S: Deserialize<'de>"))]
struct ResolvedDataSourceWire<S> {
    #[serde(flatten)]
    resource: DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>,
    #[serde(default, skip_serializing_if = "ResolvedDependencies::is_empty")]
    value_dependencies: ResolvedDependencies,
    #[serde(default, skip_serializing_if = "ResolvedDependencies::is_empty")]
    explicit_dependencies: ResolvedDependencies,
    #[serde(default, skip_serializing_if = "ResolvedDependencyReads::is_empty")]
    value_dependency_reads: ResolvedDependencyReads,
}

pub type SavedResolvedDataSource = ResolvedDataSource<SavedModuleSource>;

impl From<ResolvedDataSource> for SavedResolvedDataSource {
    fn from(resource: ResolvedDataSource) -> Self {
        let (resource, value_dependencies, explicit_dependencies, value_dependency_reads) =
            resource.into_parts();
        Self::from_resolved_parts_with_reads(
            resource.into(),
            value_dependencies,
            explicit_dependencies,
            value_dependency_reads,
        )
    }
}

impl<S> ResolvedDataSource<S> {
    /// Construct from a [`DataSource`], panicking if identity is pending.
    pub fn new(resource: DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>) -> Self {
        assert!(
            matches!(
                resource.id.identity_state(),
                ResourceIdentityState::Resolved(_)
            ),
            "ResolvedDataSource requires identity"
        );
        Self {
            resource,
            value_dependencies: ResolvedDependencies::default(),
            explicit_dependencies: ResolvedDependencies::default(),
            value_dependency_reads: ResolvedDependencyReads::default(),
        }
    }

    pub fn from_resolved_parts(
        resource: DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>,
        value_dependencies: ResolvedDependencies,
        explicit_dependencies: ResolvedDependencies,
    ) -> Self {
        let value_dependency_reads = ResolvedDependencyReads::unknown_for(&value_dependencies);
        Self::from_resolved_parts_with_reads(
            resource,
            value_dependencies,
            explicit_dependencies,
            value_dependency_reads,
        )
    }

    /// Rebuild a graph-resolved data-source row from persisted typed parts.
    ///
    /// Persisted state has no parser dependency fields to expand: both edge
    /// sets have already crossed the resolved-address boundary. Accepting the
    /// typed id and dependency sets directly keeps that distinction explicit
    /// without exposing the dependency-phase transition on `DataSource`.
    pub fn from_persisted_parts(
        id: ResolvedResourceId,
        attributes: IndexMap<String, Value>,
        directives: Directives,
        binding: Option<AbsoluteBindingAddress>,
        value_dependencies: ResolvedDependencies,
        explicit_dependencies: ResolvedDependencies,
    ) -> Self {
        Self::from_resolved_parts(
            DataSource {
                id: id.into(),
                attributes,
                directives,
                binding,
                dependencies: (),
                module_source: None,
                quoted_string_attrs: HashSet::new(),
            },
            value_dependencies,
            explicit_dependencies,
        )
    }

    pub(crate) fn from_resolved_parts_with_reads(
        resource: DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>,
        value_dependencies: ResolvedDependencies,
        explicit_dependencies: ResolvedDependencies,
        value_dependency_reads: ResolvedDependencyReads,
    ) -> Self {
        assert!(
            matches!(
                resource.id.identity_state(),
                ResourceIdentityState::Resolved(_)
            ),
            "ResolvedDataSource requires identity"
        );
        Self {
            resource,
            value_dependencies,
            explicit_dependencies,
            value_dependency_reads,
        }
    }

    /// Try to construct; returns `None` if identity is pending.
    pub fn try_new(
        resource: DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>,
    ) -> Option<Self> {
        match resource.id.identity_state() {
            ResourceIdentityState::Pending(_) => None,
            ResourceIdentityState::Resolved(_) => Some(Self::new(resource)),
        }
    }

    /// Convert data-source fixtures at a boundary where identity assignment is
    /// known to be complete. Production paths obtain this witness from the
    /// expanded graph instead.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn collect_resolved(
        resources: impl IntoIterator<Item = DataSource<S>>,
    ) -> Result<Vec<Self>, super::PendingResourceIdentityError> {
        resources
            .into_iter()
            .map(|resource| match resource.id.identity_state() {
                ResourceIdentityState::Pending(_) => {
                    Err(super::PendingResourceIdentityError::new(resource.id))
                }
                ResourceIdentityState::Resolved(_) => {
                    Ok(Self::new(resource.with_dependency_phase(())))
                }
            })
            .collect()
    }

    /// Borrow the inner [`DataSource`].
    pub fn as_inner(&self) -> &DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()> {
        &self.resource
    }

    /// Consume and return the inner [`DataSource`].
    pub fn into_inner(self) -> DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()> {
        self.resource
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>,
        ResolvedDependencies,
        ResolvedDependencies,
        ResolvedDependencyReads,
    ) {
        (
            self.resource,
            self.value_dependencies,
            self.explicit_dependencies,
            self.value_dependency_reads,
        )
    }

    /// Replace only the value-bearing data-source payload while preserving
    /// the already-resolved graph metadata.
    pub(crate) fn replace_inner(
        &self,
        resource: DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>,
    ) -> Self {
        assert_eq!(
            resource.id, self.resource.id,
            "replacing a resolved data source cannot change its address"
        );
        Self::from_resolved_parts_with_reads(
            resource,
            self.value_dependencies.clone(),
            self.explicit_dependencies.clone(),
            self.value_dependency_reads.clone(),
        )
    }

    pub fn value_dependencies(&self) -> &ResolvedDependencies {
        &self.value_dependencies
    }

    pub fn explicit_dependencies(&self) -> &ResolvedDependencies {
        &self.explicit_dependencies
    }

    pub fn all_dependencies(&self) -> impl Iterator<Item = &ResolvedResourceId> {
        self.value_dependencies
            .iter()
            .chain(self.explicit_dependencies.iter())
    }

    pub fn identity(&self) -> &ResourceIdentity {
        match self.resource.id.identity_state() {
            ResourceIdentityState::Pending(_) => {
                unreachable!("ResolvedDataSource requires identity")
            }
            ResourceIdentityState::Resolved(identity) => identity,
        }
    }

    pub fn identity_str(&self) -> &str {
        self.identity().as_str()
    }
}

impl<S> Deref for ResolvedDataSource<S> {
    type Target = DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>;

    fn deref(&self) -> &Self::Target {
        &self.resource
    }
}

impl<S> From<ResolvedDataSource<S>> for DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()> {
    fn from(resource: ResolvedDataSource<S>) -> Self {
        resource.resource
    }
}

impl<S> TryFrom<DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>>
    for ResolvedDataSource<S>
{
    type Error = String;

    fn try_from(
        resource: DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>,
    ) -> Result<Self, Self::Error> {
        Self::try_new(resource).ok_or_else(|| "identity is required".to_string())
    }
}

impl<S> TryFrom<ResolvedDataSourceWire<S>> for ResolvedDataSource<S> {
    type Error = String;

    fn try_from(wire: ResolvedDataSourceWire<S>) -> Result<Self, Self::Error> {
        if matches!(
            wire.resource.id.identity_state(),
            ResourceIdentityState::Pending(_)
        ) {
            return Err("identity is required".to_string());
        }
        Ok(Self {
            resource: wire.resource,
            value_dependencies: wire.value_dependencies,
            explicit_dependencies: wire.explicit_dependencies,
            value_dependency_reads: wire.value_dependency_reads,
        })
    }
}

impl<S> From<ResolvedDataSource<S>> for ResolvedDataSourceWire<S> {
    fn from(resource: ResolvedDataSource<S>) -> Self {
        Self {
            resource: resource.resource,
            value_dependencies: resource.value_dependencies,
            explicit_dependencies: resource.explicit_dependencies,
            value_dependency_reads: resource.value_dependency_reads,
        }
    }
}

impl<S> AsRef<DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>> for ResolvedDataSource<S> {
    fn as_ref(&self) -> &DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()> {
        self.as_inner()
    }
}
