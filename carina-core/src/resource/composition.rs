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
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::parser::{TypeExpr, ValidateExpr};

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
enum CompositionAttributeValue {
    /// Single-hop alias to another node's attribute, by path.
    Forwarded(AccessPath),
    /// Multi-source expression: a literal, interpolation, function
    /// call, or any other `Value` shape that is not a bare
    /// single-hop reference.
    Derived(Value),
}

/// One module output together with its declared boundary type.
///
/// Keeping the optional annotation in the same entry as the classified value
/// makes it impossible for expansion or later value rewrites to create an
/// output without deciding what happens to its declaration. The annotation is
/// validation-only and is skipped during serialization so the saved-plan wire
/// representation remains byte-shape compatible with the pre-#3798 enum.
/// Compositions are never persisted in state files; saved-plan execution has
/// already passed validation and therefore does not need this metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CompositionAttribute {
    value: CompositionAttributeValue,
    #[serde(skip)]
    declared_type: Option<TypeExpr>,
    /// The module argument directly forwarded by this output before call-site
    /// substitution. Validation checks that declaration in the module's own
    /// scope; retaining the provenance prevents an expanded caller value from
    /// being checked again and blamed on the output declaration.
    #[serde(skip)]
    source_argument: Option<String>,
}

impl CompositionAttribute {
    /// Classify a `Value` into the appropriate
    /// [`CompositionAttribute`] variant.
    ///
    /// `Value::Deferred(DeferredValue::ResourceRef { path })` is a
    /// single-hop alias and is classified as forwarded.
    /// Every other `Value` shape is multi-source (literal,
    /// interpolation, function call, etc.) and lifts into
    /// classified as derived.
    pub fn from_value(value: Value, declared_type: Option<TypeExpr>) -> Self {
        let value = match value {
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                CompositionAttributeValue::Forwarded(path)
            }
            other => CompositionAttributeValue::Derived(other),
        };
        Self {
            value,
            declared_type,
            source_argument: None,
        }
    }

    /// The module-boundary annotation carried from `attributes {}`.
    pub fn declared_type(&self) -> Option<&TypeExpr> {
        self.declared_type.as_ref()
    }

    /// The forwarded path when this output is a single-hop alias.
    pub fn forwarded_path(&self) -> Option<&AccessPath> {
        match &self.value {
            CompositionAttributeValue::Forwarded(path) => Some(path),
            CompositionAttributeValue::Derived(_) => None,
        }
    }

    pub fn source_argument(&self) -> Option<&str> {
        self.source_argument.as_deref()
    }

    pub(crate) fn with_source_argument(mut self, source_argument: Option<String>) -> Self {
        self.source_argument = source_argument;
        self
    }

    /// Reclassify a rewritten value while preserving its declaration.
    pub fn with_value(&self, value: Value) -> Self {
        Self::from_value(value, self.declared_type.clone())
            .with_source_argument(self.source_argument.clone())
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
        match &self.value {
            CompositionAttributeValue::Forwarded(path) => {
                Value::Deferred(DeferredValue::ResourceRef { path: path.clone() })
            }
            CompositionAttributeValue::Derived(v) => v.clone(),
        }
    }
}

/// One resolved module-call argument together with its declared boundary type.
///
/// The declaration is validation-only and deliberately travels with every
/// value rewrite. Keeping both fields private means callers cannot insert a
/// raw [`Value`] into [`Signature::arguments`]; every live construction site
/// must supply the declared type. The optional storage only keeps saved-plan
/// deserialization backward-compatible: skipped validation metadata
/// deserializes as `None`, while [`Self::from_value`] always stores `Some`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CompositionArgument {
    value: Value,
    #[serde(skip)]
    declared_type: Option<TypeExpr>,
}

