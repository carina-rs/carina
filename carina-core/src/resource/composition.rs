//! `LiveComposition` — a synthetic IR node created by the module
//! resolver to expose module `attributes` values.
//!
//! Part of the resource typestate split (#3169). Virtual resources
//! are not sent to providers; they exist only in the IR. The
//! `signature.attributes` map may contain unresolved
//! `ResourceRef` / `BindingRef` values whose resolution is **deferred
//! to the post-apply path**. The typestate split encodes that
//! invariant: a `LiveComposition` is never accepted by the pre-apply
//! resolver.
//!
//! Unlike [`Resource`](super::Resource), this struct
//! does not carry `directives` (no `prevent_destroy` applies to a
//! synthetic node) or `prefixes` (no auto-generated names on a
//! non-provider resource). `module_source` display metadata is flattened to
//! `module_name` + `instance`; parser-minted identity lives in `call_path`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::parser::ValidateExpr;
use crate::static_type::ConsumerStaticType;

use super::{
    AbsoluteBindingAddress, AccessPath, BindingAddress, DeferredValue, ModuleInstanceScope,
    RelativeBindingName, ResolvedResourceId, StructuralOperand, StructuralOperandRef, Value,
};

#[cfg(test)]
use super::RootScope;

/// How a composition attribute is produced from the rest of the
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
enum CompositionAttributeValue<B: BindingAddress = AbsoluteBindingAddress> {
    /// Single-hop alias to another node's attribute, by path.
    Forwarded(AccessPath<B>),
    /// Multi-source expression: a literal, interpolation, function
    /// call, or any other `Value` shape that is not a bare
    /// single-hop reference.
    Derived(Value<B>),
}

/// One module output together with its declared boundary type.
///
/// Keeping the optional annotation in the same entry as the classified value
/// makes it impossible for expansion or later value rewrites to create an
/// output without deciding what happens to its declaration. The annotation is
/// validation-only and is discarded explicitly by the exhaustive live-to-saved
/// conversion. Compositions are never persisted in state files; saved-plan
/// execution has already passed validation and therefore does not need this
/// metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveCompositionAttribute<B: BindingAddress = AbsoluteBindingAddress> {
    value: CompositionAttributeValue<B>,
    declared_type: Option<ConsumerStaticType>,
    /// The module argument directly forwarded by this output before call-site
    /// substitution. Validation checks that declaration in the module's own
    /// scope; retaining the provenance prevents an expanded caller value from
    /// being checked again and blamed on the output declaration.
    source_argument: Option<String>,
}

pub type AuthoredLiveCompositionAttribute = LiveCompositionAttribute<RelativeBindingName>;
pub type ExpandedLiveCompositionAttribute = LiveCompositionAttribute<AbsoluteBindingAddress>;

impl<B: BindingAddress> LiveCompositionAttribute<B> {
    /// Whether this output contains a saved-plan secret placeholder.
    ///
    /// Keep the destructuring exhaustive so adding another field or value
    /// representation requires the saved-plan placeholder inventory to make an
    /// explicit decision about it.
    pub fn contains_redacted_secret_placeholder(&self) -> bool {
        let Self {
            value,
            declared_type: _declared_type,
            source_argument: _source_argument,
        } = self;
        match value {
            CompositionAttributeValue::Forwarded(_) => false,
            CompositionAttributeValue::Derived(value) => {
                crate::value::contains_redacted_secret(value)
            }
        }
    }

    /// Classify a `Value` into the appropriate
    /// [`LiveCompositionAttribute`] variant.
    ///
    /// `Value::Deferred(DeferredValue::ResourceRef { path })` is a
    /// single-hop alias and is classified as forwarded.
    /// Every other `Value` shape is multi-source (literal,
    /// interpolation, function call, etc.) and lifts into
    /// classified as derived.
    pub fn from_value(value: Value<B>, declared_type: Option<ConsumerStaticType>) -> Self {
        Self::from_value_with_static_type(value, declared_type)
    }

    pub fn from_value_with_static_type(
        value: Value<B>,
        declared_type: Option<ConsumerStaticType>,
    ) -> Self {
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
    pub fn declared_type(&self) -> Option<&ConsumerStaticType> {
        self.declared_type.as_ref()
    }

    /// The forwarded path when this output is a single-hop alias.
    pub fn forwarded_path(&self) -> Option<&AccessPath<B>> {
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
    pub fn with_value(&self, value: Value<B>) -> Self {
        Self::from_value_with_static_type(value, self.declared_type.clone())
            .with_source_argument(self.source_argument.clone())
    }

    /// Reify back into a [`Value`] for callers that have not yet been
    /// migrated to the new typed-variant dispatch.
    ///
    /// The post-apply resolver, plan display, and exporters consume
    /// composition attributes as `Value`s today; this lets PR G ship
    /// the type-level split without rewriting every consumer in one
    /// commit. Each subsequent migration replaces a `.to_value()` site
    /// with a direct match on `LiveCompositionAttribute`.
    pub fn to_value(&self) -> Value<B> {
        match &self.value {
            CompositionAttributeValue::Forwarded(path) => {
                Value::Deferred(DeferredValue::ResourceRef { path: path.clone() })
            }
            CompositionAttributeValue::Derived(v) => v.clone(),
        }
    }
}

impl LiveCompositionAttribute<RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> LiveCompositionAttribute<AbsoluteBindingAddress> {
        let Self {
            value,
            declared_type,
            source_argument,
        } = self;
        let value = match value {
            CompositionAttributeValue::Forwarded(path) => {
                CompositionAttributeValue::Forwarded(path.resolve_bindings(resolve))
            }
            CompositionAttributeValue::Derived(value) => {
                CompositionAttributeValue::Derived(value.resolve_bindings(resolve))
            }
        };
        LiveCompositionAttribute {
            value,
            declared_type,
            source_argument,
        }
    }
}

impl LiveCompositionAttribute<AbsoluteBindingAddress> {
    fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        let Self {
            value,
            declared_type: _declared_type,
            source_argument: _source_argument,
        } = self;
        match value {
            CompositionAttributeValue::Forwarded(path) => path.map_absolute_bindings(map),
            CompositionAttributeValue::Derived(value) => value.map_absolute_bindings(map),
        }
    }
}

/// Saved-plan form of one module output.
///
/// Declaration and parser provenance are deliberately absent: a saved plan
/// carries only the forwarded/derived execution value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SavedCompositionAttribute {
    value: CompositionAttributeValue,
}

impl SavedCompositionAttribute {
    pub fn from_value(value: Value) -> Self {
        let value = match value {
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                CompositionAttributeValue::Forwarded(path)
            }
            other => CompositionAttributeValue::Derived(other),
        };
        Self { value }
    }

    pub fn contains_redacted_secret_placeholder(&self) -> bool {
        let Self { value } = self;
        match value {
            CompositionAttributeValue::Forwarded(_) => false,
            CompositionAttributeValue::Derived(value) => {
                crate::value::contains_redacted_secret(value)
            }
        }
    }

    pub fn forwarded_path(&self) -> Option<&AccessPath> {
        match &self.value {
            CompositionAttributeValue::Forwarded(path) => Some(path),
            CompositionAttributeValue::Derived(_) => None,
        }
    }

    pub fn with_value(&self, value: Value) -> Self {
        Self::from_value(value)
    }

    pub fn to_value(&self) -> Value {
        match &self.value {
            CompositionAttributeValue::Forwarded(path) => {
                Value::Deferred(DeferredValue::ResourceRef { path: path.clone() })
            }
            CompositionAttributeValue::Derived(value) => value.clone(),
        }
    }
}

