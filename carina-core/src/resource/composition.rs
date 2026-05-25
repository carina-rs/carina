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

use super::{AccessPath, DeferredValue, ResourceId, Value};

/// How a [`Composition`]'s attribute is produced from the rest of the
/// IR.
///
/// Pre-#3294 every composition attribute was a single `Value`, and the
/// resolver had to inspect the variant at runtime to decide whether it
/// was a single-hop alias (`Value::Deferred(DeferredValue::ResourceRef
/// { path })`) or a multi-source expression
/// (`Value::Deferred(DeferredValue::Interpolation { ... })`,
/// `FunctionCall`, etc.). Splitting that decision into a tagged enum
/// removes the runtime classification and matches the way the
/// post-apply resolver actually consumes them.
///
/// - **`Forwarded(path)`**: this attribute is the same value as the
///   attribute reachable through `path` on another node. The resolver
///   evaluates the path one hop at post-apply time; display can fold
///   the alias under its target; dependency analysis adds one edge.
/// - **`Derived(value)`**: this attribute is a multi-source expression
///   (interpolation, function call, arithmetic, literal). The
///   resolver evaluates the `Value` against post-apply state, which
///   may itself contain nested refs.
///
/// `AccessPath` is used in `Forwarded` rather than `NodeId` because at
/// expansion time the `NodeId` of the target may not yet be bound —
/// the path carries a binding name + attribute path that the resolver
/// already knows how to look up. A future PR can lift this to
/// `Forwarded(NodeId, AttrPath)` once a name → `NodeId` index is
/// available at expansion time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompositionAttribute {
    /// Single-hop alias to another node's attribute, by path.
    Forwarded(AccessPath),
    /// Multi-source expression: a literal, interpolation, function
    /// call, or any other `Value` shape that is not a bare
    /// single-hop reference.
    Derived(Value),
}

impl CompositionAttribute {
    /// Classify a `Value` into the appropriate
    /// [`CompositionAttribute`] variant.
    ///
    /// `Value::Deferred(DeferredValue::ResourceRef { path })` is a
    /// single-hop alias and lifts into [`Forwarded`](Self::Forwarded).
    /// Every other `Value` shape is multi-source (literal,
    /// interpolation, function call, etc.) and lifts into
    /// [`Derived`](Self::Derived).
    pub fn from_value(value: Value) -> Self {
        match value {
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                CompositionAttribute::Forwarded(path)
            }
            other => CompositionAttribute::Derived(other),
        }
    }

    /// Reify back into a [`Value`] for callers that have not yet been
    /// migrated to the new typed-variant dispatch.
    ///
    /// The post-apply resolver, plan display, and exporters consume
    /// composition attributes as `Value`s today; this lets PR G ship
    /// the type-level split without rewriting every consumer in one
    /// commit. Each subsequent migration replaces a `.to_value()` site
    /// with a direct match on `CompositionAttribute`.
    pub fn to_value(&self) -> Value {
        match self {
            CompositionAttribute::Forwarded(path) => {
                Value::Deferred(DeferredValue::ResourceRef { path: path.clone() })
            }
            CompositionAttribute::Derived(v) => v.clone(),
        }
    }
}

impl From<Value> for CompositionAttribute {
    fn from(v: Value) -> Self {
        Self::from_value(v)
    }
}

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
    /// Module-output values classified by how they are produced
    /// (#3294): [`Forwarded`](CompositionAttribute::Forwarded) for
    /// single-hop aliases, [`Derived`](CompositionAttribute::Derived)
    /// for multi-source expressions. The resolver dispatches on the
    /// variant at post-apply time.
    #[serde(default)]
    pub attributes: IndexMap<String, CompositionAttribute>,
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

impl Composition {
    /// The composition's id wrapped as an [`EphemeralId`](super::EphemeralId).
    ///
    /// `Composition` is plan-scoped and never persists in state, so its
    /// id is `EphemeralId`-typed. By construction this id cannot enter
    /// state-load APIs that take `&PersistentId` — that mismatch is a
    /// compile error.
    pub fn ephemeral_id(&self) -> super::EphemeralId {
        super::EphemeralId::new(self.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ConcreteValue;

    #[test]
    fn from_value_resource_ref_classifies_as_forwarded() {
        let path = AccessPath::new("role", "arn");
        let v = Value::Deferred(DeferredValue::ResourceRef { path: path.clone() });
        let attr = CompositionAttribute::from_value(v);
        match attr {
            CompositionAttribute::Forwarded(p) => assert_eq!(p, path),
            CompositionAttribute::Derived(_) => panic!("ResourceRef must lift to Forwarded"),
        }
    }

    #[test]
    fn from_value_concrete_string_classifies_as_derived() {
        let v = Value::Concrete(ConcreteValue::String("literal".to_string()));
        let attr = CompositionAttribute::from_value(v.clone());
        match attr {
            CompositionAttribute::Derived(d) => assert_eq!(d, v),
            CompositionAttribute::Forwarded(_) => panic!("literal must lift to Derived"),
        }
    }

    #[test]
    fn from_value_interpolation_classifies_as_derived() {
        use crate::resource::InterpolationPart;
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".to_string()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("svc", "id"),
            })),
        ]));
        let attr = CompositionAttribute::from_value(v.clone());
        match attr {
            CompositionAttribute::Derived(d) => assert_eq!(d, v),
            CompositionAttribute::Forwarded(_) => {
                panic!("multi-source interpolation must lift to Derived")
            }
        }
    }

    #[test]
    fn forwarded_to_value_is_resource_ref() {
        let path = AccessPath::new("svc", "endpoint");
        let attr = CompositionAttribute::Forwarded(path.clone());
        let v = attr.to_value();
        assert_eq!(
            v,
            Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
        );
    }

    #[test]
    fn derived_to_value_returns_inner() {
        let inner = Value::Concrete(ConcreteValue::String("kept".to_string()));
        let attr = CompositionAttribute::Derived(inner.clone());
        assert_eq!(attr.to_value(), inner);
    }

    /// Round-trip: a `Value` → `CompositionAttribute` → `Value` is
    /// lossless for both `Forwarded`-lifted refs and `Derived`-wrapped
    /// expressions. This is the invariant the resolver / display
    /// layers rely on while migrating to per-variant dispatch.
    #[test]
    fn from_value_to_value_round_trip() {
        let cases = vec![
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("a", "b"),
            }),
            Value::Concrete(ConcreteValue::String("literal".to_string())),
            Value::Concrete(ConcreteValue::Int(42)),
        ];
        for original in cases {
            let lifted = CompositionAttribute::from_value(original.clone());
            assert_eq!(lifted.to_value(), original);
        }
    }
}
