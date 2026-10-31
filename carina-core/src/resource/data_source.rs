//! `DataSource` — a read-only resource that is queried but not
//! managed.
//!
//! Part of the resource typestate split (#3169). A `DataSource`
//! carries the same fields as a [`Resource`](super::Resource)
//! minus `prefixes` (auto-generated names do not apply to read-only
//! lookups). `directives` is retained because `depends_on` and
//! `provider_instance` are still meaningful when ordering reads
//! against other resources.

use std::collections::{BTreeSet, HashSet};
use std::ops::Deref;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::{
    Directives, ModuleSource, ProviderModuleSource, ResourceId, ResourceIdentity,
    ResourceIdentityState, SavedModuleSource, Value,
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
#[serde(bound(serialize = "S: Serialize", deserialize = "S: Deserialize<'de>"))]
pub struct DataSource<S = ModuleSource> {
    pub id: ResourceId,
    /// Source-order preserving map of attribute name → expression.
    pub attributes: IndexMap<String, Value>,
    /// `directives` meta-argument block — `depends_on` and
    /// `provider_instance` are meaningful for data sources too.
    #[serde(default)]
    pub directives: Directives,
    /// Binding name from `let` bindings in DSL.
    #[serde(default)]
    pub binding: Option<String>,
    /// Binding names this data source depends on.
    #[serde(default)]
    pub dependency_bindings: BTreeSet<String>,
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
pub type SavedDataSource = DataSource<SavedModuleSource>;

/// Source-neutral data-source payload exposed to providers.
pub type ProviderDataSource = DataSource<ProviderModuleSource>;

impl From<DataSource> for SavedDataSource {
    fn from(data_source: DataSource) -> Self {
        let DataSource {
            id,
            attributes,
            directives,
            binding,
            dependency_bindings,
            module_source,
            quoted_string_attrs,
        } = data_source;
        Self {
            id,
            attributes,
            directives,
            binding,
            dependency_bindings,
            module_source: module_source.as_ref().map(SavedModuleSource::from),
            quoted_string_attrs,
        }
    }
}

impl DataSource {
    /// Create a pending data source with an empty attribute map.
    pub fn pending(resource_type: impl Into<String>) -> Self {
        Self {
            id: ResourceId::pending(resource_type),
            attributes: IndexMap::new(),
            directives: Directives::default(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
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
            dependency_bindings: BTreeSet::new(),
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
            dependency_bindings: BTreeSet::new(),
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
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }
}

impl<S: Clone> DataSource<S> {
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

impl<S> DataSource<S> {
    /// Remove module provenance while crossing into provider-owned code.
    pub(crate) fn into_provider_data_source(self) -> ProviderDataSource {
        let DataSource {
            id,
            attributes,
            directives,
            binding,
            dependency_bindings,
            module_source: _,
            quoted_string_attrs,
        } = self;
        ProviderDataSource {
            id,
            attributes,
            directives,
            binding,
            dependency_bindings,
            module_source: None,
            quoted_string_attrs,
        }
    }

    /// Get an attribute value by key.
    pub fn get_attr(&self, key: &str) -> Option<&Value> {
        self.attributes.get(key)
    }

    /// Set an attribute value.
    pub fn set_attr(&mut self, key: impl Into<String>, value: Value) {
        self.attributes.insert(key.into(), value);
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    pub fn with_binding(mut self, binding: impl Into<String>) -> Self {
        self.binding = Some(binding.into());
        self
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
#[serde(try_from = "DataSource<S>", into = "DataSource<S>")]
pub struct ResolvedDataSource<S = ModuleSource> {
    inner: DataSource<S>,
    explicit_dependencies: crate::dependency::ResolvedDependencies,
}

pub type SavedResolvedDataSource = ResolvedDataSource<SavedModuleSource>;

impl From<ResolvedDataSource> for SavedResolvedDataSource {
    fn from(resource: ResolvedDataSource) -> Self {
        let (resource, explicit_dependencies) = resource.into_parts();
        Self::from_resolved_parts(resource.into(), explicit_dependencies)
    }
}

impl<S> ResolvedDataSource<S> {
    /// Construct from a [`DataSource`], panicking if identity is pending.
    pub fn new(resource: DataSource<S>) -> Self {
        assert!(
            matches!(
                resource.id.identity_state(),
                ResourceIdentityState::Resolved(_)
            ),
            "ResolvedDataSource requires identity"
        );
        assert!(
            resource.directives.depends_on.is_empty(),
            "ResolvedDataSource with depends_on requires dependency resolution"
        );
        Self::from_resolved_parts(resource, crate::dependency::ResolvedDependencies::empty())
    }

    fn from_resolved_parts(
        resource: DataSource<S>,
        explicit_dependencies: crate::dependency::ResolvedDependencies,
    ) -> Self {
        Self {
            inner: resource,
            explicit_dependencies,
        }
    }

    pub fn new_with_dependencies(
        resource: DataSource<S>,
        explicit_dependencies: crate::dependency::ResolvedDependencies,
    ) -> Result<Self, crate::dependency::DependencyResolutionError> {
        let owner = format!(
            "directives.depends_on on '{}'",
            resource.binding.as_deref().unwrap_or("(anonymous)")
        );
        let authored = resource
            .directives
            .depends_on
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        if authored != explicit_dependencies.names() {
            return Err(crate::dependency::DependencyResolutionError::MetadataMismatch { owner });
        }
        if matches!(
            resource.id.identity_state(),
            ResourceIdentityState::Pending(_)
        ) {
            return Err(
                crate::dependency::DependencyResolutionError::PendingTargetIdentity {
                    binding: resource
                        .binding
                        .clone()
                        .unwrap_or_else(|| resource.id.to_string()),
                },
            );
        }
        Ok(Self::from_resolved_parts(resource, explicit_dependencies))
    }

    /// Try to construct; returns `None` if identity is pending.
    pub fn try_new(resource: DataSource<S>) -> Option<Self> {
        match resource.id.identity_state() {
            ResourceIdentityState::Pending(_) => None,
            ResourceIdentityState::Resolved(_) => Some(Self::new(resource)),
        }
    }

    /// Borrow the inner [`DataSource`].
    pub fn as_inner(&self) -> &DataSource<S> {
        &self.inner
    }

    /// Consume and return the inner [`DataSource`].
    pub fn into_inner(self) -> DataSource<S> {
        self.inner
    }

    pub fn into_parts(self) -> (DataSource<S>, crate::dependency::ResolvedDependencies) {
        (self.inner, self.explicit_dependencies)
    }

    pub fn explicit_dependencies(&self) -> &crate::dependency::ResolvedDependencies {
        &self.explicit_dependencies
    }

    /// Rewrap an attribute-transformed data source without discarding the
    /// resolver evidence carried by this value.
    pub(crate) fn with_transformed_inner(&self, resource: DataSource<S>) -> Self {
        Self::new_with_dependencies(resource, self.explicit_dependencies.clone())
            .expect("a transformed resolved data source must preserve identity and depends_on")
    }

    pub fn identity(&self) -> &ResourceIdentity {
        match self.inner.id.identity_state() {
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
    type Target = DataSource<S>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<S> From<ResolvedDataSource<S>> for DataSource<S> {
    fn from(resource: ResolvedDataSource<S>) -> Self {
        resource.inner
    }
}

impl<S> TryFrom<DataSource<S>> for ResolvedDataSource<S> {
    type Error = String;

    fn try_from(resource: DataSource<S>) -> Result<Self, Self::Error> {
        if !resource.directives.depends_on.is_empty() {
            return Err("depends_on requires dependency resolution".to_string());
        }
        Self::try_new(resource).ok_or_else(|| "identity is required".to_string())
    }
}

impl<S> AsRef<DataSource<S>> for ResolvedDataSource<S> {
    fn as_ref(&self) -> &DataSource<S> {
        self.as_inner()
    }
}