/// One resolved module-call argument together with its live boundary metadata.
///
/// The declaration and resolver origin are validation-only and deliberately
/// travel together through every value rewrite. Keeping the fields private
/// means callers cannot insert a raw [`Value`] into [`LiveCompositionSignature::arguments`];
/// every live construction site must supply both pieces of metadata. Saved
/// plans keep the historical value-only shape and deserialize into the
/// explicitly metadata-free state.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveCompositionArgument<B: BindingAddress = AbsoluteBindingAddress> {
    operand: StructuralOperand<B>,
    metadata: LiveCompositionArgumentMetadata,
}

pub type AuthoredLiveCompositionArgument = LiveCompositionArgument<RelativeBindingName>;
pub type ExpandedLiveCompositionArgument = LiveCompositionArgument<AbsoluteBindingAddress>;

#[derive(Debug, Clone, PartialEq)]
struct LiveCompositionArgumentMetadata {
    declared_type: ConsumerStaticType,
    origin: crate::parser::ArgumentOrigin,
}

impl<B: BindingAddress> LiveCompositionArgument<B> {
    /// Whether this argument contains a saved-plan secret placeholder.
    ///
    /// The exhaustive destructuring makes new fields part of the saved-plan
    /// placeholder inventory by construction.
    pub fn contains_redacted_secret_placeholder(&self) -> bool {
        let Self { operand, metadata } = self;
        let LiveCompositionArgumentMetadata {
            declared_type,
            origin,
        } = metadata;
        let _validation_metadata = (declared_type, origin);
        crate::value::contains_redacted_secret(operand.traversal_value())
    }

    pub fn from_value(
        value: Value<B>,
        declared_type: ConsumerStaticType,
        origin: crate::parser::ArgumentOrigin,
    ) -> Self {
        Self::from_value_with_static_type(value, declared_type, origin)
    }

    pub fn from_value_with_static_type(
        value: Value<B>,
        declared_type: ConsumerStaticType,
        origin: crate::parser::ArgumentOrigin,
    ) -> Self {
        Self {
            operand: StructuralOperand::from_value(value),
            metadata: LiveCompositionArgumentMetadata {
                declared_type,
                origin,
            },
        }
    }

    pub(crate) fn from_handle_with_static_type(
        value: Value<B>,
        declared_type: ConsumerStaticType,
        handle: crate::eval_value::StructuralHandle<B>,
        origin: crate::parser::ArgumentOrigin,
    ) -> Self {
        Self {
            operand: StructuralOperand::from_handle_with_access_base(handle, value),
            metadata: LiveCompositionArgumentMetadata {
                declared_type,
                origin,
            },
        }
    }

    pub fn value(&self) -> &Value<B> {
        self.operand.traversal_value()
    }

    pub(crate) fn try_visit_values<E>(
        &self,
        visit: &mut impl FnMut(&Value<B>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.operand.try_visit_values(visit)
    }

    pub fn declared_type(&self) -> &ConsumerStaticType {
        &self.metadata.declared_type
    }

    pub fn metadata(&self) -> (&ConsumerStaticType, crate::parser::ArgumentOrigin) {
        (&self.metadata.declared_type, self.metadata.origin)
    }

    pub(crate) fn validation_metadata(
        &self,
    ) -> (&ConsumerStaticType, crate::parser::ArgumentOrigin) {
        self.metadata()
    }

    pub(crate) fn operand_ref(&self) -> StructuralOperandRef<'_, B> {
        self.operand.value_ref()
    }

    /// Rewrite a prefixed/substituted value without dropping its declaration.
    pub fn with_value(&self, value: Value<B>) -> Self {
        let operand = match self.operand.handle() {
            Some(handle) => StructuralOperand::from_handle_with_access_base(handle.clone(), value),
            None => StructuralOperand::from_value(value),
        };
        Self {
            operand,
            metadata: self.metadata.clone(),
        }
    }
}

impl LiveCompositionArgument<RelativeBindingName> {
    fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> LiveCompositionArgument<AbsoluteBindingAddress> {
        let Self { operand, metadata } = self;
        LiveCompositionArgument {
            operand: operand.resolve_bindings(resolve),
            metadata,
        }
    }
}

impl LiveCompositionArgument<AbsoluteBindingAddress> {
    fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        let Self {
            operand,
            metadata:
                LiveCompositionArgumentMetadata {
                    declared_type: _declared_type,
                    origin: _origin,
                },
        } = self;
        operand.map_absolute_bindings(map);
    }
}

/// Saved-plan form of a module-call argument.
///
/// This is intentionally value-only. It has no declaration, origin, or
/// structural-handle accessor that a validation caller could accidentally use.
///
/// ```compile_fail
/// use carina_core::resource::SavedCompositionArgument;
/// fn declaration(argument: &SavedCompositionArgument) {
///     let _ = argument.declared_type();
/// }
/// ```
///
/// ```compile_fail
/// use carina_core::resource::SavedCompositionArgument;
/// fn origin(argument: &SavedCompositionArgument) {
///     let _ = argument.metadata();
/// }
/// ```
///
/// A saved argument cannot be inserted into a live signature:
///
/// ```compile_fail
/// use carina_core::resource::{LiveCompositionSignature, SavedCompositionArgument};
/// fn insert(signature: &mut LiveCompositionSignature, argument: SavedCompositionArgument) {
///     signature.arguments.insert("value".to_string(), argument);
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SavedCompositionArgument {
    value: Value,
}

impl SavedCompositionArgument {
    pub fn from_value(value: Value) -> Self {
        Self { value }
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn contains_redacted_secret_placeholder(&self) -> bool {
        let Self { value } = self;
        crate::value::contains_redacted_secret(value)
    }

    pub fn with_value(&self, value: Value) -> Self {
        Self { value }
    }
}

/// Stable identity of one authored module constraint within its module.
///
/// The module instance is deliberately not part of this identifier: the same
/// declaration may be instantiated many times. Execution code pairs this ID
/// with [`LiveComposition::instance`] when it needs an instance-wide identity.
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

    /// The argument owned by an argument-local validation constraint.
    ///
    /// `validation` blocks are nested in an argument declaration and may
    /// evaluate only against that argument. Keeping that scope encoded in the
    /// stable constraint ID lets saved plans preserve the language rule
    /// without adding a second, independently maintained discriminator.
    pub fn validation_argument(&self) -> Option<&str> {
        self.0
            .strip_prefix("argument:")
            .and_then(|rest| rest.rsplit_once(':').map(|(argument, _)| argument))
    }
}

/// A module constraint whose referenced arguments were not fully known at
/// expansion time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingModuleConstraint<B: BindingAddress = AbsoluteBindingAddress> {
    pub id: ModuleConstraintId,
    pub expression: ValidateExpr<B>,
    pub message: String,
}

impl<B: BindingAddress> PendingModuleConstraint<B> {
    pub fn id(&self) -> &ModuleConstraintId {
        &self.id
    }

