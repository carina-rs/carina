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

impl ManagedResource {
    /// Project a legacy [`Resource`] onto a `ManagedResource` view
    /// **regardless of `kind`**, discarding the discriminant.
    ///
    /// Unlike the strict [`TryFrom<&Resource>`] impl above, this
    /// helper does not check `kind`. It exists for one specific
    /// caller (`carina-cli`'s `resolve_exports`) where the
    /// bindings-index construction needs to treat
    /// `ResourceKind::DataSource` as having the same attribute /
    /// state shape as a managed resource: the binding index only
    /// indexes by `binding` name + attribute map, and DataSources
    /// participate in that lookup just like Managed resources do.
    /// Routing both through the same `ManagedResource` view keeps
    /// the binding-index API single-typed without forcing the
    /// caller to spread `match kind` across the bridge.
    ///
    /// `Virtual` resources must **not** be passed here — the
    /// caller has already split them out (the typestate invariant
    /// that virtuals are post-apply-only forbids them participating
    /// in the pre-apply / managed binding view). Passing a virtual
    /// drops it to a Managed view, which would re-introduce the
    /// class of bug #3169 fixed. Callers that observe a virtual
    /// must route it through [`VirtualResource::try_from`] instead.
    ///
    /// Removed when #3181 inline-merges `Resource` into
    /// `ManagedResource`.
    pub fn as_managed_view(r: &Resource) -> Self {
        debug_assert!(
            !matches!(r.kind, ResourceKind::Virtual { .. }),
            "as_managed_view called on Virtual; route via VirtualResource::try_from",
        );
        Self {
            id: r.id.clone(),
            attributes: r.attributes.clone(),
            directives: r.directives.clone(),
            prefixes: r.prefixes.clone(),
            binding: r.binding.clone(),
            dependency_bindings: r.dependency_bindings.clone(),
            module_source: r.module_source.clone(),
            quoted_string_attrs: r.quoted_string_attrs.clone(),
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
        }
    }
}
