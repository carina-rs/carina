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
#[derive(Debug, Clone, PartialEq)]
pub struct DataSource {
    pub id: ResourceId,
    /// Source-order preserving map of attribute name → expression.
    pub attributes: IndexMap<String, Value>,
    /// `directives` meta-argument block — `depends_on` and
    /// `provider_instance` are meaningful for data sources too.
    pub directives: Directives,
    /// Binding name from `let` bindings in DSL.
    pub binding: Option<String>,
    /// Binding names this data source depends on.
    pub dependency_bindings: BTreeSet<String>,
    /// Module source info for data sources from modules.
    pub module_source: Option<ModuleSource>,
    /// Parser-level: attributes whose value was written as a quoted
    /// string literal.
    pub quoted_string_attrs: HashSet<String>,
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