    pub fn expression(&self) -> &ValidateExpr<B> {
        &self.expression
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl PendingModuleConstraint<AbsoluteBindingAddress> {
    fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        let Self {
            id: _id,
            expression,
            message: _message,
        } = self;
        expression.map_absolute_bindings(map);
    }
}

/// The function-shaped I/O surface of a live composition.
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
/// `LiveCompositionSignature` is intentionally *only* on `LiveComposition`: the DSL
/// gives `Resource` and `DataSource` a single user-written
/// `attributes` namespace with no `arguments` concept, so embedding a
/// `LiveCompositionSignature` on those structs would be a pretextual abstraction
/// with one populated half. See the rescoped design note
/// (`notes/specs/2026-05-25-composition-graph-node-design.md`, PR
/// #3301) for the rationale.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveCompositionSignature<B: BindingAddress = AbsoluteBindingAddress> {
    /// Resolved call-site arguments. Empty when the composition was
    /// produced by a module that does not declare any `argument`
    /// parameters, or when the call site passed no arguments.
    pub arguments: IndexMap<String, LiveCompositionArgument<B>>,
    /// Module-output values classified by how they are produced
    /// (#3294): forwarded values are single-hop aliases and derived values are
    /// multi-source expressions. Each entry also carries its optional declared
    /// [`crate::parser::TypeExpr`] for validation.
    pub attributes: IndexMap<String, LiveCompositionAttribute<B>>,
    /// Constraints deferred until all referenced argument values are known.
    pub pending_constraints: Vec<PendingModuleConstraint<B>>,
}

pub type AuthoredLiveCompositionSignature = LiveCompositionSignature<RelativeBindingName>;
pub type ExpandedLiveCompositionSignature = LiveCompositionSignature<AbsoluteBindingAddress>;

/// Value-only function boundary persisted in a saved plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedCompositionSignature {
    pub arguments: IndexMap<String, SavedCompositionArgument>,
    pub attributes: IndexMap<String, SavedCompositionAttribute>,
    pub pending_constraints: Vec<PendingModuleConstraint<AbsoluteBindingAddress>>,
}

/// Structural identity for one module call that produced a composition.
///
/// `instance` is retained for resource addressing, while `binding` and
/// `module_name` are the authored names used by diagnostics.
/// The optional source is the path from the corresponding `use` statement.
/// This metadata is validation-only and is never persisted in saved plans.
/// Equality and hashing use only the parser-minted ID; every other field is
/// display or resolution context and must not become a system identity.
#[derive(Debug, Clone)]
pub struct CompositionCall<B: BindingAddress = AbsoluteBindingAddress> {
    pub id: crate::parser::ModuleCallId,
    pub module_name: String,
    pub binding: Option<B>,
    pub instance: String,
    pub module_source: Option<String>,
    pub module_directory: Option<PathBuf>,
    /// Validation-only origin when an unbound loop produced this call.
    #[doc(hidden)]
    pub loop_origin: Option<Box<super::LoopElementOrigin>>,
}

pub type AuthoredCompositionCall = CompositionCall<RelativeBindingName>;
pub type ExpandedCompositionCall = CompositionCall<AbsoluteBindingAddress>;

impl CompositionCall<RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> CompositionCall<AbsoluteBindingAddress> {
        let Self {
            id,
            module_name,
            binding,
            instance,
            module_source,
            module_directory,
            loop_origin,
        } = self;
        CompositionCall {
            id,
            module_name,
            binding: binding.map(resolve),
            instance,
            module_source,
            module_directory,
            loop_origin,
        }
    }

    #[cfg(test)]
    pub(crate) fn resolve_root(self, scope: &RootScope) -> CompositionCall<AbsoluteBindingAddress> {
        self.resolve_bindings(&mut |binding| scope.resolve(binding))
    }
}

impl CompositionCall<AbsoluteBindingAddress> {
    fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        let Self {
            id: _id,
            module_name: _module_name,
            binding,
            instance: _instance,
            module_source: _module_source,
            module_directory: _module_directory,
            loop_origin: _loop_origin,
        } = self;
        if let Some(binding) = binding {
            *binding = map(binding);
        }
    }
}

impl<B: BindingAddress> PartialEq for CompositionCall<B> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<B: BindingAddress> Eq for CompositionCall<B> {}

impl<B: BindingAddress> std::hash::Hash for CompositionCall<B> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.id, state);
    }
}

impl<B: BindingAddress> CompositionCall<B> {
    pub(crate) fn loop_origin(&self) -> Option<&super::LoopElementOrigin> {
        self.loop_origin.as_deref()
    }

    /// User-facing call label. Anonymous calls deliberately hide their
    /// content-derived synthetic instance identifier.
    pub fn display_label(&self) -> String {
        match self.id.expansion() {
            crate::parser::CallExpansion::Direct => self
                .binding
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("{} (anonymous call)", self.module_name)),
            crate::parser::CallExpansion::Expanded(_expansion_key) => {
                format!("{} (anonymous call)", self.module_name)
            }
        }
    }

    /// Project an authored parser call onto this display authority. Fields
    /// unrelated to labels are intentionally empty and never escape this call.
    pub(crate) fn display_label_for_module_call(call: &crate::parser::ModuleCall) -> String {
        CompositionCall::<RelativeBindingName> {
            id: call.id().clone(),
            module_name: call.module_name.clone(),
            binding: call.binding_name.clone(),
            instance: String::new(),
            module_source: None,
            module_directory: None,
            loop_origin: None,
        }
        .display_label()
    }
}

/// Complete non-empty module-call ancestry of one live composition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompositionCallPath<B: BindingAddress = AbsoluteBindingAddress> {
    root: CompositionCall<B>,
    descendants: Vec<CompositionCall<B>>,
}

pub type AuthoredCompositionCallPath = CompositionCallPath<RelativeBindingName>;
pub type ExpandedCompositionCallPath = CompositionCallPath<AbsoluteBindingAddress>;

impl<B: BindingAddress> CompositionCallPath<B> {
    pub fn direct(call: CompositionCall<B>) -> Self {
        Self {
            root: call,
            descendants: Vec::new(),
        }
    }

    pub fn root(&self) -> &CompositionCall<B> {
        &self.root
    }

    pub fn immediate(&self) -> &CompositionCall<B> {
        self.descendants.last().unwrap_or(&self.root)
    }

    pub fn calls(&self) -> impl Iterator<Item = &CompositionCall<B>> {
        std::iter::once(&self.root).chain(self.descendants.iter())
    }

    /// Whether this call ancestry governs the node owned by `other`.
    pub fn is_prefix_of(&self, other: &Self) -> bool {
        let mut other_calls = other.calls();
        self.calls().all(|call| {
            other_calls
                .next()
                .is_some_and(|other_call| call.id == other_call.id)
        })
    }

    pub(crate) fn calls_mut(&mut self) -> impl Iterator<Item = &mut CompositionCall<B>> {
        std::iter::once(&mut self.root).chain(self.descendants.iter_mut())
    }

    pub(crate) fn prepend(self, root: CompositionCall<B>) -> Self {
        let Self {
            root: previous_root,
            descendants: previous_descendants,
        } = self;
        let mut descendants = Vec::with_capacity(previous_descendants.len() + 1);
        descendants.push(previous_root);
        descendants.extend(previous_descendants);
        Self { root, descendants }
    }

    pub(crate) fn append(mut self, call: CompositionCall<B>) -> Self {
        self.descendants.push(call);
        self
    }
}

impl CompositionCallPath<RelativeBindingName> {
    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> CompositionCallPath<AbsoluteBindingAddress> {
        let Self { root, descendants } = self;
        CompositionCallPath {
            root: root.resolve_bindings(resolve),
            descendants: descendants
                .into_iter()
                .map(|call| call.resolve_bindings(resolve))
                .collect(),
        }
    }
}

impl CompositionCallPath<AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        let Self { root, descendants } = self;
        root.map_absolute_bindings(map);
        for call in descendants {
            call.map_absolute_bindings(map);
        }
    }
}

