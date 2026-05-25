//! `Composition` — a synthetic IR node created by the module
//! resolver to expose module `attributes` values.
//!
//! Part of the resource typestate split (#3169). Virtual resources
//! are not sent to providers; they exist only in the IR. The
//! `signature.attributes` map may contain unresolved
//! `ResourceRef` / `BindingRef` values whose resolution is **deferred
//! to the post-apply path**. The typestate split encodes that
//! invariant: a `Composition` is never accepted by the pre-apply
//! resolver.
//!
//! Unlike [`Resource`](super::Resource), this struct
//! does not carry `directives` (no `prevent_destroy` applies to a
//! synthetic node) or `prefixes` (no auto-generated names on a
//! non-provider resource). `module_source` is flattened to
//! `module_name` + `instance` — those are always set for compositions.

use std::collections::{BTreeSet, HashSet};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::{ResourceId, Value};

/// The function-shaped I/O surface of a [`Composition`].
///
/// Carries both halves of the module-call boundary on the expanded
/// node itself:
///
/// - **`arguments`**: resolved call-site values, populated from
///   `ModuleCall.arguments` at expansion time. Today these are read
///   for substitution and then dropped with the `ModuleCall`;
///   keeping them on the composition makes "what was passed in"
///   inspectable post-expansion.
/// - **`attributes`**: resolved module-output values (the
///   `attribute_params` resolved against `arguments`). May still
///   carry unresolved `ResourceRef` / `BindingRef` values whose
///   resolution is deferred until post-apply.
///
/// `Signature` is intentionally *only* on `Composition`: the DSL
/// gives `Resource` and `DataSource` a single user-written
/// `attributes` namespace with no `arguments` concept, so embedding a
/// `Signature` on those structs would be a pretextual abstraction
/// with one populated half. See the rescoped design note
/// (`notes/specs/2026-05-25-composition-graph-node-design.md`, PR
/// #3301) for the rationale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signature {
    /// Resolved call-site arguments. Empty when the composition was
    /// produced by a module that does not declare any `argument`
    /// parameters, or when the call site passed no arguments.
    #[serde(default)]
    pub arguments: IndexMap<String, Value>,
    /// Resolved module-output values. May contain unresolved
    /// `ResourceRef` / `BindingRef` values; resolution is deferred to
    /// the post-apply path.
    #[serde(default)]
    pub attributes: IndexMap<String, Value>,
}

/// A composition resource created by module-call expansion.
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
/// use carina_core::resource::Composition;
/// fn _f(v: &Composition) -> &std::collections::HashMap<String, String> {
///     &v.prefixes
/// }
/// ```
///
/// `directives` is dropped (no `prevent_destroy` applies to a synthetic node):
///
/// ```compile_fail
/// use carina_core::resource::Composition;
/// fn _f(v: &Composition) -> &carina_core::resource::Directives {
///     &v.directives
/// }
/// ```
///
/// `module_source` is dropped — module metadata is flattened into
/// `module_name` + `instance`:
///
/// ```compile_fail
/// use carina_core::resource::Composition;
/// fn _f(v: &Composition) -> &Option<carina_core::resource::ModuleSource> {
///     &v.module_source
/// }
/// ```
///
/// The bare `attributes` field is dropped — the I/O surface lives on
/// `signature` (PR #3292). Direct access through `&v.attributes` must
/// no longer compile; consumers should use `&v.signature.attributes`
/// or, where polymorphism is needed, the
/// [`ResourceLike::attributes`](super::ResourceLike::attributes)
/// accessor.
///
/// ```compile_fail
/// use carina_core::resource::Composition;
/// use indexmap::IndexMap;
/// fn _f(v: &Composition) -> &IndexMap<String, carina_core::resource::Value> {
///     &v.attributes
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Composition {
    pub id: ResourceId,
    /// I/O surface of this composition: resolved call-site arguments
    /// + resolved module-output attributes. See [`Signature`].
    #[serde(default, flatten)]
    pub signature: Signature,
    /// Binding name from `let` bindings in DSL.
    #[serde(default)]
    pub binding: Option<String>,
    /// Binding names this composition depends on.
    #[serde(default)]
    pub dependency_bindings: BTreeSet<String>,
    /// Module name from the originating module-call expansion
    /// (e.g. "web_tier"). Always set for compositions — see #2516.
    pub module_name: String,
    /// Module instance binding name (e.g. "web").
    pub instance: String,
    /// Parser-level: attributes whose value was written as a quoted
    /// string literal. Parse-time only; `#[serde(skip)]` keeps it out
    /// of state — mirrors [`Resource::quoted_string_attrs`](super::Resource).
    #[serde(default, skip)]
    pub quoted_string_attrs: HashSet<String>,
}
