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
//! non-provider resource). `module_source` is flattened to
//! `module_name` + `instance` — those are always set for compositions.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::parser::{TypeExpr, ValidateExpr};

use super::{AccessPath, DeferredValue, ResolvedResourceId, StructuralOperand, Value};

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
/// validation-only and is discarded explicitly by the exhaustive live-to-saved
/// conversion. Compositions are never persisted in state files; saved-plan
/// execution has already passed validation and therefore does not need this
/// metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveCompositionAttribute {
    value: CompositionAttributeValue,
    declared_type: Option<TypeExpr>,
    /// The module argument directly forwarded by this output before call-site
    /// substitution. Validation checks that declaration in the module's own
    /// scope; retaining the provenance prevents an expanded caller value from
    /// being checked again and blamed on the output declaration.
    source_argument: Option<String>,
}

impl LiveCompositionAttribute {
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
            CompositionAttributeValue::Forwarded(_path) => false,
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
    /// with a direct match on `LiveCompositionAttribute`.
    pub fn to_value(&self) -> Value {
        match &self.value {
            CompositionAttributeValue::Forwarded(path) => {
                Value::Deferred(DeferredValue::ResourceRef { path: path.clone() })
            }
            CompositionAttributeValue::Derived(v) => v.clone(),
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
            CompositionAttributeValue::Forwarded(path) => {
                let _path = path;
                false
            }
            CompositionAttributeValue::Derived(value) => {
                crate::value::contains_redacted_secret(value)
            }
        }
    }