#[cfg(test)]
impl Default for CompositionCallPath {
    fn default() -> Self {
        Self::direct(CompositionCall {
            id: crate::parser::ModuleCallId::test_dummy_in(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
            ),
            module_name: "test_module".to_string(),
            binding: Some(RootScope::new().resolve(RelativeBindingName::new("test_call"))),
            instance: "test_call".to_string(),
            module_source: None,
            module_directory: None,
            loop_origin: None,
        })
    }
}

const MAX_SAVED_SOURCE_PATH_BYTES: usize = 4096;
const MAX_SAVED_SOURCE_PATH_COMPONENTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "PathBuf", into = "PathBuf")]
struct SourceRootRelativePath(PathBuf);

/// A saved composition source path violated its portable relative-path
/// invariant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SavedCompositionSourcePathError {
    #[error("saved composition source path must not be empty")]
    Empty,
    #[error(
        "saved composition source path must be relative and contain only normal or parent-directory (`..`) components"
    )]
    NotRelative,
    #[error("saved composition source path must not contain a NUL byte")]
    ContainsNul,
    #[error(
        "saved composition source path exceeds the maximum length of {maximum} bytes (found {length} bytes)"
    )]
    TooLong { length: usize, maximum: usize },
    #[error(
        "saved composition source path has {count} components; the maximum is {maximum} components"
    )]
    TooManyComponents { count: usize, maximum: usize },
    #[error("saved composition source path must not contain a current-directory (`.`) component")]
    CurrentDirectory,
}

impl TryFrom<PathBuf> for SourceRootRelativePath {
    type Error = SavedCompositionSourcePathError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        if path.as_os_str().is_empty() {
            return Err(SavedCompositionSourcePathError::Empty);
        }
        let encoded = path.as_os_str().as_encoded_bytes();
        if encoded.contains(&0) {
            return Err(SavedCompositionSourcePathError::ContainsNul);
        }
        if encoded.len() > MAX_SAVED_SOURCE_PATH_BYTES {
            return Err(SavedCompositionSourcePathError::TooLong {
                length: encoded.len(),
                maximum: MAX_SAVED_SOURCE_PATH_BYTES,
            });
        }
        if encoded
            .split(|byte| byte.is_ascii() && std::path::is_separator(char::from(*byte)))
            .any(|component| component == b".")
        {
            return Err(SavedCompositionSourcePathError::CurrentDirectory);
        }

        let mut component_count = 0;
        for component in path.components() {
            component_count += 1;
            if component_count > MAX_SAVED_SOURCE_PATH_COMPONENTS {
                return Err(SavedCompositionSourcePathError::TooManyComponents {
                    count: component_count,
                    maximum: MAX_SAVED_SOURCE_PATH_COMPONENTS,
                });
            }
            if !matches!(component, Component::Normal(_) | Component::ParentDir) {
                return Err(SavedCompositionSourcePathError::NotRelative);
            }
        }

        Ok(Self(path))
    }
}

impl From<SourceRootRelativePath> for PathBuf {
    fn from(path: SourceRootRelativePath) -> Self {
        path.0
    }
}

impl SourceRootRelativePath {
    fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "location", content = "path", rename_all = "snake_case")]
enum SavedSourceFileId {
    SourceRootRelative(SourceRootRelativePath),
}

/// A composition call source could not be converted into a portable saved
/// identity.
#[derive(Debug)]
pub enum CompositionSourceIdentityError {
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
    DifferentRoot {
        path: PathBuf,
        source_root: PathBuf,
    },
    InvalidRelativePath {
        path: PathBuf,
        source: SavedCompositionSourcePathError,
    },
}

impl CompositionSourceIdentityError {
    fn canonicalize(path: &Path) -> Result<PathBuf, Self> {
        path.canonicalize().map_err(|source| Self::Canonicalize {
            path: path.to_path_buf(),
            source,
        })
    }
}

impl std::fmt::Display for CompositionSourceIdentityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canonicalize { path, source: _ } => {
                let path = path.to_string_lossy();
                write!(
                    formatter,
                    "failed to canonicalize composition source path `{}`",
                    path.escape_debug(),
                )
            }
            Self::DifferentRoot { path, source_root } => {
                let path = path.to_string_lossy();
                let source_root = source_root.to_string_lossy();
                write!(
                    formatter,
                    "composition source path `{}` cannot be expressed relative to source root `{}`",
                    path.escape_debug(),
                    source_root.escape_debug(),
                )
            }
            Self::InvalidRelativePath { path, source: _ } => {
                let path = path.to_string_lossy();
                write!(
                    formatter,
                    "composition source path `{}` cannot be saved as a source-root-relative path",
                    path.escape_debug(),
                )
            }
        }
    }
}

impl std::error::Error for CompositionSourceIdentityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Canonicalize { source, .. } => Some(source),
            Self::DifferentRoot { .. } => None,
            Self::InvalidRelativePath { source, .. } => Some(source),
        }
    }
}

