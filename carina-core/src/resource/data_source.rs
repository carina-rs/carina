//! `DataSource` — a read-only resource that is queried but not
//! managed.
//!
//! Part of the resource typestate split (#3169). A `DataSource`
//! carries the same fields as a [`ManagedResource`](super::ManagedResource)
//! minus `prefixes` (auto-generated names do not apply to read-only
//! lookups). `directives` is retained because `depends_on` and
//! `provider_instance` are still meaningful when ordering reads
//! against other resources.

use std::collections::{BTreeSet, HashSet};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::{
    Directives, ModuleSource, Resource, ResourceId, ResourceKind, ResourceKindLabel,
    ResourceKindMismatch, Value,
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
    /// of state — mirrors [`Resource::quoted_string_attrs`].
    #[serde(default, skip)]
    pub quoted_string_attrs: HashSet<String>,
}

impl DataSource {
    /// Create a data source with an empty attribute map.
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: ResourceId::new(resource_type, name),
            attributes: IndexMap::new(),
            directives: Directives::default(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }

    /// Create a data source with a provider-qualified id.
    pub fn with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        name: impl Into<String>,
        provider_instance: Option<String>,
    ) -> Self {
        Self {
            id: ResourceId::with_provider(provider, resource_type, name, provider_instance),
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

impl TryFrom<&Resource> for DataSource {
    type Error = ResourceKindMismatch;

    fn try_from(res: &Resource) -> Result<Self, Self::Error> {
        match res.kind {
            ResourceKind::DataSource => Ok(Self {
                id: res.id.clone(),
                attributes: res.attributes.clone(),
                directives: res.directives.clone(),
                binding: res.binding.clone(),
                dependency_bindings: res.dependency_bindings.clone(),
                module_source: res.module_source.clone(),
                quoted_string_attrs: res.quoted_string_attrs.clone(),
            }),
            _ => Err(ResourceKindMismatch {
                expected: ResourceKindLabel::DataSource,
                actual: res.kind.label(),
            }),
        }
    }
}

/// Owned-`Resource` convenience over [`TryFrom<&Resource>`]. Symmetric
/// with the `ManagedResource` impl; removed with the other transitional
/// bridges when #3181 inline-merges `Resource`.
impl TryFrom<Resource> for DataSource {
    type Error = ResourceKindMismatch;

    fn try_from(res: Resource) -> Result<Self, Self::Error> {
        Self::try_from(&res)
    }
}

/// Transitional bridge — rebuild a legacy [`Resource`] from a
/// `DataSource`. Symmetric with [`From<&ManagedResource> for Resource`]
/// in `managed.rs`; removed alongside it when #3181 inline-merges
/// `Resource` into the typestate structs.
///
/// `prefixes` is reconstructed empty — `DataSource` drops the field as a
/// compile-time invariant (auto-generated names do not apply to
/// read-only lookups), and a `Resource` synthesized from a `DataSource`
/// only flows into `Effect::Read { resource }` whose downstream
/// consumers (executor read path) do not read `prefixes`.
impl From<&DataSource> for Resource {
    fn from(d: &DataSource) -> Self {
        Self {
            id: d.id.clone(),
            attributes: d.attributes.clone(),
            kind: ResourceKind::DataSource,
            directives: d.directives.clone(),
            prefixes: std::collections::HashMap::new(),
            binding: d.binding.clone(),
            dependency_bindings: d.dependency_bindings.clone(),
            module_source: d.module_source.clone(),
            quoted_string_attrs: d.quoted_string_attrs.clone(),
            virtual_module: None,
        }
    }
}