impl CompositionArgument {
    pub fn from_value(value: Value, declared_type: TypeExpr) -> Self {
        Self {
            value,
            declared_type: Some(declared_type),
        }
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn declared_type(&self) -> Option<&TypeExpr> {
        self.declared_type.as_ref()
    }

    /// Rewrite a prefixed/substituted value without dropping its declaration.
    pub fn with_value(&self, value: Value) -> Self {
        Self {
            value,
            declared_type: self.declared_type.clone(),
        }
    }
}

/// Stable identity of one authored module constraint within its module.
///
/// The module instance is deliberately not part of this identifier: the same
/// declaration may be instantiated many times. Execution code pairs this ID
/// with [`Composition::instance`] when it needs an instance-wide identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModuleConstraintId(String);

impl ModuleConstraintId {
    pub fn argument_validation(argument: &str, ordinal: usize) -> Self {
        Self(format!("argument:{argument}:{ordinal}"))
    }

    pub fn require(ordinal: usize) -> Self {
        Self(format!("require:{ordinal}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A module constraint whose referenced arguments were not fully known at
/// expansion time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingModuleConstraint {
    pub id: ModuleConstraintId,
    pub expression: ValidateExpr,
    pub message: String,
}

impl PendingModuleConstraint {
    pub fn id(&self) -> &ModuleConstraintId {
        &self.id
    }

    pub fn expression(&self) -> &ValidateExpr {
        &self.expression
    }

    pub fn message(&self) -> &str {
        &self.message
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
    pub arguments: IndexMap<String, CompositionArgument>,
    /// Module-output values classified by how they are produced
    /// (#3294): forwarded values are single-hop aliases and derived values are
    /// multi-source expressions. Each entry also carries its optional declared
    /// [`TypeExpr`] for validation.
    #[serde(default)]
    pub attributes: IndexMap<String, CompositionAttribute>,
    /// Constraints deferred until all referenced argument values are known.
    #[serde(default)]
    pub pending_constraints: Vec<PendingModuleConstraint>,
}

/// Structural identity for one module call that produced a composition.
///
/// `instance` is retained for unambiguous internal identity, while
/// `binding` and `module_name` are the authored names used by diagnostics.
/// The optional source is the path from the corresponding `use` statement.
/// This metadata is validation-only and is never persisted in saved plans.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompositionCall {
    pub module_name: String,
    pub binding: Option<String>,
    pub instance: String,
    pub module_source: Option<String>,
    pub module_directory: Option<PathBuf>,
}

impl CompositionCall {
    /// User-facing call label. Anonymous calls deliberately hide their
    /// content-derived synthetic instance identifier.
    pub fn display_label(&self) -> String {
        self.binding
            .clone()
            .unwrap_or_else(|| format!("{} (anonymous call)", self.module_name))
    }
}

/// Diagnostic lineage recorded while nested module calls are expanded.
///
/// `call` identifies the immediate boundary represented by the composition;
/// `root_call` identifies the root-level call whose expansion transitively
/// produced it. LSP diagnostics use this relationship to select their owning
/// document without parsing dot-separated instance strings.
///
/// This is diagnostic-only metadata, not part of a composition's identity.
/// Live expansion can only construct the `Expanded` state through
/// [`Self::expanded`], which requires both ends of the ancestry together.
/// Serialization skips the whole value, so a saved-plan reload enters the
/// explicitly named `Deserialized` state instead of a partially populated
/// pair of `Option`s.
#[derive(Debug, Clone)]
pub struct CompositionProvenance {
    state: CompositionProvenanceState,
}

#[derive(Debug, Clone)]
enum CompositionProvenanceState {
    Expanded(Box<ExpandedCompositionProvenance>),
    Deserialized,
}

#[derive(Debug, Clone)]
struct ExpandedCompositionProvenance {
    call: CompositionCall,
    root_call: CompositionCall,
}

impl CompositionProvenance {
    pub fn expanded(call: CompositionCall, root_call: CompositionCall) -> Self {
        Self {
            state: CompositionProvenanceState::Expanded(Box::new(ExpandedCompositionProvenance {
                call,
                root_call,
            })),
        }
    }

    pub fn deserialized() -> Self {
        Self {
            state: CompositionProvenanceState::Deserialized,
        }
    }

    fn calls(&self) -> Option<(&CompositionCall, &CompositionCall)> {
        match &self.state {
            CompositionProvenanceState::Expanded(expanded) => {
                Some((&expanded.call, &expanded.root_call))
            }
            CompositionProvenanceState::Deserialized => None,
        }
    }

    pub(crate) fn calls_mut(&mut self) -> Option<(&mut CompositionCall, &mut CompositionCall)> {
        match &mut self.state {
            CompositionProvenanceState::Expanded(expanded) => {
                Some((&mut expanded.call, &mut expanded.root_call))
            }
            CompositionProvenanceState::Deserialized => None,
        }
    }
}

impl Default for CompositionProvenance {
    fn default() -> Self {
        Self::deserialized()
    }
}

impl PartialEq for CompositionProvenance {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for CompositionProvenance {}

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
    /// Diagnostic-only call ancestry; see [`CompositionProvenance`].
    #[serde(skip)]
    pub provenance: Box<CompositionProvenance>,
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

    /// Immediate call identity when this composition came from live expansion.
    pub fn diagnostic_call(&self) -> Option<&CompositionCall> {
        self.provenance.calls().map(|(call, _)| call)
    }

    /// Root-level ancestor call when this composition came from live expansion.
    pub fn diagnostic_root_call(&self) -> Option<&CompositionCall> {
        self.provenance.calls().map(|(_, root_call)| root_call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{CompareOp, ValidateExpr};
    use crate::resource::ConcreteValue;

    #[test]
    fn equality_ignores_provenance_but_compares_composition_fields() {
        let provenance = |binding: &str| {
            let call = CompositionCall {
                module_name: "module".to_string(),
                binding: Some(binding.to_string()),
                instance: binding.to_string(),
                module_source: Some(format!("../{binding}")),
                module_directory: None,
            };
            Box::new(CompositionProvenance::expanded(call.clone(), call))
        };
        let left = Composition {
            id: ResourceId::with_identity("_virtual", "call"),
            signature: Signature {
                arguments: IndexMap::new(),
                attributes: IndexMap::new(),
                pending_constraints: Vec::new(),
            },
            binding: Some("call".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "module".to_string(),
            instance: "call".to_string(),
            provenance: provenance("first"),
            quoted_string_attrs: HashSet::new(),
        };
        let mut right = left.clone();
        right.provenance = provenance("second");

        assert_eq!(left, right);
        assert!(left.diagnostic_call().is_some());

        let decoded: Composition = serde_json::from_value(
            serde_json::to_value(&left).expect("serialize composition with provenance"),
        )
        .expect("deserialize composition without provenance");
        assert!(decoded.diagnostic_call().is_none());
        assert!(decoded.diagnostic_root_call().is_none());

        right.binding = Some("different".to_string());
        assert_ne!(left, right);
    }

    #[test]
    fn from_value_resource_ref_classifies_as_forwarded() {
        let path = AccessPath::new("role", "arn");
        let v = Value::Deferred(DeferredValue::ResourceRef { path: path.clone() });
        let attr = CompositionAttribute::from_value(v, None);
        assert_eq!(attr.forwarded_path(), Some(&path));
    }

    #[test]
    fn from_value_concrete_string_classifies_as_derived() {
        let v = Value::Concrete(ConcreteValue::String("literal".to_string()));
        let attr = CompositionAttribute::from_value(v.clone(), None);
        assert_eq!(attr.forwarded_path(), None);
        assert_eq!(attr.to_value(), v);
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
        let attr = CompositionAttribute::from_value(v.clone(), None);
        assert_eq!(attr.forwarded_path(), None);
        assert_eq!(attr.to_value(), v);
    }

    #[test]
    fn forwarded_to_value_is_resource_ref() {
        let path = AccessPath::new("svc", "endpoint");
        let attr = CompositionAttribute::from_value(
            Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
            None,
        );
        let v = attr.to_value();
        assert_eq!(
            v,
            Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
        );
    }

    #[test]
    fn derived_to_value_returns_inner() {
        let inner = Value::Concrete(ConcreteValue::String("kept".to_string()));
        let attr = CompositionAttribute::from_value(inner.clone(), None);
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
            let lifted = CompositionAttribute::from_value(original.clone(), None);
            assert_eq!(lifted.to_value(), original);
        }
    }

    #[test]
    fn declared_type_is_carried_with_value_rewrites() {
        let declared = TypeExpr::String;
        let attr = CompositionAttribute::from_value(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("svc", "endpoint"),
            }),
            Some(declared.clone()),
        );

        let rewritten = attr.with_value(Value::Concrete(ConcreteValue::String("x".to_string())));

        assert_eq!(rewritten.declared_type(), Some(&declared));
    }

    #[test]
    fn declared_type_is_validation_only_in_serde_round_trip() {
        let value = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("svc", "endpoint"),
        });
        let typed = CompositionAttribute::from_value(value.clone(), Some(TypeExpr::String));
        let untyped = CompositionAttribute::from_value(value.clone(), None);

        let typed_json = serde_json::to_value(&typed).expect("serialize typed attribute");
        let untyped_json = serde_json::to_value(&untyped).expect("serialize untyped attribute");
        assert_eq!(
            typed_json, untyped_json,
            "declared type must not alter the saved-plan wire shape"
        );

        let decoded: CompositionAttribute =
            serde_json::from_value(typed_json).expect("deserialize legacy-compatible attribute");
        assert_eq!(decoded.to_value(), value);
        assert_eq!(decoded.declared_type(), None);
    }

    #[test]
    fn argument_declared_type_is_carried_with_value_rewrites() {
        let declared = TypeExpr::SchemaType {
            provider: "aws".to_string(),
            path: "ec2.Vpc".to_string(),
            type_name: "Id".to_string(),
        };
        let argument = CompositionArgument::from_value(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("vpc", "vpc_id"),
            }),
            declared.clone(),
        );