fn relative_path_from_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let root_components = root.components().collect::<Vec<_>>();
    let path_components = path.components().collect::<Vec<_>>();
    let anchor_len = |components: &[Component<'_>]| {
        components
            .iter()
            .take_while(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
            .count()
    };
    let root_anchor_len = anchor_len(&root_components);
    let path_anchor_len = anchor_len(&path_components);
    if root_components[..root_anchor_len] != path_components[..path_anchor_len] {
        return None;
    }

    let common = root_components
        .iter()
        .zip(&path_components)
        .take_while(|(root, path)| root == path)
        .count();
    let mut relative = PathBuf::new();
    for _ in &root_components[common..] {
        relative.push(Component::ParentDir.as_os_str());
    }
    for component in &path_components[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

/// Operation-scoped converter from live parser paths to portable saved call
/// identities. The source root is canonicalized once and each distinct live
/// source file is canonicalized at most once.
#[derive(Debug)]
pub struct CompositionSourceIdentityResolver {
    canonical_source_root: PathBuf,
    canonical_source_files: HashMap<PathBuf, PathBuf>,
}

impl CompositionSourceIdentityResolver {
    pub fn new(source_root: &Path) -> Result<Self, CompositionSourceIdentityError> {
        Ok(Self {
            canonical_source_root: CompositionSourceIdentityError::canonicalize(source_root)?,
            canonical_source_files: HashMap::new(),
        })
    }

    fn saved_source_file(
        &mut self,
        path: &Path,
    ) -> Result<SavedSourceFileId, CompositionSourceIdentityError> {
        let source = match self.canonical_source_files.get(path) {
            Some(source) => source.clone(),
            None => {
                // The loader preserves the path as supplied by the process.
                // Relative source paths therefore resolve against the process
                // CWD, not against the already-resolved source root.
                let source = CompositionSourceIdentityError::canonicalize(path)?;
                self.canonical_source_files
                    .insert(path.to_path_buf(), source.clone());
                source
            }
        };
        let relative =
            relative_path_from_root(&self.canonical_source_root, &source).ok_or_else(|| {
                CompositionSourceIdentityError::DifferentRoot {
                    path: source.clone(),
                    source_root: self.canonical_source_root.clone(),
                }
            })?;
        let relative_path = relative.clone();
        let relative = SourceRootRelativePath::try_from(relative).map_err(|source| {
            CompositionSourceIdentityError::InvalidRelativePath {
                path: relative_path,
                source,
            }
        })?;
        Ok(SavedSourceFileId::SourceRootRelative(relative))
    }

    pub fn saved_origin_id(
        &mut self,
        path: &CompositionCallPath,
    ) -> Result<SavedCompositionOriginId, CompositionSourceIdentityError> {
        SavedCompositionOriginId::from_live(path, self)
    }
}

impl SavedSourceFileId {
    fn display_path(&self) -> &Path {
        match self {
            Self::SourceRootRelative(path) => path.as_path(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct SavedSourceSpan {
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
}

impl From<crate::parser::SourceSpan> for SavedSourceSpan {
    fn from(span: crate::parser::SourceSpan) -> Self {
        Self {
            start_byte: span.start_byte(),
            end_byte: span.end_byte(),
            start_line: span.start_line(),
            start_column: span.start_column(),
            end_line: span.end_line(),
            end_column: span.end_column(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "key", rename_all = "snake_case")]
enum SavedCallExpansion {
    Direct,
    Expanded(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SavedModuleCallId {
    source_file: SavedSourceFileId,
    call_span: SavedSourceSpan,
    expansion: SavedCallExpansion,
}

impl SavedModuleCallId {
    fn from_live(
        id: &crate::parser::ModuleCallId,
        resolver: &mut CompositionSourceIdentityResolver,
    ) -> Result<Self, CompositionSourceIdentityError> {
        let expansion = match id.expansion() {
            crate::parser::CallExpansion::Direct => SavedCallExpansion::Direct,
            crate::parser::CallExpansion::Expanded(key) => {
                SavedCallExpansion::Expanded(key.as_str().to_string())
            }
        };
        Ok(Self {
            source_file: resolver.saved_source_file(id.source_file())?,
            call_span: id.call_span().into(),
            expansion,
        })
    }

    pub fn source_file(&self) -> &Path {
        self.source_file.display_path()
    }

    pub fn start_line(&self) -> usize {
        self.call_span.start_line
    }

    pub fn start_column(&self) -> usize {
        self.call_span.start_column
    }
}

/// Persisted complete call-ID path used to join a saved composition to a
/// freshly expanded live composition.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SavedCompositionOriginId {
    root: SavedModuleCallId,
    descendants: Vec<SavedModuleCallId>,
}

impl SavedCompositionOriginId {
    fn from_live(
        path: &CompositionCallPath,
        resolver: &mut CompositionSourceIdentityResolver,
    ) -> Result<Self, CompositionSourceIdentityError> {
        let CompositionCallPath { root, descendants } = path;
        let root = SavedModuleCallId::from_live(&root.id, resolver)?;
        let descendants = descendants
            .iter()
            .map(|call| SavedModuleCallId::from_live(&call.id, resolver))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { root, descendants })
    }

    pub fn root(&self) -> &SavedModuleCallId {
        &self.root
    }

    pub fn immediate(&self) -> &SavedModuleCallId {
        self.descendants.last().unwrap_or(&self.root)
    }

    pub fn calls(&self) -> impl Iterator<Item = &SavedModuleCallId> {
        std::iter::once(&self.root).chain(self.descendants.iter())
    }
}

/// A live composition resource created by module-call expansion.
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
/// use carina_core::resource::LiveComposition;
/// fn _f(v: &LiveComposition) -> &std::collections::HashMap<String, String> {
///     &v.prefixes
/// }
/// ```
///
/// `directives` is dropped (no `prevent_destroy` applies to a synthetic node):
///
/// ```compile_fail
/// use carina_core::resource::LiveComposition;
/// fn _f(v: &LiveComposition) -> &carina_core::resource::Directives {
///     &v.directives
/// }
/// ```
///
/// `module_source` is dropped — display metadata is flattened into
/// `module_name` + `instance`, while identity lives in `call_path`:
///
/// ```compile_fail
/// use carina_core::resource::LiveComposition;
/// fn _f(v: &LiveComposition) -> &Option<carina_core::resource::ModuleSource> {
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
/// use carina_core::resource::LiveComposition;
/// use indexmap::IndexMap;
/// fn _f(v: &LiveComposition) -> &IndexMap<String, carina_core::resource::Value> {
///     &v.attributes
/// }
/// ```
///
/// Live arguments expose required validation metadata, not an `Option`:
///
/// ```compile_fail
/// use carina_core::parser::TypeExpr;
/// use carina_core::resource::LiveCompositionArgument;
/// fn optional(argument: &LiveCompositionArgument) -> Option<&TypeExpr> {
///     argument.declared_type()
/// }
/// ```
///
/// A live composition cannot be deserialized from plan JSON:
///
/// ```compile_fail
/// use carina_core::resource::LiveComposition;
/// let _: LiveComposition = serde_json::from_str("{}").unwrap();
/// ```
///
/// A live composition cannot be reconstructed without its call path:
///
/// ```compile_fail
/// use carina_core::resource::LiveComposition;
/// fn omit_call_path(composition: LiveComposition) -> LiveComposition {
///     LiveComposition {
///         id: composition.id,
///         signature: composition.signature,
///         binding: composition.binding,
///         dependency_bindings: composition.dependency_bindings,
///         module_name: composition.module_name,
///         instance: composition.instance,
///         quoted_string_attrs: composition.quoted_string_attrs,
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct LiveComposition<B: BindingAddress = AbsoluteBindingAddress> {
    pub id: ResolvedResourceId,
    pub signature: LiveCompositionSignature<B>,
    /// Binding name from `let` bindings in DSL.
    pub binding: Option<B>,
    /// Binding names this composition depends on.
    pub dependency_bindings: BTreeSet<B>,
    /// Module name from the originating module-call expansion
    /// (e.g. "web_tier"). Always set for compositions — see #2516.
    pub module_name: String,
    /// Module instance binding name (e.g. "web").
    pub instance: String,
    pub call_path: CompositionCallPath<B>,
    /// Parser-level: attributes whose value was written as a quoted
    /// string literal. Parse-time only; `#[serde(skip)]` keeps it out
    /// of state — mirrors [`Resource::quoted_string_attrs`](super::Resource).
    pub quoted_string_attrs: HashSet<String>,
}

pub type AuthoredLiveComposition = LiveComposition<RelativeBindingName>;
pub type ExpandedLiveComposition = LiveComposition<AbsoluteBindingAddress>;

impl<B: BindingAddress> PartialEq for LiveComposition<B> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.signature == other.signature
            && self.binding == other.binding
            && self.dependency_bindings == other.dependency_bindings
            && self.module_name == other.module_name
            && self.instance == other.instance
            && self.call_path == other.call_path
            && self.quoted_string_attrs == other.quoted_string_attrs
    }
}

impl<B: BindingAddress> LiveComposition<B> {
    /// The composition's id wrapped as an [`EphemeralId`](super::EphemeralId).
    ///
    /// A composition is plan-scoped and never persists in state, so its
    /// id is `EphemeralId`-typed. By construction this id cannot enter
    /// state-load APIs that take `&PersistentId` — that mismatch is a
    /// compile error.
    pub fn ephemeral_id(&self) -> super::EphemeralId {
        super::EphemeralId::new(self.id.clone())
    }

    pub fn call_path(&self) -> &CompositionCallPath<B> {
        &self.call_path
    }
}

impl LiveComposition<RelativeBindingName> {
    pub fn resolve_module(
        self,
        scope: &ModuleInstanceScope,
    ) -> LiveComposition<AbsoluteBindingAddress> {
        self.resolve_bindings(&mut |binding| scope.resolve(binding))
    }

    pub(crate) fn resolve_bindings(
        self,
        resolve: &mut impl FnMut(RelativeBindingName) -> AbsoluteBindingAddress,
    ) -> LiveComposition<AbsoluteBindingAddress> {
        let Self {
            id,
            signature,
            binding,
            dependency_bindings,
            module_name,
            instance,
            call_path,
            quoted_string_attrs,
        } = self;
        let LiveCompositionSignature {
            arguments,
            attributes,
            pending_constraints,
        } = signature;
        LiveComposition {
            id,
            signature: LiveCompositionSignature {
                arguments: arguments
                    .into_iter()
                    .map(|(name, argument)| (name, argument.resolve_bindings(resolve)))
                    .collect(),
                attributes: attributes
                    .into_iter()
                    .map(|(name, attribute)| (name, attribute.resolve_bindings(resolve)))
                    .collect(),
                pending_constraints: pending_constraints
                    .into_iter()
                    .map(|constraint| {
                        let PendingModuleConstraint {
                            id,
                            expression,
                            message,
                        } = constraint;
                        PendingModuleConstraint {
                            id,
                            expression: expression.resolve_bindings(resolve),
                            message,
                        }
                    })
                    .collect(),
            },
            binding: binding.map(&mut *resolve),
            dependency_bindings: dependency_bindings.into_iter().map(&mut *resolve).collect(),
            module_name,
            instance,
            call_path: call_path.resolve_bindings(resolve),
            quoted_string_attrs,
        }
    }
}

impl LiveComposition<AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(&AbsoluteBindingAddress) -> AbsoluteBindingAddress,
    ) {
        let Self {
            id: _id,
            signature,
            binding,
            dependency_bindings,
            module_name: _module_name,
            instance: _instance,
            call_path,
            quoted_string_attrs: _quoted_string_attrs,
        } = self;
        let LiveCompositionSignature {
            arguments,
            attributes,
            pending_constraints,
        } = signature;
        for argument in arguments.values_mut() {
            argument.map_absolute_bindings(map);
        }
        for attribute in attributes.values_mut() {
            attribute.map_absolute_bindings(map);
        }
        for constraint in pending_constraints {
            constraint.map_absolute_bindings(map);
        }
        if let Some(binding) = binding {
            *binding = map(binding);
        }
        *dependency_bindings = std::mem::take(dependency_bindings)
            .into_iter()
            .map(|binding| map(&binding))
            .collect();
        call_path.map_absolute_bindings(map);
    }

    pub fn into_saved_with_resolver(
        self,
        resolver: &mut CompositionSourceIdentityResolver,
    ) -> Result<SavedComposition, CompositionSourceIdentityError> {
        let Self {
            id,
            signature,
            binding,
            dependency_bindings,
            module_name,
            instance,
            call_path,
            quoted_string_attrs,
        } = self;
        let origin_id = resolver.saved_origin_id(&call_path)?;
        let LiveCompositionSignature {
            arguments,
            attributes,
            pending_constraints,
        } = signature;
        let arguments = arguments
            .into_iter()
            .map(|(name, argument)| {
                let LiveCompositionArgument { operand, metadata } = argument;
                let LiveCompositionArgumentMetadata {
                    declared_type,
                    origin,
                } = metadata;
                let discarded_live_metadata = (declared_type, origin);
                drop(discarded_live_metadata);
                (
                    name,
                    SavedCompositionArgument::from_value(operand.traversal_value().clone()),
                )
            })
            .collect();
        let attributes = attributes
            .into_iter()
            .map(|(name, attribute)| {
                let LiveCompositionAttribute {
                    value,
                    declared_type,
                    source_argument,
                } = attribute;
                let discarded_live_metadata = (declared_type, source_argument);
                drop(discarded_live_metadata);
                let saved_attribute = match value {
                    CompositionAttributeValue::Forwarded(path) => SavedCompositionAttribute {
                        value: CompositionAttributeValue::Forwarded(path),
                    },
                    CompositionAttributeValue::Derived(value) => SavedCompositionAttribute {
                        value: CompositionAttributeValue::Derived(value),
                    },
                };
                (name, saved_attribute)
            })
            .collect();
        drop(quoted_string_attrs);
        Ok(SavedComposition {
            id,
            signature: SavedCompositionSignature {
                arguments,
                attributes,
                pending_constraints,
            },
            binding,
            dependency_bindings,
            module_name,
            instance,
            origin_id,
        })
    }
}

/// Value-only composition persisted in a saved plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedComposition {
    pub id: ResolvedResourceId,
    #[serde(flatten)]
    pub signature: SavedCompositionSignature,
    #[serde(deserialize_with = "deserialize_present_option")]
    pub binding: Option<AbsoluteBindingAddress>,
    pub dependency_bindings: BTreeSet<AbsoluteBindingAddress>,
    pub module_name: String,
    pub instance: String,
    pub origin_id: SavedCompositionOriginId,
}

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl SavedComposition {
    pub fn ephemeral_id(&self) -> super::EphemeralId {
        super::EphemeralId::new(self.id.clone())
    }

    pub fn contains_redacted_secret_placeholder(&self) -> bool {
        let Self {
            id: _id,
            signature,
            binding: _binding,
            dependency_bindings: _dependency_bindings,
            module_name: _module_name,
            instance: _instance,
            origin_id: _origin_id,
        } = self;
        let SavedCompositionSignature {
            arguments,
            attributes,
            pending_constraints: _pending_constraints,
        } = signature;
        arguments
            .values()
            .any(SavedCompositionArgument::contains_redacted_secret_placeholder)
            || attributes
                .values()
                .any(SavedCompositionAttribute::contains_redacted_secret_placeholder)
    }
}

/// Private lifecycle-erased projection used only by core runtime algorithms.
/// Validation APIs deliberately accept `LiveComposition` directly instead.
#[derive(Clone, Copy)]
pub(crate) enum CompositionRuntimeRef<'a> {
    Live(&'a LiveComposition),
    Saved(&'a SavedComposition),
}

impl<'a> CompositionRuntimeRef<'a> {
    pub(crate) fn id(self) -> &'a ResolvedResourceId {
        match self {
            Self::Live(composition) => &composition.id,
            Self::Saved(composition) => &composition.id,
        }
    }

    pub(crate) fn binding(self) -> Option<&'a str> {
        self.binding_address().map(AbsoluteBindingAddress::as_str)
    }

    pub(crate) fn binding_address(self) -> Option<&'a AbsoluteBindingAddress> {
        match self {
            Self::Live(composition) => composition.binding.as_ref(),
            Self::Saved(composition) => composition.binding.as_ref(),
        }
    }

    pub(crate) fn dependency_bindings(self) -> &'a BTreeSet<AbsoluteBindingAddress> {
        match self {
            Self::Live(composition) => &composition.dependency_bindings,
            Self::Saved(composition) => &composition.dependency_bindings,
        }
    }

    pub(crate) fn module_name(self) -> &'a str {
        match self {
            Self::Live(composition) => &composition.module_name,
            Self::Saved(composition) => &composition.module_name,
        }
    }

    pub(crate) fn instance(self) -> &'a str {
        match self {
            Self::Live(composition) => &composition.instance,
            Self::Saved(composition) => &composition.instance,
        }
    }

    pub(crate) fn call_id(self) -> Option<&'a crate::parser::ModuleCallId> {
        match self {
            Self::Live(composition) => Some(&composition.call_path.immediate().id),
            Self::Saved(_) => None,
        }
    }

    pub(crate) fn arguments(self) -> CompositionRuntimeArguments<'a> {
        match self {
            Self::Live(composition) => {
                CompositionRuntimeArguments::Live(composition.signature.arguments.iter())
            }
            Self::Saved(composition) => {
                CompositionRuntimeArguments::Saved(composition.signature.arguments.iter())
            }
        }
    }

    pub(crate) fn attributes(self) -> CompositionRuntimeAttributes<'a> {
        match self {
            Self::Live(composition) => {
                CompositionRuntimeAttributes::Live(composition.signature.attributes.iter())
            }
            Self::Saved(composition) => {
                CompositionRuntimeAttributes::Saved(composition.signature.attributes.iter())
            }
        }
    }

    pub(crate) fn pending_constraints(self) -> &'a [PendingModuleConstraint] {
        match self {
            Self::Live(composition) => &composition.signature.pending_constraints,
            Self::Saved(composition) => &composition.signature.pending_constraints,
        }
    }
}

