//! `VirtualResource` — a synthetic IR node created by the module
//! resolver to expose module `attributes` values.
//!
//! Part of the resource typestate split (#3169). Virtual resources
//! are not sent to providers; they exist only in the IR. Their
//! `attributes` may contain unresolved `ResourceRef` / `BindingRef`
//! values whose resolution is **deferred to the post-apply path**.
//! The typestate split encodes that invariant: a `VirtualResource`
//! is never accepted by the pre-apply resolver.
//!
//! Unlike [`ManagedResource`](super::ManagedResource), this struct
//! does not carry `directives` (no `prevent_destroy` applies to a
//! synthetic node) or `prefixes` (no auto-generated names on a
//! non-provider resource). `module_source` is flattened to
//! `module_name` + `instance` — those are always set for virtuals.

use std::collections::{BTreeSet, HashSet};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::{Resource, ResourceId, ResourceKind, ResourceKindLabel, ResourceKindMismatch, Value};

/// A virtual resource created by module-call expansion.
///
/// # Dropped fields (compile-time invariants)
///
/// These guards pin the design-doc invariants for #3169. If any of
/// these fields is re-added, the corresponding doctest compiles and
/// CI fails — re-read the design doc before doing so.
///
/// `prefixes` is dropped (no auto-generated names on a synthetic node):
///
/// ```compile_fail
/// use carina_core::resource::VirtualResource;
/// fn _f(v: &VirtualResource) -> &std::collections::HashMap<String, String> {
///     &v.prefixes
/// }
/// ```
///
/// `directives` is dropped (no `prevent_destroy` applies to a synthetic node):
///
/// ```compile_fail
/// use carina_core::resource::VirtualResource;
/// fn _f(v: &VirtualResource) -> &carina_core::resource::Directives {
///     &v.directives
/// }
/// ```
///
/// `module_source` is dropped — module metadata is flattened into
/// `module_name` + `instance`:
///
/// ```compile_fail
/// use carina_core::resource::VirtualResource;
/// fn _f(v: &VirtualResource) -> &Option<carina_core::resource::ModuleSource> {
///     &v.module_source
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VirtualResource {
    pub id: ResourceId,
    /// Attributes that may contain unresolved `ResourceRef` /
    /// `BindingRef` values. Resolution is deferred until post-apply.
    pub attributes: IndexMap<String, Value>,
    /// Binding name from `let` bindings in DSL.
    #[serde(default)]
    pub binding: Option<String>,
    /// Binding names this virtual depends on.
    #[serde(default)]
    pub dependency_bindings: BTreeSet<String>,
    /// Module name from the originating `ResourceKind::Virtual` tag
    /// (e.g. "web_tier"). Always set for virtuals — see #2516.
    pub module_name: String,
    /// Module instance binding name (e.g. "web").
    pub instance: String,
    /// Parser-level: attributes whose value was written as a quoted
    /// string literal. Parse-time only; `#[serde(skip)]` keeps it out
    /// of state — mirrors [`Resource::quoted_string_attrs`].
    #[serde(default, skip)]
    pub quoted_string_attrs: HashSet<String>,
}

impl TryFrom<&Resource> for VirtualResource {
    type Error = ResourceKindMismatch;

    fn try_from(res: &Resource) -> Result<Self, Self::Error> {
        match (&res.kind, &res.virtual_module) {
            (ResourceKind::Virtual, Some((module_name, instance))) => Ok(Self {
                id: res.id.clone(),
                attributes: res.attributes.clone(),
                binding: res.binding.clone(),
                dependency_bindings: res.dependency_bindings.clone(),
                module_name: module_name.clone(),
                instance: instance.clone(),
                quoted_string_attrs: res.quoted_string_attrs.clone(),
            }),
            (ResourceKind::Virtual, None) => Err(ResourceKindMismatch {
                // Inconsistent: kind says Virtual but virtual_module is None.
                // Treat as the kind label mismatch since the data is missing.
                expected: ResourceKindLabel::Virtual,
                actual: ResourceKindLabel::Virtual,
            }),
            (other, _) => Err(ResourceKindMismatch {
                expected: ResourceKindLabel::Virtual,
                actual: other.label(),
            }),
        }
    }
}

/// Transitional bridge — rebuild a legacy [`Resource`] from a
/// `VirtualResource`. Symmetric with [`From<&ManagedResource> for Resource`]
/// in `managed.rs` and [`From<&DataSource> for Resource`] in
/// `data_source.rs`; removed alongside them when #3181 inline-merges
/// `Resource` into the typestate structs.
///
/// `directives` / `prefixes` are reconstructed empty — `VirtualResource`
/// drops both fields as compile-time invariants (no `prevent_destroy` and
/// no auto-generated names apply to a synthetic node). The flattened
/// `module_name` + `instance` pair is restored into `virtual_module`.
impl From<&VirtualResource> for Resource {
    fn from(v: &VirtualResource) -> Self {
        Self {
            id: v.id.clone(),
            attributes: v.attributes.clone(),
            kind: ResourceKind::Virtual,
            directives: super::Directives::default(),
            prefixes: std::collections::HashMap::new(),
            binding: v.binding.clone(),
            dependency_bindings: v.dependency_bindings.clone(),
            module_source: None,
            quoted_string_attrs: v.quoted_string_attrs.clone(),
            virtual_module: Some((v.module_name.clone(), v.instance.clone())),
        }
    }
}
