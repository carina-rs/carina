//! `ManagedResource` — a managed infrastructure resource with full
//! CRUD lifecycle.
//!
//! Part of the resource typestate split (#3169). Carries only the
//! fields meaningful for a managed lifecycle: identifier, attributes,
//! Carina-side directives, attribute prefixes, binding metadata,
//! module source, and the parser-level `quoted_string_attrs` bit.

use std::collections::{BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;

use super::{
    Directives, ModuleSource, Resource, ResourceId, ResourceKind, ResourceKindLabel,
    ResourceKindMismatch, Value,
};

/// A managed infrastructure resource.
///
/// Cf. [`VirtualResource`](super::VirtualResource) and
/// [`DataSource`](super::DataSource) for the other two arms of the
/// typestate split.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagedResource {
    pub id: ResourceId,
    /// Source-order preserving map of attribute name → expression.
    /// Fully resolvable at pre-apply (no deferred virtual-only refs).
    pub attributes: IndexMap<String, Value>,
    /// `directives` meta-argument block.
    pub directives: Directives,
    /// Attribute prefixes (e.g. `bucket_name_prefix = "my-app-"`).
    pub prefixes: HashMap<String, String>,
    /// Binding name from `let` bindings in DSL.
    pub binding: Option<String>,
    /// Binding names of resources this resource depends on.
    pub dependency_bindings: BTreeSet<String>,
    /// Module source info for resources that belong to a module.
    pub module_source: Option<ModuleSource>,
    /// Parser-level: attributes whose value was written as a quoted
    /// string literal (`attr = "..."`).
    pub quoted_string_attrs: HashSet<String>,
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
