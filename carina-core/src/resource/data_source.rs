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

use super::{Directives, ModuleSource, ResourceId, Value, identity_if_present};

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataSource {
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
    pub module_source: Option<ModuleSource>,
    /// Parser-level: attributes whose value was written as a quoted
    /// string literal. Parse-time only; `#[serde(skip)]` keeps it out
    /// of state — mirrors [`Resource::quoted_string_attrs`](super::Resource).
    #[serde(default, skip)]
    pub quoted_string_attrs: HashSet<String>,
}

impl DataSource {
    /// Create a data source with an empty attribute map.
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: ResourceId::new(resource_type, identity_if_present(name)),
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
        name: impl Into<String>,
        provider_instance: Option<String>,
    ) -> Self {
        Self {
            id: ResourceId::with_provider(
                provider,
                resource_type,
                identity_if_present(name),
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
/// let resource = DataSource::new("test", "");
/// let _ = ResolvedDataSource(resource);
/// ```
///
/// ```compile_fail
/// use carina_core::resource::{DataSource, ResolvedDataSource};
///
/// let resource = DataSource::new("test", "");
/// let _: ResolvedDataSource = resource.into();
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "DataSource", into = "DataSource")]
pub struct ResolvedDataSource(DataSource);

impl ResolvedDataSource {
    /// Construct from a [`DataSource`], panicking if identity is `None`.
    pub fn new(resource: DataSource) -> Self {
        assert!(
            resource.id.identity.is_some(),
            "ResolvedDataSource requires identity"
        );
        Self(resource)
    }

    /// Try to construct; returns `None` if identity is absent.
    pub fn try_new(resource: DataSource) -> Option<Self> {
        if resource.id.identity.is_some() {
            Some(Self(resource))
        } else {
            None
        }
    }

    /// Borrow the inner [`DataSource`].
    pub fn as_inner(&self) -> &DataSource {
        &self.0
    }

    /// Consume and return the inner [`DataSource`].
    pub fn into_inner(self) -> DataSource {
        self.0
    }
}

impl Deref for ResolvedDataSource {
    type Target = DataSource;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<ResolvedDataSource> for DataSource {
    fn from(resource: ResolvedDataSource) -> Self {
        resource.0
    }
}

impl TryFrom<DataSource> for ResolvedDataSource {
    type Error = String;

    fn try_from(resource: DataSource) -> Result<Self, Self::Error> {
        Self::try_new(resource).ok_or_else(|| "identity is required".to_string())
    }
}

impl AsRef<DataSource> for ResolvedDataSource {
    fn as_ref(&self) -> &DataSource {
        self.as_inner()
    }
}