    pub fn forwarded_path(&self) -> Option<&AccessPath> {
        match &self.value {
            CompositionAttributeValue::Forwarded(path) => Some(path),
            CompositionAttributeValue::Derived(value) => {
                let _value = value;
                None
            }
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
pub struct LiveCompositionArgument {
    operand: StructuralOperand,
    metadata: LiveCompositionArgumentMetadata,
}

#[derive(Debug, Clone, PartialEq)]
struct LiveCompositionArgumentMetadata {
    declared_type: TypeExpr,
    origin: crate::parser::ArgumentOrigin,
}

impl LiveCompositionArgument {
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
        value: Value,
        declared_type: TypeExpr,
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

    pub(crate) fn from_handle(
        value: Value,
        declared_type: TypeExpr,
        handle: crate::eval_value::StructuralHandle,
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

    pub fn value(&self) -> &Value {
        self.operand.traversal_value()
    }

    pub fn declared_type(&self) -> &TypeExpr {
        &self.metadata.declared_type
    }

    pub fn metadata(&self) -> (&TypeExpr, crate::parser::ArgumentOrigin) {
        (&self.metadata.declared_type, self.metadata.origin)
    }

    pub(crate) fn validation_metadata(&self) -> (&TypeExpr, crate::parser::ArgumentOrigin) {
        self.metadata()
    }

    pub(crate) fn is_handle(&self) -> bool {
        self.operand.handle().is_some()
    }

    pub(crate) fn handle(&self) -> Option<&crate::eval_value::StructuralHandle> {
        self.operand.handle()
    }

    pub(crate) fn forwarded_parameter(&self) -> Option<&str> {
        self.operand.parameter()
    }

    /// Rewrite a prefixed/substituted value without dropping its declaration.
    pub fn with_value(&self, value: Value) -> Self {
        let operand = match self.operand.handle() {
            Some(handle) => StructuralOperand::from_handle_with_access_base(handle.clone(), value),
            None => StructuralOperand::from_value(value),
        };
        Self {
            operand,
            metadata: self.metadata.clone(),
        }
    }

    pub(crate) fn with_handle(
        &self,
        value: Value,
        handle: crate::eval_value::StructuralHandle,
    ) -> Self {
        Self {
            operand: StructuralOperand::from_handle_with_access_base(handle, value),
            metadata: self.metadata.clone(),
        }
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
pub struct LiveCompositionSignature {
    /// Resolved call-site arguments. Empty when the composition was
    /// produced by a module that does not declare any `argument`
    /// parameters, or when the call site passed no arguments.
    pub arguments: IndexMap<String, LiveCompositionArgument>,
    /// Module-output values classified by how they are produced
    /// (#3294): forwarded values are single-hop aliases and derived values are
    /// multi-source expressions. Each entry also carries its optional declared
    /// [`TypeExpr`] for validation.
    pub attributes: IndexMap<String, LiveCompositionAttribute>,
    /// Constraints deferred until all referenced argument values are known.
    pub pending_constraints: Vec<PendingModuleConstraint>,
}

/// Value-only function boundary persisted in a saved plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedCompositionSignature {
    #[serde(default)]
    pub arguments: IndexMap<String, SavedCompositionArgument>,
    #[serde(default)]
    pub attributes: IndexMap<String, SavedCompositionAttribute>,
    #[serde(default)]
    pub pending_constraints: Vec<PendingModuleConstraint>,
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
pub struct CompositionCall {
    pub id: crate::parser::ModuleCallId,
    pub module_name: String,
    pub binding: Option<String>,
    pub instance: String,
    pub module_source: Option<String>,
    pub module_directory: Option<PathBuf>,
}

impl PartialEq for CompositionCall {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for CompositionCall {}

impl std::hash::Hash for CompositionCall {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.id, state);
    }
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

/// Complete non-empty module-call ancestry of one live composition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompositionCallPath {
    root: CompositionCall,
    descendants: Vec<CompositionCall>,
}

impl CompositionCallPath {
    pub fn direct(call: CompositionCall) -> Self {
        Self {
            root: call,
            descendants: Vec::new(),
        }
    }

    pub fn root(&self) -> &CompositionCall {
        &self.root
    }

    pub fn immediate(&self) -> &CompositionCall {
        self.descendants.last().unwrap_or(&self.root)
    }

    pub fn calls(&self) -> impl Iterator<Item = &CompositionCall> {
        std::iter::once(&self.root).chain(self.descendants.iter())
    }

    pub(crate) fn calls_mut(&mut self) -> impl Iterator<Item = &mut CompositionCall> {
        std::iter::once(&mut self.root).chain(self.descendants.iter_mut())
    }

    pub(crate) fn prepend(self, root: CompositionCall) -> Self {
        let Self {
            root: previous_root,
            descendants: previous_descendants,
        } = self;
        let mut descendants = Vec::with_capacity(previous_descendants.len() + 1);
        descendants.push(previous_root);
        descendants.extend(previous_descendants);
        Self { root, descendants }
    }

    pub fn saved_origin_id(&self, canonical_source_root: &Path) -> SavedCompositionOriginId {
        SavedCompositionOriginId::from_live(self, canonical_source_root)
    }
}

#[cfg(test)]
impl Default for CompositionCallPath {
    fn default() -> Self {
        Self::direct(CompositionCall {
            id: crate::parser::ModuleCallId::test_dummy(),
            module_name: "test_module".to_string(),
            binding: Some("test_call".to_string()),
            instance: "test_call".to_string(),
            module_source: None,
            module_directory: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "location", content = "path", rename_all = "snake_case")]
enum SavedSourceFileId {
    SourceRootRelative(PathBuf),
    External(PathBuf),
}

impl SavedSourceFileId {
    fn from_live(path: &Path, canonical_source_root: &Path) -> Self {
        let root = canonical_source_root
            .canonicalize()
            .unwrap_or_else(|error| {
                let _canonicalize_error = error;
                canonical_source_root.to_path_buf()
            });
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let source = joined.canonicalize().unwrap_or_else(|error| {
            let _canonicalize_error = error;
            joined
        });
        match source.strip_prefix(&root) {
            Ok(relative) => Self::SourceRootRelative(relative.to_path_buf()),
            Err(error) => {
                let _outside_source_root = error;
                Self::External(source)
            }
        }
    }

    fn display_path(&self) -> &Path {
        match self {
            Self::SourceRootRelative(path) | Self::External(path) => path,
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
    fn from_live(id: &crate::parser::ModuleCallId, canonical_source_root: &Path) -> Self {
        let expansion = match id.expansion() {
            crate::parser::CallExpansion::Direct => SavedCallExpansion::Direct,
            crate::parser::CallExpansion::Expanded(key) => {
                SavedCallExpansion::Expanded(key.as_str().to_string())
            }
        };
        Self {
            source_file: SavedSourceFileId::from_live(id.source_file(), canonical_source_root),
            call_span: id.call_span().into(),
            expansion,
        }
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
    fn from_live(path: &CompositionCallPath, canonical_source_root: &Path) -> Self {
        let CompositionCallPath { root, descendants } = path;
        let root = SavedModuleCallId::from_live(&root.id, canonical_source_root);
        let descendants = descendants
            .iter()
            .map(|call| SavedModuleCallId::from_live(&call.id, canonical_source_root))
            .collect();
        Self { root, descendants }
    }

    pub fn root(&self) -> &SavedModuleCallId {
        &self.root
    }

    pub fn immediate(&self) -> &SavedModuleCallId {
        self.descendants.last().unwrap_or(&self.root)
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
/// `module_source` is dropped — module metadata is flattened into
/// `module_name` + `instance`:
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
pub struct LiveComposition {
    pub id: ResolvedResourceId,
    pub signature: LiveCompositionSignature,
    /// Binding name from `let` bindings in DSL.
    pub binding: Option<String>,
    /// Binding names this composition depends on.
    pub dependency_bindings: BTreeSet<String>,
    /// Module name from the originating module-call expansion
    /// (e.g. "web_tier"). Always set for compositions — see #2516.
    pub module_name: String,
    /// Module instance binding name (e.g. "web").
    pub instance: String,
    pub call_path: CompositionCallPath,
    /// Parser-level: attributes whose value was written as a quoted
    /// string literal. Parse-time only; `#[serde(skip)]` keeps it out
    /// of state — mirrors [`Resource::quoted_string_attrs`](super::Resource).
    pub quoted_string_attrs: HashSet<String>,
}

impl PartialEq for LiveComposition {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.signature == other.signature
            && self.binding == other.binding
            && self.dependency_bindings == other.dependency_bindings
            && self.module_name == other.module_name
            && self.instance == other.instance
            && self.quoted_string_attrs == other.quoted_string_attrs
    }
}

impl LiveComposition {
    /// The composition's id wrapped as an [`EphemeralId`](super::EphemeralId).
    ///
    /// A composition is plan-scoped and never persists in state, so its
    /// id is `EphemeralId`-typed. By construction this id cannot enter
    /// state-load APIs that take `&PersistentId` — that mismatch is a
    /// compile error.
    pub fn ephemeral_id(&self) -> super::EphemeralId {
        super::EphemeralId::new(self.id.clone())
    }

    pub fn call_path(&self) -> &CompositionCallPath {
        &self.call_path
    }

    pub fn into_saved(self, canonical_source_root: &Path) -> SavedComposition {
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
        let origin_id = call_path.saved_origin_id(canonical_source_root);
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
        SavedComposition {
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
        }
    }
}

/// Value-only composition persisted in a saved plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedComposition {
    pub id: ResolvedResourceId,
    #[serde(flatten)]
    pub signature: SavedCompositionSignature,
    #[serde(default)]
    pub binding: Option<String>,
    #[serde(default)]
    pub dependency_bindings: BTreeSet<String>,
    pub module_name: String,
    pub instance: String,
    pub origin_id: SavedCompositionOriginId,
}

impl SavedComposition {
    pub fn ephemeral_id(&self) -> super::EphemeralId {
        super::EphemeralId::new(self.id.clone())
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
        match self {
            Self::Live(composition) => composition.binding.as_deref(),
            Self::Saved(composition) => composition.binding.as_deref(),
        }
    }

    pub(crate) fn dependency_bindings(self) -> &'a BTreeSet<String> {
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
    use crate::parser::{CompareOp, ValidateExpr};
    use crate::resource::{ConcreteValue, ResourceId};

    #[test]
    fn equality_ignores_call_path_display_but_compares_composition_fields() {
        let call_path = |binding: &str| {
            CompositionCallPath::direct(CompositionCall {
                id: crate::parser::ModuleCallId::test_dummy(),
                module_name: "module".to_string(),
                binding: Some(binding.to_string()),
                instance: binding.to_string(),
                module_source: Some(format!("../{binding}")),
                module_directory: None,
            })
        };
        let left = LiveComposition {
            id: ResolvedResourceId::new(ResourceId::with_identity("_virtual", "call")),
            signature: LiveCompositionSignature {
                arguments: IndexMap::new(),
                attributes: IndexMap::new(),
                pending_constraints: Vec::new(),
            },
            binding: Some("call".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "module".to_string(),
            instance: "call".to_string(),
            call_path: call_path("first"),
            quoted_string_attrs: HashSet::new(),
        };
        let mut right = left.clone();
        right.call_path = call_path("second");

        assert_eq!(left, right);
        assert_eq!(
            left.call_path().immediate().binding.as_deref(),
            Some("first")
        );

        right.binding = Some("different".to_string());
        assert_ne!(left, right);
    }

    #[test]
    fn from_value_resource_ref_classifies_as_forwarded() {
        let path = AccessPath::new("role", "arn");
        let v = Value::Deferred(DeferredValue::ResourceRef { path: path.clone() });
        let attr = LiveCompositionAttribute::from_value(v, None);
        assert_eq!(attr.forwarded_path(), Some(&path));
    }

    #[test]
    fn from_value_concrete_string_classifies_as_derived() {
        let v = Value::Concrete(ConcreteValue::String("literal".into()));
        let attr = LiveCompositionAttribute::from_value(v.clone(), None);
        assert_eq!(attr.forwarded_path(), None);
        assert_eq!(attr.to_value(), v);
    }

    #[test]
    fn from_value_interpolation_classifies_as_derived() {
        use crate::resource::InterpolationPart;
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("svc", "id"),
            })),
        ]));
        let attr = LiveCompositionAttribute::from_value(v.clone(), None);
        assert_eq!(attr.forwarded_path(), None);
        assert_eq!(attr.to_value(), v);
    }

    #[test]
    fn forwarded_to_value_is_resource_ref() {
        let path = AccessPath::new("svc", "endpoint");
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
        let inner = Value::Concrete(ConcreteValue::String("kept".into()));
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
                path: AccessPath::new("a", "b"),
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
                path: AccessPath::new("svc", "endpoint"),
            }),
            Some(declared.clone()),
        );

        let rewritten = attr.with_value(Value::Concrete(ConcreteValue::String("x".into())));

        assert_eq!(rewritten.declared_type(), Some(&declared));
    }

    #[test]
    fn saved_attribute_round_trip_is_value_only() {
        let value = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("svc", "endpoint"),
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
            provider: "aws".to_string(),
            path: "ec2.Vpc".to_string(),
            type_name: "Id".to_string(),
        };
        let argument = LiveCompositionArgument::from_value(
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("vpc", "vpc_id"),
            }),
            declared.clone(),
            crate::parser::ArgumentOrigin::Defaulted,
        );

