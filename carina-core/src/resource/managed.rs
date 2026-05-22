//! `ManagedResource` — a managed infrastructure resource with full
//! CRUD lifecycle.
//!
//! Part of the resource typestate split (#3169). Carries only the
//! fields meaningful for a managed lifecycle: identifier, attributes,
//! Carina-side directives, attribute prefixes, binding metadata,
//! module source, and the parser-level `quoted_string_attrs` bit.

use std::collections::{BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::{
    Directives, ModuleSource, Resource, ResourceId, ResourceKind, ResourceKindLabel,
    ResourceKindMismatch, Value,
};

/// A managed infrastructure resource.
///
/// Cf. [`VirtualResource`](super::VirtualResource) and
/// [`DataSource`](super::DataSource) for the other two arms of the
/// typestate split.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedResource {
    pub id: ResourceId,
    /// Source-order preserving map of attribute name → expression.
    /// Fully resolvable at pre-apply (no deferred virtual-only refs).
    pub attributes: IndexMap<String, Value>,
    /// `directives` meta-argument block.
    #[serde(default)]
    pub directives: Directives,
    /// Attribute prefixes (e.g. `bucket_name_prefix = "my-app-"`).
    #[serde(default)]
    pub prefixes: HashMap<String, String>,
    /// Binding name from `let` bindings in DSL.
    #[serde(default)]
    pub binding: Option<String>,
    /// Binding names of resources this resource depends on.
    #[serde(default)]
    pub dependency_bindings: BTreeSet<String>,
    /// Module source info for resources that belong to a module.
    #[serde(default)]
    pub module_source: Option<ModuleSource>,
    /// Parser-level: attributes whose value was written as a quoted
    /// string literal (`attr = "..."`). Parse-time only; `#[serde(skip)]`
    /// keeps it out of state — mirrors [`Resource::quoted_string_attrs`].
    #[serde(default, skip)]
    pub quoted_string_attrs: HashSet<String>,
}

impl ManagedResource {
    /// Create a managed resource with an empty attribute map.
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: ResourceId::new(resource_type, name),
            attributes: IndexMap::new(),
            directives: Directives::default(),
            prefixes: HashMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }

    /// Create a managed resource with a provider-qualified id.
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
            prefixes: HashMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }

    /// Returns the attributes projected to a `HashMap<String, Value>`.
    pub fn resolved_attributes(&self) -> HashMap<String, Value> {
        super::attrs_to_hashmap(&self.attributes)
    }

    /// Get an attribute value by key.
    pub fn get_attr(&self, key: &str) -> Option<&Value> {
        self.attributes.get(key)
    }

    /// Get a mutable attribute value by key.
    pub fn get_attr_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.attributes.get_mut(key)
    }

    /// Set an attribute value.
    pub fn set_attr(&mut self, key: impl Into<String>, value: Value) {
        self.attributes.insert(key.into(), value);
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    /// Set attributes from a `HashMap<String, Value>`.
    pub fn with_value_attributes(mut self, attrs: HashMap<String, Value>) -> Self {
        self.attributes = attrs.into_iter().collect();
        self
    }

    pub fn with_binding(mut self, binding: impl Into<String>) -> Self {
        self.binding = Some(binding.into());
        self
    }

    pub fn with_dependency_bindings(mut self, deps: BTreeSet<String>) -> Self {
        self.dependency_bindings = deps;
        self
    }

    pub fn with_module_source(mut self, source: ModuleSource) -> Self {
        self.module_source = Some(source);
        self
    }
}

impl TryFrom<&Resource> for ManagedResource {
    type Error = ResourceKindMismatch;

    fn try_from(res: &Resource) -> Result<Self, Self::Error> {
        match res.kind {
            ResourceKind::Managed => Ok(Self {
                id: res.id.clone(),
                attributes: res.attributes.clone(),
                directives: res.directives.clone(),
                prefixes: res.prefixes.clone(),
                binding: res.binding.clone(),
                dependency_bindings: res.dependency_bindings.clone(),
                module_source: res.module_source.clone(),
                quoted_string_attrs: res.quoted_string_attrs.clone(),
            }),
            _ => Err(ResourceKindMismatch {
                expected: ResourceKindLabel::Managed,
                actual: res.kind.label(),
            }),
        }
    }
}

/// Owned-`Resource` convenience over [`TryFrom<&Resource>`]. Lets call
/// sites that hold an owned legacy `Resource` write `resource.try_into()`
/// without an explicit borrow. Removed with the rest of the transitional
/// bridges when #3181 inline-merges `Resource` into `ManagedResource`.
impl TryFrom<Resource> for ManagedResource {
    type Error = ResourceKindMismatch;

    fn try_from(res: Resource) -> Result<Self, Self::Error> {
        Self::try_from(&res)
    }
}

/// Transitional bridge — rebuild a legacy [`Resource`] from a
/// `ManagedResource`. Co-located with the reverse `TryFrom<&Resource>`
/// impl above so #3181 can remove both directions in one place when
/// `Resource` is inline-merged into `ManagedResource`.
impl From<&ManagedResource> for Resource {
    fn from(m: &ManagedResource) -> Self {
        Self {
            id: m.id.clone(),
            attributes: m.attributes.clone(),
            kind: ResourceKind::Managed,
            directives: m.directives.clone(),
            prefixes: m.prefixes.clone(),
            binding: m.binding.clone(),
            dependency_bindings: m.dependency_bindings.clone(),
            module_source: m.module_source.clone(),
            quoted_string_attrs: m.quoted_string_attrs.clone(),
            virtual_module: None,
        }
    }
}