        let rewritten = argument.with_value(Value::Concrete(ConcreteValue::String("x".into())));

        assert_eq!(rewritten.declared_type(), Some(&declared));
    }

    #[test]
    fn argument_declared_type_is_validation_only_in_serde_round_trip() {
        let value = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("vpc", "vpc_id"),
        });
        let typed = CompositionArgument::from_value(value.clone(), TypeExpr::String);

        let typed_json = serde_json::to_value(&typed).expect("serialize typed argument");
        let value_json = serde_json::to_value(&value).expect("serialize argument value");
        assert_eq!(typed_json, value_json);

        let decoded: CompositionArgument =
            serde_json::from_value(typed_json).expect("deserialize composition argument");
        assert_eq!(decoded.value(), &value);
        assert_eq!(decoded.declared_type(), None);
    }

    #[test]
    fn pending_constraints_survive_serde_round_trip_and_secret_redaction() {
        let constraint = PendingModuleConstraint {
            id: ModuleConstraintId::argument_validation("password", 0),
            expression: ValidateExpr::Compare {
                lhs: Box::new(ValidateExpr::FunctionCall {
                    name: "length".to_string(),
                    args: vec![ValidateExpr::Var("password".to_string())],
                }),
                op: CompareOp::Gte,
                rhs: Box::new(ValidateExpr::Int(12)),
            },
            message: "password must contain at least 12 characters".to_string(),
        };
        let composition = Composition {
            id: ResourceId::with_identity("_virtual", "secure"),
            signature: Signature {
                arguments: IndexMap::from([(
                    "password".to_string(),
                    CompositionArgument::from_value(
                        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                            ConcreteValue::String("plaintext-secret".to_string()),
                        )))),
                        TypeExpr::String,
                    ),
                )]),
                attributes: IndexMap::new(),
                pending_constraints: vec![constraint.clone()],
            },
            binding: Some("secure".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "secure_module".to_string(),
            instance: "secure".to_string(),
            provenance: Default::default(),
            quoted_string_attrs: HashSet::new(),
        };

        let redacted = crate::value::redact_secrets_in_virtual(&composition)
            .expect("composition redaction must succeed");
        let json = serde_json::to_string(&redacted).expect("serialize redacted composition");
        assert!(!json.contains("plaintext-secret"), "{json}");
        let decoded: Composition = serde_json::from_str(&json).expect("round-trip composition");
        assert_eq!(decoded.signature.pending_constraints, vec![constraint]);
    }
}