        let rewritten = argument.with_value(Value::Concrete(ConcreteValue::String("x".into())));

        assert_eq!(rewritten.declared_type(), &declared);
    }

    #[test]
    fn saved_argument_round_trip_is_value_only() {
        let value = Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new("vpc", "vpc_id"),
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
        let composition = LiveComposition {
            id: ResolvedResourceId::new(ResourceId::with_identity("_virtual", "secure")),
            signature: LiveCompositionSignature {
                arguments: IndexMap::from([(
                    "password".to_string(),
                    LiveCompositionArgument::from_value(
                        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                            ConcreteValue::String("plaintext-secret".into()),
                        )))),
                        TypeExpr::String,
                        crate::parser::ArgumentOrigin::Defaulted,
                    ),
                )]),
                attributes: IndexMap::new(),
                pending_constraints: vec![constraint.clone()],
            },
            binding: Some("secure".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "secure_module".to_string(),
            instance: "secure".to_string(),
            call_path: Default::default(),
            quoted_string_attrs: HashSet::new(),
        };

        let saved = composition.into_saved(Path::new("/tmp/project"));
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
    }

    #[test]
    fn saved_origin_uses_explicit_external_source_representation() {
        let parsed = crate::parser::parse_file(
            "let module = use { source = './module' }\nmodule {}\n",
            Path::new("/outside/module.crn"),
            &crate::parser::ProviderContext::default(),
        )
        .expect("parse external module call");
        let call = &parsed.module_calls[0];
        let origin = CompositionCallPath::direct(CompositionCall {
            id: call.id().clone(),
            module_name: call.module_name.clone(),
            binding: call.binding_name.clone(),
            instance: "module".to_string(),
            module_source: Some("./module".to_string()),
            module_directory: None,
        })
        .saved_origin_id(Path::new("/project"));

        let json = serde_json::to_value(origin).expect("serialize external origin");
        assert_eq!(json["root"]["source_file"]["location"], "external");
        assert_eq!(json["root"]["source_file"]["path"], "/outside/module.crn");
    }
}