pub(crate) enum CompositionRuntimeArguments<'a> {
    Live(indexmap::map::Iter<'a, String, LiveCompositionArgument>),
    Saved(indexmap::map::Iter<'a, String, SavedCompositionArgument>),
}

impl<'a> Iterator for CompositionRuntimeArguments<'a> {
    type Item = (&'a String, &'a Value);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Live(arguments) => arguments
                .next()
                .map(|(name, argument)| (name, argument.value())),
            Self::Saved(arguments) => arguments
                .next()
                .map(|(name, argument)| (name, argument.value())),
        }
    }
}

pub(crate) enum CompositionRuntimeAttributes<'a> {
    Live(indexmap::map::Iter<'a, String, LiveCompositionAttribute>),
    Saved(indexmap::map::Iter<'a, String, SavedCompositionAttribute>),
}

impl<'a> Iterator for CompositionRuntimeAttributes<'a> {
    type Item = (&'a String, Value);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Live(attributes) => attributes
                .next()
                .map(|(name, attribute)| (name, attribute.to_value())),
            Self::Saved(attributes) => attributes
                .next()
                .map(|(name, attribute)| (name, attribute.to_value())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{CompareOp, TypeExpr, ValidateExpr};
    use crate::resource::{
        ExpandedConcreteValue as ConcreteValue, ExpandedDeferredValue as DeferredValue,
        ExpandedInterpolationPart as InterpolationPart, ExpandedValue as Value, ResourceId,
        RootScope,
    };

    fn resolved_type(type_expr: TypeExpr) -> ConsumerStaticType {
        crate::validation::resolve_consumer_static_type_for_signature(
            &type_expr,
            &crate::parser::ProviderContext::default(),
        )
    }

    #[test]
    fn equality_compares_call_path_identity_but_ignores_call_display() {
        let call_path = |source_file: &str, binding: &str| {
            CompositionCallPath::direct(CompositionCall {
                id: crate::parser::ModuleCallId::test_dummy_in(source_file),
                module_name: "module".to_string(),
                binding: Some(RootScope::new().resolve(binding.into())),
                instance: binding.to_string(),
                module_source: Some(format!("../{binding}")),
                module_directory: None,
                loop_origin: None,
            })
        };
        let left = LiveComposition {
            id: ResolvedResourceId::new(ResourceId::with_identity("_virtual", "call")),
            signature: LiveCompositionSignature {
                arguments: IndexMap::new(),
                attributes: IndexMap::new(),
                pending_constraints: Vec::new(),
            },
            binding: Some(RootScope::new().resolve("call".into())),
            dependency_bindings: BTreeSet::new(),
            module_name: "module".to_string(),
            instance: "call".to_string(),
            call_path: call_path("first.crn", "first"),
            quoted_string_attrs: HashSet::new(),
        };
        let mut right = left.clone();
        right.call_path = call_path("second.crn", "second");

        assert_ne!(left, right);
        assert_eq!(
            left.call_path()
                .immediate()
                .binding
                .as_ref()
                .map(AbsoluteBindingAddress::as_str),
            Some("first")
        );

        right.call_path = call_path("first.crn", "different display");
        assert_eq!(left, right);

        right.binding = Some(RootScope::new().resolve("different".into()));
        assert_ne!(left, right);
    }

    #[test]
    fn saved_source_path_rejects_empty_path() {
        let error = SourceRootRelativePath::try_from(PathBuf::new())
            .expect_err("an empty path cannot identify a source file");

        assert_eq!(error, SavedCompositionSourcePathError::Empty);
    }

    #[test]
    fn composition_source_identity_error_escapes_untrusted_paths() {
        let escaped_path = PathBuf::from("source\u{1b}[31m.crn");
        let escaped_root = PathBuf::from("root\nline");
        let error = CompositionSourceIdentityError::DifferentRoot {
            path: escaped_path,
            source_root: escaped_root,
        };

        let rendered = error.to_string();
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\n'), "{rendered:?}");
        assert!(rendered.contains(r"source\u{1b}[31m.crn"), "{rendered:?}");
        assert!(rendered.contains(r"root\nline"), "{rendered:?}");
    }

    #[test]
    fn from_value_resource_ref_classifies_as_forwarded() {
        let path = AccessPath::root("role", "arn");
        let v = Value::Deferred(DeferredValue::ResourceRef { path: path.clone() });
        let attr = LiveCompositionAttribute::from_value(v, None);
        assert_eq!(attr.forwarded_path(), Some(&path));
    }

    #[test]
    fn from_value_concrete_string_classifies_as_derived() {
        let v: Value = Value::Concrete(ConcreteValue::String("literal".into()));
        let attr = LiveCompositionAttribute::from_value(v.clone(), None);
        assert_eq!(attr.forwarded_path(), None);
        assert_eq!(attr.to_value(), v);
    }

    #[test]
    fn from_value_interpolation_classifies_as_derived() {
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::root("svc", "id"),
            })),
        ]));
        let attr = LiveCompositionAttribute::from_value(v.clone(), None);
        assert_eq!(attr.forwarded_path(), None);
        assert_eq!(attr.to_value(), v);
    }

    #[test]
    fn forwarded_to_value_is_resource_ref() {
        let path = AccessPath::root("svc", "endpoint");
        let attr = LiveCompositionAttribute::from_value(
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
        let inner: Value = Value::Concrete(ConcreteValue::String("kept".into()));
        let attr = LiveCompositionAttribute::from_value(inner.clone(), None);
        assert_eq!(attr.to_value(), inner);
    }

    /// Round-trip: a `Value` → `LiveCompositionAttribute` → `Value` is
    /// lossless for both `Forwarded`-lifted refs and `Derived`-wrapped
    /// expressions. This is the invariant the resolver / display
    /// layers rely on while migrating to per-variant dispatch.
    #[test]
    fn from_value_to_value_round_trip() {
        let cases = vec![
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::root("a", "b"),
            }),
            Value::Concrete(ConcreteValue::String("literal".into())),
            Value::Concrete(ConcreteValue::Int(42)),
        ];
        for original in cases {
            let lifted = LiveCompositionAttribute::from_value(original.clone(), None);
            assert_eq!(lifted.to_value(), original);
        }
    }

    #[test]
    fn declared_type_is_carried_with_value_rewrites() {
        let declared = TypeExpr::String;
        let attr = LiveCompositionAttribute::from_value(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::root("svc", "endpoint"),
            }),
            Some(resolved_type(declared.clone())),
        );

        let rewritten = attr.with_value(Value::Concrete(ConcreteValue::String("x".into())));

        assert_eq!(
            rewritten
                .declared_type()
                .and_then(|static_type| static_type.known()),
            Some(&declared)
        );
    }

    #[test]
    fn saved_attribute_round_trip_is_value_only() {
        let value = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root("svc", "endpoint"),
        });
        let saved = SavedCompositionAttribute::from_value(value.clone());

        let json = serde_json::to_value(&saved).expect("serialize saved attribute");
        let decoded: SavedCompositionAttribute =
            serde_json::from_value(json).expect("deserialize saved attribute");
        assert_eq!(decoded.to_value(), value);
    }

    #[test]
    fn argument_declared_type_is_carried_with_value_rewrites() {
        let declared = TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::provider(
                "aws",
                crate::schema::TypeIdentity::from_schema_type("aws", "ec2.Vpc", "Id"),
            ),
        };
        let argument = LiveCompositionArgument::from_value(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::root("vpc", "vpc_id"),
            }),
            resolved_type(declared.clone()),
            crate::parser::ArgumentOrigin::Defaulted,
        );

        let rewritten = argument.with_value(Value::Concrete(ConcreteValue::String("x".into())));

        assert_eq!(rewritten.declared_type().known(), Some(&declared));
    }

    #[test]
    fn saved_argument_round_trip_is_value_only() {
        let value = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root("vpc", "vpc_id"),
        });
        let saved = SavedCompositionArgument::from_value(value.clone());

        let saved_json = serde_json::to_value(&saved).expect("serialize saved argument");
        let value_json = serde_json::to_value(&value).expect("serialize argument value");
        assert_eq!(saved_json, value_json);

        let decoded: SavedCompositionArgument =
            serde_json::from_value(saved_json).expect("deserialize composition argument");
        assert_eq!(decoded.value(), &value);
    }

    #[test]
    fn pending_constraints_survive_serde_round_trip_and_secret_redaction() {
        let source_root = tempfile::tempdir().expect("source root");
        let constraint: PendingModuleConstraint = PendingModuleConstraint {
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
        let composition = LiveComposition {
            id: ResolvedResourceId::new(ResourceId::with_identity("_virtual", "secure")),
            signature: LiveCompositionSignature {
                arguments: IndexMap::from([(
                    "password".to_string(),
                    LiveCompositionArgument::from_value(
                        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                            ConcreteValue::String("plaintext-secret".into()),
                        )))),
                        resolved_type(TypeExpr::String),
                        crate::parser::ArgumentOrigin::Defaulted,
                    ),
                )]),
                attributes: IndexMap::new(),
                pending_constraints: vec![constraint.clone()],
            },
            binding: Some(RootScope::new().resolve("secure".into())),
            dependency_bindings: BTreeSet::new(),
            module_name: "secure_module".to_string(),
            instance: "secure".to_string(),
            call_path: Default::default(),
            quoted_string_attrs: HashSet::new(),
        };

        let mut source_identities = CompositionSourceIdentityResolver::new(source_root.path())
            .expect("create composition source identity resolver");
        let saved = composition
            .into_saved_with_resolver(&mut source_identities)
            .expect("save composition identity");
        let redacted = crate::value::redact_secrets_in_saved_composition(&saved)
            .expect("composition redaction must succeed");
        let json = serde_json::to_string(&redacted).expect("serialize redacted composition");
        assert!(!json.contains("plaintext-secret"), "{json}");
        assert!(json.contains("origin_id"), "{json}");
        assert!(!json.contains("declared_type"), "{json}");
        assert!(!json.contains("\"origin\":"), "{json}");
        assert!(!json.contains("call_path"), "{json}");
        let decoded: SavedComposition =
            serde_json::from_str(&json).expect("round-trip composition");
        assert_eq!(decoded.signature.pending_constraints, vec![constraint]);

        let mut missing_origin = serde_json::to_value(&redacted).expect("serialize composition");
        missing_origin
            .as_object_mut()
            .expect("composition object")
            .remove("origin_id");
        let error = serde_json::from_value::<SavedComposition>(missing_origin)
            .expect_err("saved composition origin must be mandatory");
        assert!(error.to_string().contains("origin_id"), "{error}");

        let complete = serde_json::to_value(&redacted).expect("serialize composition");
        for required in [
            "binding",
            "dependency_bindings",
            "arguments",
            "attributes",
            "pending_constraints",
        ] {
            let mut malformed = complete.clone();
            malformed
                .as_object_mut()
                .expect("composition object")
                .remove(required);
            let error = match serde_json::from_value::<SavedComposition>(malformed) {
                Ok(_) => panic!("missing {required} must be rejected"),
                Err(error) => error,
            };
            assert!(error.to_string().contains(required), "{required}: {error}");
        }
    }

    #[test]
    fn saved_origin_uses_source_root_relative_parent_segments_for_sibling_tree() {
        let source_root = tempfile::tempdir().expect("source root");
        let external_root = tempfile::tempdir().expect("external root");
        let source_file = external_root.path().join("module.crn");
        std::fs::write(
            &source_file,
            "let module = use { source = './module' }\nmodule {}\n",
        )
        .expect("write external source");
        let parsed = crate::parser::parse_file(
            "let module = use { source = './module' }\nmodule {}\n",
            &source_file,
            &crate::parser::ProviderContext::default(),
        )
        .expect("parse external module call");
        let call = &parsed.module_calls[0];
        let call_path = CompositionCallPath::direct(
            CompositionCall {
                id: call.id().clone(),
                module_name: call.module_name.clone(),
                binding: call.binding_name.clone(),
                instance: "module".to_string(),
                module_source: Some("./module".to_string()),
                module_directory: None,
                loop_origin: None,
            }
            .resolve_root(&RootScope::new()),
        );
        let mut source_identities = CompositionSourceIdentityResolver::new(source_root.path())
            .expect("create composition source identity resolver");
        let origin = source_identities
            .saved_origin_id(&call_path)
            .expect("save external origin");

        let relative = origin.root().source_file();
        assert!(
            relative.starts_with(".."),
            "a sibling module must be represented with parent segments: {}",
            relative.display(),
        );
        assert!(
            !relative.is_absolute(),
            "a saved module identity must never retain a host-absolute path: {}",
            relative.display(),
        );
        assert_eq!(
            source_root
                .path()
                .join(relative)
                .canonicalize()
                .expect("resolve saved relative source"),
            source_file
                .canonicalize()
                .expect("canonical external source"),
        );

        let json = serde_json::to_value(origin).expect("serialize external origin");
        assert_eq!(
            json["root"]["source_file"]["location"],
            "source_root_relative"
        );
        assert!(
            !json["root"]["source_file"]["path"]
                .as_str()
                .expect("saved source path string")
                .starts_with('/'),
            "serialized composition identity leaked an absolute path: {json}",
        );
    }

    #[test]
    fn saved_origin_rejects_a_source_path_that_cannot_be_canonicalized() {
        let source_root = tempfile::tempdir().expect("source root");
        let missing = source_root.path().join("missing.crn");
        let parsed = crate::parser::parse_file(
            "let module = use { source = './module' }\nmodule {}\n",
            &missing,
            &crate::parser::ProviderContext::default(),
        )
        .expect("parse call with a missing source identity");
        let call = &parsed.module_calls[0];

        let call_path = CompositionCallPath::direct(
            CompositionCall {
                id: call.id().clone(),
                module_name: call.module_name.clone(),
                binding: call.binding_name.clone(),
                instance: "module".to_string(),
                module_source: Some("./module".to_string()),
                module_directory: None,
                loop_origin: None,
            }
            .resolve_root(&RootScope::new()),
        );
        let mut source_identities = CompositionSourceIdentityResolver::new(source_root.path())
            .expect("create composition source identity resolver");
        let error = source_identities
            .saved_origin_id(&call_path)
            .expect_err("a missing source path must not mint a fallback identity");

        assert_eq!(
            error.to_string(),
            format!(
                "failed to canonicalize composition source path `{}`",
                missing.display()
            ),
        );
        let source = std::error::Error::source(&error)
            .expect("canonicalization failure must preserve its io source");
        assert!(
            source.downcast_ref::<std::io::Error>().is_some(),
            "composition source identity lost its typed io cause: {source:?}",
        );
    }
}
