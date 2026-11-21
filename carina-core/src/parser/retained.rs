//! Authored expressions and their provisional projections live on separate axes.
//!
//! The syntax tree is captured before evaluation, including operands that an
//! enclosing operation does not evaluate. Only the judge can mint admission
//! evidence. A projection is useful for identities and expansion, but cannot
//! be turned back into an authored expression.

use super::resource_body::{AuthoredSlotKind, resource_child_kind};
use super::{DeclarationLocation, Located, PreprocessedFile, Rule, TypeExpr};
use crate::resource::DiagnosticScope;
use crate::resource::InsideSecret;
use crate::resource::{
    BindingAddress, ParserPlaceholder, PathSegment, RelativeBindingName, Value, ValueKind,
};
use indexmap::IndexMap;
use pest::iterators::{Pair, Pairs};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, OnceLock},
};

#[cfg(test)]
mod conversion_tests;
#[cfg(test)]
mod review_round2_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static PROJECTION_COPY_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SECRECY_INDEX_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn count_secrecy_index_builds<T>(run: impl FnOnce() -> T) -> (T, usize) {
    SECRECY_INDEX_BUILDS.set(0);
    let result = run();
    (result, SECRECY_INDEX_BUILDS.get())
}

#[cfg(test)]
pub(crate) fn count_projection_copy_work<T>(run: impl FnOnce() -> T) -> (T, usize) {
    PROJECTION_COPY_WORK.set(0);
    let result = run();
    (result, PROJECTION_COPY_WORK.get())
}

#[cfg(test)]
fn record_projection_copy(value: &Value<ParserPlaceholder, RelativeBindingName>) {
    fn size(value: &Value<ParserPlaceholder, RelativeBindingName>) -> usize {
        1 + match value {
            Value::Concrete(crate::resource::ConcreteValue::List(values)) => {
                values.iter().map(size).sum()
            }
            Value::Concrete(crate::resource::ConcreteValue::Map(values)) => {
                values.values().map(size).sum()
            }
            Value::Concrete(_) | Value::Deferred(_) => 0,
            Value::Reference(never) | Value::Conversion(never) => match *never {},
        }
    }
    PROJECTION_COPY_WORK.update(|work| work + size(value));
}

pub(crate) mod conversion;
pub(crate) use conversion::ConversionContext;
pub(crate) use conversion::ConversionVerdict;
pub(crate) use conversion::TransportJudgement;
pub(crate) use conversion::check as check_conversion;
pub use conversion::{ConversionJudgement, TemplateDeferral};

mod cursor;
pub use cursor::{
    AuthoredCursor, AuthoredSlots, AuthoredUse, CompletenessReport, EdgeRole, ExpansionInstanceId,
    JudgedNodeRef, JudgedSlot, NoAuthoredEvidence, NodeId, ProjectedPhase, ProjectionGeneration,
    SlotDeclarationId, UnreachedSlot, UseId,
};
pub(crate) use cursor::{
    AuthoredForm, AuthoredNavigation, CursorKind, ProjectionAuthority, RowAttributes,
    SourceReference, SyntheticSource, ValidationProjection,
};

/// The existing authored walk owns source diagnostics for expressions that
/// require its policy. This is diagnostic ownership, not an admission proof.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BindingDiagnosticOwner<'a> {
    OrdinarySource(OrdinaryJudgedExpr<'a>),
    JudgedConversion(Arc<StaticSourceReceipt>),
    WalkOwnedConversion,
    AuthoredExpression,
}

/// The expansion scope is part of expression identity, even when two module
/// instances originate at the same authored source location.
#[derive(Debug, Clone, Default)]
pub(crate) enum ExpressionInstance {
    #[default]
    Root,
    Module(Arc<ExpressionEnvironment>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ExpressionEnvironmentId(u64);

pub(crate) struct ExpressionEnvironment {
    id: ExpressionEnvironmentId,
    scope: Arc<str>,
}

impl std::fmt::Debug for ExpressionEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.scope.fmt(f)
    }
}

impl PartialEq for ExpressionInstance {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}
impl Eq for ExpressionInstance {}
impl std::hash::Hash for ExpressionInstance {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id().hash(state);
    }
}

/// A failed evidence lookup is an invariant error, never deferred authority.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("missing expression evidence: {request}")]
pub struct MissingEvidence {
    request: String,
}
impl MissingEvidence {
    pub(crate) fn new(request: impl Into<String>) -> Self {
        Self {
            request: request.into(),
        }
    }
    pub fn for_resource(id: &crate::resource::ResourceId) -> Self {
        Self::new(format!("resource {id}"))
    }
}

impl ExpressionInstance {
    pub(crate) fn for_scope(scope: &crate::resource::ModuleInstanceScope) -> Self {
        #[derive(Default)]
        struct Environments {
            scopes: HashMap<Arc<str>, std::sync::Weak<ExpressionEnvironment>>,
            next_cleanup: usize,
        }
        static ENVIRONMENTS: std::sync::OnceLock<std::sync::Mutex<Environments>> =
            std::sync::OnceLock::new();
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut environments = ENVIRONMENTS
            .get_or_init(Default::default)
            .lock()
            .expect("expression environments lock");
        if let Some(environment) = environments
            .scopes
            .get(scope.key())
            .and_then(std::sync::Weak::upgrade)
        {
            return Self::Module(environment);
        }
        if environments.scopes.len() >= environments.next_cleanup {
            environments
                .scopes
                .retain(|_, environment| environment.strong_count() != 0);
            environments.next_cleanup = environments.scopes.len().saturating_mul(2).max(64);
        }
        let environment = Arc::new(ExpressionEnvironment {
            id: ExpressionEnvironmentId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
            scope: Arc::from(scope.key()),
        });
        environments
            .scopes
            .insert(environment.scope.clone(), Arc::downgrade(&environment));
        Self::Module(environment)
    }

    pub(crate) fn id(&self) -> ExpressionEnvironmentId {
        match self {
            Self::Root => ExpressionEnvironmentId(0),
            Self::Module(environment) => environment.id,
        }
    }
}

/// Parser decisions are versioned independently of provisional Values. Each
/// slot takes a constant-size view of this shared history, so later loop
/// iterations cannot change the branch that produced an earlier projection.
#[derive(Debug, Default)]
struct ConditionHistory {
    version: usize,
    values: HashMap<NodeId, Vec<(usize, bool)>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ConditionSnapshot {
    history: Option<Arc<std::sync::RwLock<ConditionHistory>>>,
    version: usize,
    previous: Option<Arc<ConditionSnapshot>>,
}

impl ConditionSnapshot {
    pub(crate) fn get(&self, id: NodeId) -> Option<bool> {
        let mut snapshot = self;
        loop {
            if let Some(history) = &snapshot.history {
                let history = history.read().expect("retained conditions lock");
                if let Some(selected) = history.values.get(&id).and_then(|values| {
                    let index = values.partition_point(|(version, _)| *version <= snapshot.version);
                    index.checked_sub(1).map(|index| values[index].1)
                }) {
                    return Some(selected);
                }
            }
            snapshot = snapshot.previous.as_deref()?;
        }
    }

    fn over(&self, previous: &Self) -> Self {
        if self.history.is_none() && self.previous.is_none() {
            return previous.clone();
        }
        let mut snapshot = self.clone();
        snapshot.previous = (previous.history.is_some() || previous.previous.is_some())
            .then(|| Arc::new(previous.clone()));
        snapshot
    }
}

impl Drop for ConditionSnapshot {
    fn drop(&mut self) {
        let mut previous = self.previous.take();
        while let Some(snapshot) = previous {
            previous = Arc::into_inner(snapshot).and_then(|mut snapshot| snapshot.previous.take());
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ConditionRecorder(Arc<std::sync::RwLock<ConditionHistory>>);

impl ConditionRecorder {
    pub(crate) fn extend(&self, decisions: impl IntoIterator<Item = (NodeId, bool)>) {
        let mut history = self.0.write().expect("retained conditions lock");
        for (id, selected) in decisions {
            history.version += 1;
            let version = history.version;
            history
                .values
                .entry(id)
                .or_default()
                .push((version, selected));
        }
    }

    fn snapshot(&self) -> ConditionSnapshot {
        let version = self.0.read().expect("retained conditions lock").version;
        ConditionSnapshot {
            history: (version != 0).then(|| self.0.clone()),
            version,
            previous: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
/// A diagnostic range. Never use this value as authored admission identity.
pub struct ExpressionLocation {
    instance: ExpressionInstance,
    authored: DeclarationLocation,
}

impl ExpressionLocation {
    pub fn declaration(&self) -> &DeclarationLocation {
        &self.authored
    }

    fn new(instance: ExpressionInstance, authored: &DeclarationLocation) -> Self {
        Self {
            instance,
            authored: authored.clone(),
        }
    }
}

/// An expression retains its complete authored root and provisional payload.
/// Neither field nor a constructor from a reduced `Value` is public.
///
/// ```compile_fail,E0277
/// use carina_core::parser::retained::ExprValue;
/// use carina_core::resource::{AuthoredValue, Value, ConcreteValue};
/// let reduced: AuthoredValue = Value::Concrete(ConcreteValue::String("selected".into()));
/// let expression: ExprValue = reduced.into();
/// ```
///
/// ```compile_fail,E0616
/// use carina_core::parser::retained::ExprValue;
/// fn discard_authorship(expression: ExprValue) { let _ = expression.payload; }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ExprValue<P: ValueKind = ParserPlaceholder, B: BindingAddress = RelativeBindingName> {
    authored: Arc<AuthoredExpr<P, B>>,
    payload: Projection<P, B>,
}

#[derive(Debug, Clone, PartialEq)]
enum Projection<P: ValueKind, B: BindingAddress> {
    Pending,
    Value(Value<P, B>),
}

impl<P: ValueKind, B: BindingAddress> Projection<P, B> {
    fn map_bindings<C: BindingAddress>(self, map: &mut impl FnMut(B) -> C) -> Projection<P, C> {
        match self {
            Self::Pending => Projection::Pending,
            Self::Value(value) => Projection::Value(value.map_bindings(map)),
        }
    }
}

#[derive(Clone)]
struct AuthoredExpr<P: ValueKind, B: BindingAddress> {
    id: NodeId,
    definition_use: UseId,
    location: ExpressionLocation,
    kind: ExprKind<B>,
    roles: AuthoredChildRoles,
    secrecy_operator: Option<DeclarationLocation>,
    first_conversion: Option<Arc<ConversionSite>>,
    parse_ownership: ParseOwnership,
    children: Vec<ExprValue<P, B>>,
}

// Drain the last-owned syntax edges on the heap. Shared nodes are left for
// their last owner; each node whose Arc is consumed drops with no children.
impl<P: ValueKind, B: BindingAddress> Drop for AuthoredExpr<P, B> {
    fn drop(&mut self) {
        let mut pending = std::mem::take(&mut self.children);
        while let Some(child) = pending.pop() {
            if let Some(mut node) = Arc::into_inner(child.authored) {
                pending.append(&mut node.children);
            }
        }
    }
}

// Named child handles are identity metadata, independent of syntax equality.
#[derive(Clone)]
enum AuthoredChildRoles {
    Positional,
    If {
        condition: NodeId,
    },
    Computed,
    Function {
        body: NodeId,
    },
    Sequence {
        result: Option<NodeId>,
    },
    Sink {
        value: NodeId,
        key: Option<NodeId>,
        placement: SinkPlacement,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkPlacement {
    Value,
    Boundary,
}

// Constant-size diagnostic summaries are computed while the parser constructs
// each node. They are never admission authority or a syntax/value join.
#[derive(Debug, Clone)]
pub(crate) struct ConversionSite {
    node: NodeId,
    location: DeclarationLocation,
    parsing: conversion::ConversionParsing,
}
#[derive(Debug, Clone, Copy)]
enum ParseOwnership {
    Conversion,
    Covered,
    Ordinary,
}
/// Source dependency occurrences, before alias substitution and redaction.
/// Identity and diagnostic spelling remain separate across module expansion.
pub(crate) enum AuthoredDependency<B: BindingAddress> {
    Binding {
        binding: B,
        authored_binding: RelativeBindingName,
        scope: DiagnosticScope,
    },
    Reference {
        identity: crate::resource::AccessPath<B>,
        authored: crate::resource::AccessPath<RelativeBindingName>,
        scope: DiagnosticScope,
    },
}

impl<P: ValueKind, B: BindingAddress> AuthoredExpr<P, B> {
    fn dependency_reference(
        &self,
    ) -> Option<(
        crate::resource::AccessPath<B>,
        crate::resource::AccessPath<RelativeBindingName>,
    )> {
        match &self.kind {
            ExprKind::Reference {
                binding,
                authored_binding,
                attribute,
                segments,
            } => Some((
                crate::resource::AccessPath::from_binding(
                    binding.clone(),
                    attribute.clone(),
                    segments.clone(),
                ),
                crate::resource::AccessPath::from_binding(
                    authored_binding.clone(),
                    attribute.clone(),
                    segments.clone(),
                ),
            )),
            ExprKind::Access(step) => {
                let (identity, authored) =
                    self.children.first()?.authored.dependency_reference()?;
                Some((
                    identity.with_appended_segment(step.clone()),
                    authored.with_appended_segment(step.clone()),
                ))
            }
            _ => None,
        }
    }

    fn dependency_uses(&self, scope: DiagnosticScope, output: &mut Vec<AuthoredDependency<B>>) {
        let scope = if matches!(self.kind, ExprKind::Secret) {
            DiagnosticScope::Secret
        } else {
            scope
        };
        if let Some((identity, authored)) = self.dependency_reference() {
            output.push(AuthoredDependency::Reference {
                identity,
                authored,
                scope,
            });
        } else if let ExprKind::Binding {
            binding,
            authored_binding,
        } = &self.kind
        {
            output.push(AuthoredDependency::Binding {
                binding: binding.clone(),
                authored_binding: authored_binding.clone(),
                scope,
            });
        } else {
            for child in &self.children {
                child.authored.dependency_uses(scope, output);
            }
        }
    }

    fn conversion_role(&self) -> conversion::flow::PlacementRole {
        match self.roles {
            AuthoredChildRoles::Computed => conversion::flow::PlacementRole::Computed,
            AuthoredChildRoles::Sink {
                placement: SinkPlacement::Boundary,
                ..
            } => conversion::flow::PlacementRole::Boundary,
            AuthoredChildRoles::Positional
            | AuthoredChildRoles::If { .. }
            | AuthoredChildRoles::Function { .. }
            | AuthoredChildRoles::Sequence { .. }
            | AuthoredChildRoles::Sink {
                placement: SinkPlacement::Value,
                ..
            } => conversion::flow::PlacementRole::of(&self.kind),
        }
    }
    fn condition_index(&self) -> Option<usize> {
        match self.roles {
            AuthoredChildRoles::If { condition } => self
                .children
                .iter()
                .position(|child| child.node_id() == condition),
            AuthoredChildRoles::Positional
            | AuthoredChildRoles::Computed
            | AuthoredChildRoles::Function { .. }
            | AuthoredChildRoles::Sequence { .. }
            | AuthoredChildRoles::Sink { .. } => None,
        }
    }
    fn condition(&self) -> Option<&ExprValue<P, B>> {
        self.condition_index()
            .and_then(|index| self.children.get(index))
    }
    fn result_index(&self) -> Option<usize> {
        match self.roles {
            AuthoredChildRoles::Sequence { result } => result.and_then(|result| {
                self.children
                    .iter()
                    .position(|child| child.node_id() == result)
            }),
            AuthoredChildRoles::Positional
            | AuthoredChildRoles::If { .. }
            | AuthoredChildRoles::Computed
            | AuthoredChildRoles::Function { .. }
            | AuthoredChildRoles::Sink { .. } => None,
        }
    }
    fn is_bare_source(&self) -> bool {
        match &self.kind {
            ExprKind::Binding { .. } => true,
            ExprKind::Alias | ExprKind::Parens | ExprKind::Definition => {
                match self.children.as_slice() {
                    [child] => child.authored.is_bare_source(),
                    [] | [_, _, ..] => false,
                }
            }
            ExprKind::Scalar(_)
            | ExprKind::Parameter(_)
            | ExprKind::LoopSlot(_)
            | ExprKind::UntypedParameter
            | ExprKind::Reference { .. }
            | ExprKind::List
            | ExprKind::Map(_)
            | ExprKind::NestedBlock { .. }
            | ExprKind::Interpolation { .. }
            | ExprKind::EmptyInterpolation
            | ExprKind::Secret
            | ExprKind::Call(_)
            | ExprKind::Function { .. }
            | ExprKind::Sequence
            | ExprKind::Access(_)
            | ExprKind::Integer { .. }
            | ExprKind::Convert { .. }
            | ExprKind::Coalesce
            | ExprKind::Element
            | ExprKind::Iterable
            | ExprKind::Resource { .. }
            | ExprKind::Sink { .. }
            | ExprKind::If
            | ExprKind::For { .. }
            | ExprKind::Wait
            | ExprKind::Compose
            | ExprKind::DynamicAccess
            | ExprKind::ModuleCall
            | ExprKind::Boolean
            | ExprKind::Declaration
            | ExprKind::RequireCondition
            | ExprKind::ProviderDeclaration
            | ExprKind::Unsupported(_) => false,
        }
    }
    fn sink_value(&self) -> Option<&ExprValue<P, B>> {
        let AuthoredChildRoles::Sink { value, .. } = self.roles else {
            return None;
        };
        self.children.iter().find(|child| child.node_id() == value)
    }
    fn key_node(&self) -> Option<NodeId> {
        match self.roles {
            AuthoredChildRoles::Sink { key, .. } => key,
            AuthoredChildRoles::Positional
            | AuthoredChildRoles::If { .. }
            | AuthoredChildRoles::Computed
            | AuthoredChildRoles::Function { .. }
            | AuthoredChildRoles::Sequence { .. } => None,
        }
    }
    fn parse_failure(&self, ordinary: super::ParseError) -> super::ParseError {
        if matches!(self.parse_ownership, ParseOwnership::Conversion)
            && let Some(site) = &self.first_conversion
        {
            site.parsing.operand_failed();
        }
        ordinary
    }
}

type AuthoredNodes<B> =
    IndexMap<(ExpressionInstance, NodeId), Arc<AuthoredExpr<ParserPlaceholder, B>>>;

// Syntax equality predates identity. Keep its existing semantics for callers
// comparing parser output; admission uses NodeId explicitly.
impl<P: ValueKind, B: BindingAddress> PartialEq for AuthoredExpr<P, B> {
    fn eq(&self, other: &Self) -> bool {
        self.location == other.location
            && self.kind == other.kind
            && self.children == other.children
    }
}

impl<P: ValueKind, B: BindingAddress> std::fmt::Debug for AuthoredExpr<P, B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("AuthoredExpr");
        debug
            .field("location", &self.location)
            .field("kind", &self.kind);
        if !matches!(self.kind, ExprKind::Secret) {
            debug.field("children", &self.children);
        }
        debug.finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ExprKind<B: BindingAddress> {
    Convert {
        target: Box<Located<crate::conversion::ConversionTypePath>>,
        parsing: conversion::ConversionParsing,
    },
    Scalar(TypeExpr),
    RequireCondition,
    Parameter(TypeExpr),
    LoopSlot(TypeExpr),
    UntypedParameter,
    Binding {
        binding: B,
        authored_binding: RelativeBindingName,
    },
    Reference {
        binding: B,
        authored_binding: RelativeBindingName,
        attribute: String,
        segments: Vec<PathSegment>,
    },
    List,
    Map(Vec<super::AttributeKey>),
    NestedBlock {
        name: super::AttributeKey,
        header: DeclarationLocation,
    },
    Interpolation {
        single_reference: bool,
    },
    EmptyInterpolation,
    Coalesce,
    Secret,
    Call(String),
    Function {
        name: String,
        signature: Arc<FunctionSignature>,
    },
    Sequence,
    Access(PathSegment),
    Integer {
        op: crate::integer_arithmetic::IntegerOp,
        location: DeclarationLocation,
    },
    Alias,
    Parens,
    Definition,
    Element,
    Iterable,
    Resource {
        provider: String,
        resource_type: String,
        read: bool,
    },
    Sink {
        name: super::AttributeKey,
    },
    If,
    For {
        binding: super::ForBinding,
        bound: bool,
        iterable: super::AuthoredExpressionText,
    },
    Wait,
    Compose,
    DynamicAccess,
    ModuleCall,
    Boolean,
    Declaration,
    ProviderDeclaration,
    Unsupported(String),
}

#[derive(Debug, Clone)]
pub(crate) struct FunctionSignature {
    parameters: Vec<(String, Option<TypeExpr>)>,
    return_type: Option<TypeExpr>,
    pub(crate) parameter_nodes: Vec<NodeId>,
    pub(crate) default_nodes: Vec<Option<NodeId>>,
}

impl PartialEq for FunctionSignature {
    fn eq(&self, other: &Self) -> bool {
        self.parameters == other.parameters && self.return_type == other.return_type
    }
}

impl<P: ValueKind, B: BindingAddress> ExprValue<P, B> {
    pub fn node_id(&self) -> NodeId {
        self.authored.id
    }

    pub(crate) fn secrecy_kind(&self) -> &ExprKind<B> {
        &self.authored.kind
    }
    pub(crate) fn secrecy_children(&self) -> &[Self] {
        &self.authored.children
    }
    pub(crate) fn secrecy_instance(&self) -> &ExpressionInstance {
        &self.authored.location.instance
    }
    pub(crate) fn secrecy_operator(&self) -> &DeclarationLocation {
        self.authored
            .secrecy_operator
            .as_ref()
            .unwrap_or(self.location())
    }
    pub(crate) fn secrecy_sink(&self) -> Option<&Self> {
        self.authored.sink_value()
    }
    pub(crate) fn secrecy_condition(&self) -> Option<usize> {
        self.authored.condition_index()
    }
    pub(crate) fn secrecy_function_body(&self) -> Option<&Self> {
        match self.authored.roles {
            AuthoredChildRoles::Function { body } => self
                .authored
                .children
                .iter()
                .find(|child| child.node_id() == body),
            AuthoredChildRoles::Positional
            | AuthoredChildRoles::If { condition: _ }
            | AuthoredChildRoles::Computed
            | AuthoredChildRoles::Sequence { result: _ }
            | AuthoredChildRoles::Sink {
                value: _,
                key: _,
                placement: _,
            } => None,
        }
    }

    pub fn projection(&self) -> Option<&Value<P, B>> {
        match &self.payload {
            Projection::Value(value) => Some(value),
            Projection::Pending => None,
        }
    }

    pub fn location(&self) -> &DeclarationLocation {
        &self.authored.location.authored
    }

    fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
        memo: &mut HashMap<usize, Arc<AuthoredExpr<P, C>>>,
    ) -> ExprValue<P, C> {
        let id = Arc::as_ptr(&self.authored) as usize;
        if let Some(authored) = memo.get(&id) {
            return ExprValue {
                authored: authored.clone(),
                payload: Projection::Pending,
            };
        }
        let mut authored = Arc::unwrap_or_clone(self.authored);
        let kind = match std::mem::replace(&mut authored.kind, ExprKind::Declaration) {
            ExprKind::Convert { target, parsing } => ExprKind::Convert { target, parsing },
            ExprKind::Binding {
                binding,
                authored_binding,
            } => ExprKind::Binding {
                binding: map(binding),
                authored_binding,
            },
            ExprKind::Reference {
                binding,
                authored_binding,
                attribute,
                segments,
            } => ExprKind::Reference {
                binding: map(binding),
                authored_binding,
                attribute,
                segments,
            },
            ExprKind::Scalar(ty) => ExprKind::Scalar(ty),
            ExprKind::RequireCondition => ExprKind::RequireCondition,
            ExprKind::Parameter(ty) => ExprKind::Parameter(ty),
            ExprKind::LoopSlot(ty) => ExprKind::LoopSlot(ty),
            ExprKind::UntypedParameter => ExprKind::UntypedParameter,
            ExprKind::List => ExprKind::List,
            ExprKind::Map(keys) => ExprKind::Map(keys),
            ExprKind::NestedBlock { name, header } => ExprKind::NestedBlock { name, header },
            ExprKind::Interpolation { single_reference } => {
                ExprKind::Interpolation { single_reference }
            }
            ExprKind::EmptyInterpolation => ExprKind::EmptyInterpolation,
            ExprKind::Coalesce => ExprKind::Coalesce,
            ExprKind::Secret => ExprKind::Secret,
            ExprKind::Call(name) => ExprKind::Call(name),
            ExprKind::Function { name, signature } => ExprKind::Function { name, signature },
            ExprKind::Sequence => ExprKind::Sequence,
            ExprKind::Access(step) => ExprKind::Access(step),
            ExprKind::Integer { op, location } => ExprKind::Integer { op, location },
            ExprKind::Alias => ExprKind::Alias,
            ExprKind::Parens => ExprKind::Parens,
            ExprKind::Definition => ExprKind::Definition,
            ExprKind::Element => ExprKind::Element,
            ExprKind::Iterable => ExprKind::Iterable,
            ExprKind::Resource {
                provider,
                resource_type,
                read,
            } => ExprKind::Resource {
                provider,
                resource_type,
                read,
            },
            ExprKind::Sink { name } => ExprKind::Sink { name },
            ExprKind::If => ExprKind::If,
            ExprKind::For {
                binding,
                bound,
                iterable,
            } => ExprKind::For {
                binding,
                bound,
                iterable,
            },
            ExprKind::Wait => ExprKind::Wait,
            ExprKind::Compose => ExprKind::Compose,
            ExprKind::DynamicAccess => ExprKind::DynamicAccess,
            ExprKind::ModuleCall => ExprKind::ModuleCall,
            ExprKind::Boolean => ExprKind::Boolean,
            ExprKind::Declaration => ExprKind::Declaration,
            ExprKind::ProviderDeclaration => ExprKind::ProviderDeclaration,
            ExprKind::Unsupported(rule) => ExprKind::Unsupported(rule),
        };
        let authored = Arc::new(AuthoredExpr {
            id: authored.id,
            definition_use: authored.definition_use,
            location: authored.location.clone(),
            kind,
            roles: authored.roles.clone(),
            secrecy_operator: authored.secrecy_operator.clone(),
            first_conversion: authored.first_conversion.clone(),
            parse_ownership: authored.parse_ownership,
            children: std::mem::take(&mut authored.children)
                .into_iter()
                .map(|child| child.map_bindings(map, memo))
                .collect(),
        });
        memo.insert(id, authored.clone());
        ExprValue {
            authored,
            // A parser projection is never binding-phase authority. The next
            // phase attaches its own projection without changing the root.
            payload: Projection::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SinkExpression {
    expression: ExprValue,
    payload: crate::eval_value::ParsedSinkValue,
}

impl SinkExpression {
    pub(in crate::parser) fn projection(&self) -> &crate::eval_value::ParsedSinkValue {
        &self.payload
    }
}

pub(in crate::parser) fn evaluate(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    captures: &CapturedNodes,
    reduce: impl FnOnce() -> Result<super::ParsedValue, super::ParseError>,
) -> Result<ExprValue, super::ParseError> {
    let pair_for_record = pair.clone();
    let mut expression = captures
        .get(&ParseNodeKey::of(&pair))
        .cloned()
        .unwrap_or_else(|| {
            compile(
                pair,
                file,
                source,
                &HashMap::new(),
                &mut CapturedNodes::default(),
            )
        });
    let value = reduce().map_err(|error| expression.authored.parse_failure(error))?;
    captures.record(&pair_for_record, value.clone());
    expression.payload = Projection::Value(value);
    Ok(expression)
}

pub(in crate::parser) fn evaluate_sink(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    captures: &CapturedNodes,
    reduce: impl FnOnce() -> Result<crate::eval_value::ParsedSinkValue, super::ParseError>,
) -> Result<SinkExpression, super::ParseError> {
    let pair_for_record = pair.clone();
    let mut expression = match captures.get(&ParseNodeKey::of(&pair)) {
        Some(expression) => expression.clone(),
        None if !captures.inventory => compile(
            pair,
            file,
            source,
            &HashMap::new(),
            &mut CapturedNodes::default(),
        ),
        None => return Err(MissingEvidence::new("live sink parser identity").into()),
    };
    let payload = reduce().map_err(|error| expression.authored.parse_failure(error))?;
    if let crate::eval_value::ParsedSinkValue::Ready(value) = &payload {
        captures.record(&pair_for_record, value.clone());
        expression.payload = Projection::Value(value.clone());
    }
    Ok(SinkExpression {
        expression,
        payload,
    })
}

pub(in crate::parser) fn resource_slots(
    pair: &Pair<'_, Rule>,
    captures: &CapturedNodes,
    sinks: &HashMap<String, SinkExpression>,
    values: &IndexMap<String, Value<ParserPlaceholder, RelativeBindingName>>,
    kind: crate::schema::SchemaKind,
) -> Result<AuthoredSlots, super::ParseError> {
    let Some(root) = captures.get(&ParseNodeKey::of(pair)) else {
        if captures.inventory {
            return Err(MissingEvidence::new("live resource parser identity").into());
        }
        // Standalone expression parser helpers have no configuration inventory.
        return Ok(AuthoredSlots::default());
    };
    // Keep the parser's child classification only until constructing the slots.
    // Captured names also preserve rejected keys and grouped block identities.
    // Repeated blocks share a kind by name, and a surviving scalar sink is always an attribute.
    let mut slot_kinds = HashMap::new();
    for child in pair
        .clone()
        .into_inner()
        .filter(|child| child.as_rule() == Rule::block_content)
        .flat_map(Pair::into_inner)
        .filter(|child| matches!(child.as_rule(), Rule::attribute | Rule::nested_block))
    {
        let source = captures
            .get(&ParseNodeKey::of(&child))
            .ok_or_else(|| MissingEvidence::new("classified resource child"))?;
        let name = match &source.authored.kind {
            ExprKind::Sink { name } | ExprKind::NestedBlock { name, .. } => name,
            _ => return Err(MissingEvidence::new("resource child sink identity").into()),
        };
        slot_kinds.insert(name.clone(), resource_child_kind(&child));
    }
    let instance = ExpansionInstanceId::fresh();
    let mut slots = AuthoredSlots::default();
    slots.set_instance(instance);
    slots.set_declaration(
        captures
            .resource_declarations
            .get(&root.node_id())
            .ok_or_else(|| MissingEvidence::new("resource storage declaration"))?
            .clone(),
    );
    for child in &root.authored.children {
        let ExprKind::Sink { name, .. } = &child.authored.kind else {
            continue;
        };
        if let Some(sink) = name.lookup_hash(sinks) {
            let conditions = captures.condition_snapshot();
            slots.insert(
                name.clone(),
                sink.expression.clone(),
                kind,
                AuthoredSlotKind::Attribute,
                instance,
                conditions,
            );
        } else {
            // Grouped blocks have explicit compiler-owned container handles,
            // including when a pending child prevents a provisional Value.
            let mut expression = child
                .authored
                .sink_value()
                .ok_or_else(|| MissingEvidence::new("named sink value"))?
                .clone();
            if let Some(value) = values.get(name) {
                expression.payload = Projection::Value(value.clone());
            }
            slots.insert(
                name.clone(),
                expression,
                kind,
                *slot_kinds
                    .get(name)
                    .ok_or_else(|| MissingEvidence::new("resource slot kind"))?,
                instance,
                captures.condition_snapshot(),
            );
        }
    }
    Ok(slots)
}

/// Parser-owned expression inventory. Merging and binding expansion preserve
/// this value wholesale; no provisional evaluator receives its authored roots.
///
/// Configuration validation requires a loader-owned judgement.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::{ParsedFile, ProviderContext}, schema::SchemaRegistry,
///     module_resolver::ResolvedModuleSignatures, validation::ValidationTypeContext};
/// let raw = ParsedFile::default();
/// let config = ProviderContext::new(carina_core::EnvironmentSnapshot::new([]));
/// let schemas = SchemaRegistry::new();
/// let signatures = ResolvedModuleSignatures::default();
/// let unchecked = ValidationTypeContext::new_providerless(&raw, &config, &signatures);
/// ```
#[derive(Clone)]
pub struct Expressions<B: BindingAddress = RelativeBindingName> {
    roots: Vec<ExprValue<ParserPlaceholder, B>>,
    definitions: IndexMap<B, ExprValue<ParserPlaceholder, B>>,
    binding_names: IndexMap<B, RelativeBindingName>,
    report: Arc<ExpressionReport<B>>,
    arguments: IndexMap<B, TypeExpr>,
    secrecy_arguments: IndexMap<B, Value<ParserPlaceholder, B>>,
    secrecy_argument_modules: IndexMap<B, B>,
    secrecy_argument_roots: IndexMap<ExpressionLocation, String>,
    secrecy_calls: IndexMap<ExpressionInstance, crate::secrecy::CallSite>,
    instance: ExpressionInstance,
    captures: Arc<CapturedNodes>,
    projections: Arc<IndexMap<(ExpressionInstance, NodeId), Value<ParserPlaceholder, B>>>,
    template_placements: crate::ConversionChannel,
}

impl<B: BindingAddress> PartialEq for Expressions<B> {
    fn eq(&self, other: &Self) -> bool {
        // Captures, projection caches and template admission metadata are
        // transient authority rather than expression value semantics.
        let Self {
            roots,
            definitions,
            binding_names,
            report,
            arguments,
            secrecy_arguments,
            secrecy_argument_modules,
            secrecy_argument_roots,
            secrecy_calls,
            instance,
            captures: _,
            projections: _,
            template_placements: _,
        } = self;
        roots == &other.roots
            && definitions == &other.definitions
            && binding_names == &other.binding_names
            && report == &other.report
            && arguments == &other.arguments
            && secrecy_arguments == &other.secrecy_arguments
            && secrecy_argument_modules == &other.secrecy_argument_modules
            && secrecy_argument_roots == &other.secrecy_argument_roots
            && secrecy_calls == &other.secrecy_calls
            && instance == &other.instance
    }
}

impl<B: BindingAddress> std::fmt::Debug for Expressions<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Expressions")
            .field("roots", &self.roots)
            .field("definitions", &self.definitions)
            .field("report", &self.report)
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl<B: BindingAddress> Default for Expressions<B> {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            definitions: IndexMap::new(),
            binding_names: IndexMap::new(),
            report: Arc::new(ExpressionReport::empty()),
            arguments: IndexMap::new(),
            secrecy_arguments: IndexMap::new(),
            secrecy_argument_modules: IndexMap::new(),
            secrecy_argument_roots: IndexMap::new(),
            secrecy_calls: IndexMap::new(),
            instance: ExpressionInstance::Root,
            captures: Arc::default(),
            projections: Arc::default(),
            template_placements: Default::default(),
        }
    }
}

/// Resolve a let-bound builtin closure from authored definitions. Captures
/// stay in application order, and no evaluated value can supply this identity.
fn builtin_closure<B: BindingAddress>(
    definitions: &IndexMap<B, ExprValue<ParserPlaceholder, B>>,
    instance: &ExpressionInstance,
    name: &str,
) -> Option<(
    crate::builtins::Builtin,
    Vec<ExprValue<ParserPlaceholder, B>>,
)> {
    let named = |instance: &ExpressionInstance, name: &str| {
        let scope = match instance {
            ExpressionInstance::Root => None,
            ExpressionInstance::Module(environment) => Some(environment.scope.as_ref()),
        };
        let binding: B = crate::resource::binding_address::binding_in_scope(
            RelativeBindingName::new(name),
            scope,
        );
        definitions
            .get(&binding)
            .filter(|definition| definition.secrecy_instance() == instance)
    };
    let mut definition = named(instance, name)?;
    let mut seen = HashSet::new();
    let mut captures = Vec::new();
    loop {
        if !seen.insert((definition.secrecy_instance().clone(), definition.node_id())) {
            return None;
        }
        match definition.secrecy_kind() {
            ExprKind::Alias | ExprKind::Parens | ExprKind::Definition => {
                definition = definition.secrecy_children().last()?;
            }
            ExprKind::Binding {
                binding,
                authored_binding: _,
            } => definition = definitions.get(binding)?,
            ExprKind::Call(name) => {
                captures.push(definition.secrecy_children());
                if let Some(builtin) = crate::builtins::Builtin::from_name(name) {
                    let captures = captures
                        .into_iter()
                        .rev()
                        .flatten()
                        .cloned()
                        .collect::<Vec<_>>();
                    return (!captures.is_empty() && captures.len() < builtin.arity())
                        .then_some((builtin, captures));
                }
                definition = named(definition.secrecy_instance(), name)?;
            }
            _ => return None,
        }
    }
}

impl<B: BindingAddress> Expressions<B> {
    pub(crate) fn admission_report(&self) -> &ExpressionReport<B> {
        &self.report
    }
    pub fn binding(&self, name: &B) -> Option<&ExprValue<ParserPlaceholder, B>> {
        self.definitions.get(name)
    }
    pub(in crate::parser) fn captured_nodes(&self) -> Arc<CapturedNodes> {
        self.captures.clone()
    }

    pub fn roots(&self) -> &[ExprValue<ParserPlaceholder, B>] {
        &self.roots
    }

    pub(crate) fn authored_binding_name(&self, binding: &B) -> Option<&RelativeBindingName> {
        self.binding_names.get(binding)
    }

    pub(crate) fn dependency_uses(&self, binding: &B) -> Vec<AuthoredDependency<B>> {
        let mut uses = Vec::new();
        if let Some(definition) = self.definitions.get(binding) {
            definition
                .authored
                .dependency_uses(DiagnosticScope::Public, &mut uses);
        }
        uses
    }

    /// Read the authored loop at its use; resolved values must not decide
    /// whether an iterable came from a formal argument.
    pub(crate) fn argument_iterable_at(
        &self,
        declaration: &DeclarationLocation,
        scope: &crate::resource::ModuleInstanceScope,
    ) -> bool {
        let instance = ExpressionInstance::for_scope(scope);
        let target = declaration.span();
        let mut pending = self
            .roots
            .iter()
            .map(|root| root.authored.as_ref())
            .collect::<Vec<_>>();
        let mut innermost = None;
        while let Some(node) = pending.pop() {
            let location = &node.location.authored;
            let span = location.span();
            if node.location.instance != instance
                || location.file() != declaration.file()
                || (span.start_line(), span.start_column())
                    > (target.start_line(), target.start_column())
                || (span.end_line(), span.end_column()) < (target.end_line(), target.end_column())
            {
                continue;
            }
            if matches!(node.kind, ExprKind::For { .. }) {
                // The kind's iterable is redacted text, not a syntax node.
                // Main's constructor and admission walk use [iterable, body].
                innermost = node.children.first().map(|child| child.authored.as_ref());
            }
            pending.extend(node.children.iter().map(|child| child.authored.as_ref()));
        }
        let Some(iterable) = innermost else {
            return false;
        };
        let mut uses = Vec::new();
        iterable.dependency_uses(DiagnosticScope::Public, &mut uses);
        let mut seen = Vec::new();
        while let Some(usage) = uses.pop() {
            let binding = match usage {
                AuthoredDependency::Binding {
                    binding,
                    authored_binding: _,
                    scope: _,
                } => binding,
                AuthoredDependency::Reference {
                    identity,
                    authored: _,
                    scope: _,
                } => identity.binding_address().clone(),
            };
            if self.is_module_argument(&binding) {
                return true;
            }
            if !seen.contains(&binding) {
                uses.extend(self.dependency_uses(&binding));
                seen.push(binding);
            }
        }
        false
    }

    pub(crate) fn is_module_argument(&self, binding: &B) -> bool {
        self.arguments.contains_key(binding)
    }

    /// Provisional alias payload for diagnostic provenance, never admission evidence.
    pub(crate) fn binding_projection(&self, binding: &B) -> Option<&Value<ParserPlaceholder, B>> {
        self.definitions
            .get(binding)
            .and_then(|definition| match &definition.payload {
                Projection::Value(value) => Some(value),
                Projection::Pending => None,
            })
            .or_else(|| self.secrecy_arguments.get(binding))
    }

    pub(super) fn extend(&mut self, other: Self) {
        self.roots.extend(other.roots);
        self.definitions.extend(other.definitions);
        self.binding_names.extend(other.binding_names);
        self.arguments.extend(other.arguments);
        self.secrecy_arguments.extend(other.secrecy_arguments);
        self.secrecy_argument_modules
            .extend(other.secrecy_argument_modules);
        self.secrecy_argument_roots
            .extend(other.secrecy_argument_roots);
        self.secrecy_calls.extend(other.secrecy_calls);
        self.template_placements.append(other.template_placements);
        Arc::make_mut(&mut self.projections).extend(Arc::unwrap_or_clone(other.projections));
        Arc::make_mut(&mut self.report).extend(Arc::unwrap_or_clone(other.report));
    }

    pub(super) fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
    ) -> Expressions<C> {
        let mut memo = HashMap::new();
        Expressions {
            secrecy_arguments: self
                .secrecy_arguments
                .into_iter()
                .map(|(name, value)| (map(name), value.map_bindings(map)))
                .collect(),
            secrecy_argument_modules: self
                .secrecy_argument_modules
                .into_iter()
                .map(|(name, module)| (map(name), map(module)))
                .collect(),
            secrecy_argument_roots: self.secrecy_argument_roots,
            secrecy_calls: self.secrecy_calls,
            template_placements: self.template_placements,
            captures: Arc::default(),
            projections: Arc::new(
                self.projections
                    .iter()
                    .map(|(id, value)| (id.clone(), value.clone().map_bindings(map)))
                    .collect(),
            ),
            instance: self.instance,
            arguments: self
                .arguments
                .into_iter()
                .map(|(name, expression)| (map(name), expression))
                .collect(),
            report: Arc::new(self.report.map_bindings(map)),
            roots: self
                .roots
                .into_iter()
                .map(|root| root.map_bindings(map, &mut memo))
                .collect(),
            binding_names: self
                .binding_names
                .into_iter()
                .map(|(binding, name)| (map(binding), name))
                .collect(),
            definitions: self
                .definitions
                .into_iter()
                .map(|(name, expression)| (map(name), expression.map_bindings(map, &mut memo)))
                .collect(),
        }
    }

    pub(super) fn attach_binding_projections(
        &mut self,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
    ) {
        for (binding, expression) in &mut self.definitions {
            if let Some(value) = values.get(binding.as_str()) {
                expression.payload = Projection::Value(value.clone());
            }
        }
    }

    pub(crate) fn substitute_projections(
        &mut self,
        arguments: crate::module_resolver::expander::ArgumentSubstitutions<'_, B>,
    ) {
        let projections = self
            .projections
            .iter()
            .filter_map(|(id, value)| {
                // These are archived operands, including structural declarations.
                // Unprojectable handles invalidate their old payload instead of
                // adding a new rejection to the live value path.
                crate::value_walk::substitute_module_arguments(
                    value,
                    arguments,
                    crate::value_walk::RewritePosition::Value,
                )
                .ok()
                .map(|value| (id.clone(), value))
            })
            .collect();
        self.projections = Arc::new(projections);
        self.report = Arc::new(ExpressionReport::empty());
    }

    pub(crate) fn retain_secrecy_call(&mut self, name: String, location: DeclarationLocation) {
        self.secrecy_calls.insert(
            self.instance.clone(),
            crate::secrecy::CallSite { name, location },
        );
    }

    pub(crate) fn bind_arguments(
        &mut self,
        scope: &crate::resource::ModuleInstanceScope,
        arguments: impl IntoIterator<Item = (B, TypeExpr)>,
    ) {
        self.instance = ExpressionInstance::for_scope(scope);
        self.projections = Arc::new(
            self.projections
                .iter()
                .map(|((_, id), value)| ((self.instance.clone(), *id), value.clone()))
                .collect(),
        );
        fn qualify<P: ValueKind, B: BindingAddress>(
            expression: &mut ExprValue<P, B>,
            instance: &ExpressionInstance,
            memo: &mut HashMap<usize, Arc<AuthoredExpr<P, B>>>,
        ) {
            let id = Arc::as_ptr(&expression.authored) as usize;
            if let Some(authored) = memo.get(&id) {
                expression.authored = authored.clone();
                return;
            }
            let mut authored = (*expression.authored).clone();
            authored.location.instance = instance.clone();
            for child in &mut authored.children {
                qualify(child, instance, memo);
            }
            let authored = Arc::new(authored);
            memo.insert(id, authored.clone());
            expression.authored = authored;
        }
        let mut memo = HashMap::new();
        for root in &mut self.roots {
            qualify(root, &self.instance, &mut memo);
        }
        for root in self.definitions.values_mut() {
            qualify(root, &self.instance, &mut memo);
        }
        self.arguments.extend(arguments);
        self.report = Arc::new(ExpressionReport::empty());
    }
}

impl Expressions<crate::resource::AbsoluteBindingAddress> {
    pub(crate) fn retain_secrecy_arguments<'a>(
        &mut self,
        scope: &crate::resource::ModuleInstanceScope,
        call: &super::ModuleCall,
        caller_scope: Option<&crate::resource::ModuleInstanceScope>,
        parameters: impl IntoIterator<
            Item = (
                &'a str,
                Option<&'a Value<ParserPlaceholder, RelativeBindingName>>,
            ),
        >,
    ) {
        for (name, default) in parameters {
            if let Some(location) = call.id().argument_value_location(name) {
                let instance = caller_scope
                    .map(ExpressionInstance::for_scope)
                    .unwrap_or(ExpressionInstance::Root);
                self.secrecy_argument_roots.insert(
                    ExpressionLocation::new(instance, location),
                    name.to_string(),
                );
            }
            let explicit = call.argument(name).map(super::ModuleArgument::walker_value);
            let Some(value) = explicit.or(default) else {
                continue;
            };
            let reference_scope = if explicit.is_some() {
                caller_scope
            } else {
                Some(scope)
            };
            if let Some(reference_scope) = reference_scope {
                self.secrecy_argument_modules.insert(
                    scope.resolve(RelativeBindingName::new(name)),
                    crate::resource::RootScope::new()
                        .resolve(RelativeBindingName::new(reference_scope.key())),
                );
            }
            let resolve = |binding: RelativeBindingName| {
                reference_scope.map_or_else(
                    || crate::resource::RootScope::new().resolve(binding.clone()),
                    |scope| scope.resolve(binding.clone()),
                )
            };
            // Preserve the call's authored value before the resolver consults
            // observations. A provider return cannot turn an authored Secret
            // reference into a Public literal in this context.
            let value = value.clone().map_bindings(&mut |binding| resolve(binding));
            let value = value
                .try_map_placeholder::<ParserPlaceholder, std::convert::Infallible>(
                    &mut |placeholder| {
                        Ok(match placeholder {
                            ParserPlaceholder::ModuleArgument {
                                name,
                                diagnostic: _,
                            } => Value::Deferred(crate::resource::DeferredValue::BindingRef {
                                binding: resolve(RelativeBindingName::new(name)),
                            }),
                            ParserPlaceholder::FnParam { name } => {
                                Value::Deferred(crate::resource::DeferredValue::Placeholder(
                                    ParserPlaceholder::FnParam { name },
                                ))
                            }
                            ParserPlaceholder::FnLocal { name } => {
                                Value::Deferred(crate::resource::DeferredValue::Placeholder(
                                    ParserPlaceholder::FnLocal { name },
                                ))
                            }
                            ParserPlaceholder::Loop(slot) => {
                                Value::Deferred(crate::resource::DeferredValue::Placeholder(
                                    ParserPlaceholder::Loop(slot),
                                ))
                            }
                            ParserPlaceholder::EmptyInterpolation { source } => {
                                Value::Deferred(crate::resource::DeferredValue::Placeholder(
                                    ParserPlaceholder::EmptyInterpolation { source },
                                ))
                            }
                        })
                    },
                )
                .expect("authored argument placeholder mapping is infallible");
            self.secrecy_arguments
                .insert(scope.resolve(RelativeBindingName::new(name)), value);
        }
    }
}

/// A schema-judged configuration owns its file and report and borrows the
/// caller's schema registry. Parsing alone cannot construct this type.
/// Consumers borrow this bundle so the file and schemas stay paired.
#[derive(Debug, Clone)]
pub struct JudgedFile<'a, E, D, B: BindingAddress = RelativeBindingName> {
    file: super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &'a crate::schema::SchemaRegistry,
    secrecy: OnceLock<crate::secrecy::SecrecyReport>,
}

impl<'a, E, D, B: BindingAddress> JudgedFile<'a, E, D, B> {
    pub fn file(&self) -> &super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
        &self.file
    }
    pub fn report(&self) -> &ExpressionReport<B> {
        self.file.expression_report()
    }
    pub fn schemas(&self) -> &'a crate::schema::SchemaRegistry {
        self.schemas
    }
    pub(crate) fn into_file(self) -> super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
        self.file
    }
}

impl<E, D, B: BindingAddress> JudgedFile<'_, E, D, B>
where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    /// Derive once for this immutable graph and schema registry. Type-only
    /// consumers never need to walk structural secrecy. Consuming this owner
    /// and editing its file requires a new judgement before admission.
    pub fn secrecy(&self) -> &crate::secrecy::SecrecyReport {
        self.secrecy
            .get_or_init(|| crate::secrecy::derive(&self.file, self.report(), self.schemas))
    }
}

impl<'a, E: super::ExportParamLike<ParserPlaceholder, B>, D, B: BindingAddress>
    JudgedFile<'a, E, D, B>
where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub(crate) fn new(
        mut file: super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        schemas: &'a crate::schema::SchemaRegistry,
        config: &super::ProviderContext,
    ) -> Self {
        judge_configuration(&mut file, schemas, config);
        Self {
            file,
            schemas,
            secrecy: OnceLock::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("configuration has not crossed the expression judgement boundary")]
pub struct UnjudgedConfiguration;

impl<E, D, B: BindingAddress> super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
    pub(crate) fn expression_report(&self) -> &ExpressionReport<B> {
        &self.parser_binding_metadata.syntax().expressions().report
    }
}

pub(crate) fn judge_configuration<
    E: super::ExportParamLike<ParserPlaceholder, B>,
    D,
    B: BindingAddress,
>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &crate::schema::SchemaRegistry,
    config: &super::ProviderContext,
) where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    prepare_slots(file, schemas);
    let mut report = judge_expressions(file, schemas, config);
    for parameter in &file.export_params {
        if let (Some(sink), Some(location)) =
            (parameter.type_expr_opt(), parameter.value_location())
        {
            report.admit_sink(location, sink);
        }
    }
    for parameter in &file.attribute_params {
        if let (Some(sink), Some(location)) = (&parameter.static_type, parameter.value_location()) {
            report.admit_sink(location, sink);
        }
    }
    for parameter in &mut file.export_params {
        parameter.attach_judgement(&report);
    }
    for parameter in &mut file.attribute_params {
        parameter.attach_judgement(&report);
    }
    let report = Arc::new(report);
    attach_slot_reports(file, &report);
    file.parser_binding_metadata.syntax.expressions_mut().report = report;
}

pub(crate) fn attach_report<E, D, B: BindingAddress>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    report: ExpressionReport<B>,
) where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let report = Arc::new(report);
    attach_slot_reports(file, &report);
    file.parser_binding_metadata.syntax.expressions_mut().report = report;
}

/// Preserve the resolver template's already-produced placement findings.
pub(crate) fn retain_module_placement_findings<E, D, B: BindingAddress, C: BindingAddress>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    template: &ExpressionReport<C>,
    subject: crate::validation::FindingSubject,
) {
    let expressions = file.parser_binding_metadata.syntax.expressions_mut();
    let findings = template.placements.as_ref().clone().in_subject(subject);
    expressions.template_placements.append(findings.clone());
    Arc::make_mut(&mut Arc::make_mut(&mut expressions.report).placements).append(findings);
}

fn prepare_slots<E, D, B: BindingAddress>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &crate::schema::SchemaRegistry,
) where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    for resource in file.iter_all_resources_mut() {
        match resource {
            super::ResourceMut::Resource(resource) => {
                if let Some(schema) = schemas.get_for(resource) {
                    resource
                        .authored_slots
                        .bind_schema(schema.schema().as_schema());
                }
                resource.authored_slots.synchronize(&resource.attributes);
            }
            super::ResourceMut::DataSource(source) => {
                if let Some(schema) = schemas.get_for_data_source(source) {
                    source
                        .authored_slots
                        .bind_schema(schema.schema().as_schema());
                }
                source.authored_slots.synchronize(&source.attributes);
            }
            super::ResourceMut::Deferred { .. } | super::ResourceMut::LiveComposition(_) => {}
        }
    }
    for (id, slots) in file.deferred_for_phase.authored_slots_mut() {
        if let Some(kind) = slots.selected_schema_kind()
            && let Some(schema) = schemas.get(&id.provider, &id.resource_type, kind)
        {
            slots.bind_schema(schema.schema().as_schema());
        }
    }
}

fn attach_slot_reports<E, D, B: BindingAddress>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    report: &Arc<ExpressionReport<B>>,
) where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    for (id, slots) in file.deferred_for_phase.authored_slots_mut() {
        slots.attach_snapshot(report.shared_for_resource(id));
    }
    for resource in file.iter_all_resources_mut() {
        match resource {
            super::ResourceMut::Resource(resource) => {
                resource
                    .authored_slots
                    .attach_snapshot(report.shared_for_resource(&resource.id));
            }
            super::ResourceMut::DataSource(resource) => {
                resource
                    .authored_slots
                    .attach_snapshot(report.shared_for_resource(&resource.id));
            }
            super::ResourceMut::Deferred { .. } => {}
            super::ResourceMut::LiveComposition(_) => {}
        }
    }
}

impl Expressions {
    pub(in crate::parser) fn finish_captures(&mut self) {
        self.projections = Arc::new(
            self.captures
                .values
                .read()
                .expect("retained projections lock")
                .iter()
                .map(|(id, value)| {
                    #[cfg(test)]
                    record_projection_copy(value);
                    ((self.instance.clone(), *id), value.clone())
                })
                .collect(),
        );
    }

    pub(in crate::parser) fn from_pairs(
        pairs: Pairs<'_, Rule>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        let mut out = Self::default();
        let mut captures = CapturedNodes {
            inventory: true,
            ..Default::default()
        };
        let mut authored = pairs.clone().collect::<Vec<_>>();
        while let Some(pair) = authored.pop() {
            if matches!(
                pair.as_rule(),
                Rule::let_binding
                    | Rule::local_binding
                    | Rule::fn_local_let
                    | Rule::for_local_binding
                    | Rule::if_local_binding
            ) && let Some(rhs) = pair
                .clone()
                .into_inner()
                .find(|child| child.as_rule() == Rule::expression)
                && let Some(descent) = super::util::descend_expression(rhs, false)
                && let Some(expression) = descent.primary().into_inner().next()
                && expression.as_rule() == Rule::for_expr
            {
                captures
                    .bound_for_sites
                    .insert(ParseNodeKey::of(&expression));
            }
            authored.extend(pair.into_inner());
        }
        for statement in pairs
            .flat_map(|pair| pair.into_inner())
            .filter(|pair| pair.as_rule() == Rule::statement)
        {
            let pair = statement.into_inner().next().expect("statement body");
            if pair.as_rule() == Rule::let_binding {
                let mut parts = pair.clone().into_inner();
                let name = parts.next().expect("let binding name").as_str();
                if let Some(rhs) = parts.find(|part| part.as_rule() == Rule::expression) {
                    let expression = compile(rhs, file, source, &HashMap::new(), &mut captures);
                    if name != "_" {
                        out.definitions
                            .insert(RelativeBindingName::new(name), expression.clone());
                        out.binding_names.insert(
                            RelativeBindingName::new(name),
                            RelativeBindingName::new(name),
                        );
                    }
                    out.roots.push(if name == "_" {
                        node(&pair, ExprKind::Definition, vec![expression], file, source)
                    } else {
                        expression
                    });
                    continue;
                }
            }
            out.roots
                .push(compile(pair, file, source, &HashMap::new(), &mut captures));
        }
        out.captures = Arc::new(captures);
        out
    }
}

fn node(
    pair: &Pair<'_, Rule>,
    kind: ExprKind<RelativeBindingName>,
    children: Vec<ExprValue>,
    file: &PreprocessedFile,
    source: &Path,
) -> ExprValue {
    node_at(
        DeclarationLocation::expression_node(pair, file, source),
        kind,
        children,
    )
}

fn sink_node(
    pair: &Pair<'_, Rule>,
    name: super::AttributeKey,
    value: ExprValue,
    key: Option<ExprValue>,
    file: &PreprocessedFile,
    source: &Path,
) -> ExprValue {
    let roles = AuthoredChildRoles::Sink {
        value: value.node_id(),
        key: key.as_ref().map(ExprValue::node_id),
        placement: SinkPlacement::Value,
    };
    let kind = ExprKind::Sink { name };
    let mut children = vec![value];
    children.extend(key);
    node_with_roles(pair, kind, children, roles, file, source)
}

fn node_with_roles(
    pair: &Pair<'_, Rule>,
    kind: ExprKind<RelativeBindingName>,
    children: Vec<ExprValue>,
    roles: AuthoredChildRoles,
    file: &PreprocessedFile,
    source: &Path,
) -> ExprValue {
    node_at_with_roles(
        DeclarationLocation::expression_node(pair, file, source),
        kind,
        children,
        roles,
    )
}

fn node_at(
    location: DeclarationLocation,
    kind: ExprKind<RelativeBindingName>,
    children: Vec<ExprValue>,
) -> ExprValue {
    node_at_with_roles(location, kind, children, AuthoredChildRoles::Positional)
}

fn node_at_with_roles(
    location: DeclarationLocation,
    kind: ExprKind<RelativeBindingName>,
    children: Vec<ExprValue>,
    roles: AuthoredChildRoles,
) -> ExprValue {
    let roles =
        if matches!(kind, ExprKind::Sequence) && matches!(roles, AuthoredChildRoles::Positional) {
            AuthoredChildRoles::Sequence {
                result: children.last().map(ExprValue::node_id),
            }
        } else {
            roles
        };
    let id = NodeId::fresh();
    let first_conversion = if let ExprKind::Convert { parsing, .. } = &kind {
        Some(Arc::new(ConversionSite {
            node: id,
            location: location.clone(),
            parsing: parsing.clone(),
        }))
    } else {
        children
            .iter()
            .find_map(|child| child.authored.first_conversion.clone())
    };
    let parse_ownership = match &kind {
        ExprKind::Convert { .. } => ParseOwnership::Conversion,
        ExprKind::Interpolation { .. }
        | ExprKind::Call(_)
        | ExprKind::Integer { .. }
        | ExprKind::Access(_)
        | ExprKind::DynamicAccess
        | ExprKind::Secret => ParseOwnership::Covered,
        ExprKind::Sink { .. } => match &roles {
            AuthoredChildRoles::Sink { value, .. } => children
                .iter()
                .find(|child| child.node_id() == *value)
                .map_or(ParseOwnership::Ordinary, |child| {
                    child.authored.parse_ownership
                }),
            AuthoredChildRoles::Function { .. }
            | AuthoredChildRoles::Sequence { .. }
            | AuthoredChildRoles::Computed
            | AuthoredChildRoles::Positional
            | AuthoredChildRoles::If { .. } => ParseOwnership::Ordinary,
        },
        ExprKind::Alias | ExprKind::Parens | ExprKind::Definition => {
            children.first().map_or(ParseOwnership::Ordinary, |child| {
                child.authored.parse_ownership
            })
        }
        ExprKind::Sequence => match roles {
            AuthoredChildRoles::Sequence { result } => {
                result.and_then(|result| children.iter().find(|child| child.node_id() == result))
            }
            AuthoredChildRoles::Positional
            | AuthoredChildRoles::If { .. }
            | AuthoredChildRoles::Computed
            | AuthoredChildRoles::Function { .. }
            | AuthoredChildRoles::Sink { .. } => None,
        }
        .map_or(ParseOwnership::Ordinary, |child| {
            child.authored.parse_ownership
        }),
        ExprKind::Scalar(_)
        | ExprKind::Parameter(_)
        | ExprKind::LoopSlot(_)
        | ExprKind::UntypedParameter
        | ExprKind::Binding { .. }
        | ExprKind::Reference { .. }
        | ExprKind::List
        | ExprKind::Map(_)
        | ExprKind::NestedBlock { .. }
        | ExprKind::EmptyInterpolation
        | ExprKind::Coalesce
        | ExprKind::Function { .. }
        | ExprKind::Element
        | ExprKind::Iterable
        | ExprKind::Resource { .. }
        | ExprKind::If
        | ExprKind::For { .. }
        | ExprKind::Wait
        | ExprKind::Compose
        | ExprKind::ModuleCall
        | ExprKind::Boolean
        | ExprKind::Declaration
        | ExprKind::RequireCondition
        | ExprKind::ProviderDeclaration
        | ExprKind::Unsupported(_) => ParseOwnership::Ordinary,
    };
    ExprValue {
        authored: Arc::new(AuthoredExpr {
            id,
            definition_use: UseId::fresh(),
            location: ExpressionLocation::new(ExpressionInstance::Root, &location),
            kind,
            roles,
            secrecy_operator: None,
            first_conversion,
            parse_ownership,
            children,
        }),
        payload: Projection::Pending,
    }
}

fn lower_blocks(children: Vec<ExprValue>) -> Vec<ExprValue> {
    let mut groups: IndexMap<super::AttributeKey, Vec<ExprValue>> = IndexMap::new();
    let mut headers = HashMap::new();
    for child in &children {
        if let ExprKind::NestedBlock { name, header } = &child.authored.kind {
            groups
                .entry(name.clone())
                .or_default()
                .push(child.authored.children[0].clone());
            headers
                .entry(name.clone())
                .or_insert_with(|| header.clone());
        }
    }
    let groups: HashMap<_, _> = groups
        .into_iter()
        .map(|(name, records)| {
            let value = ExprValue {
                authored: Arc::new(AuthoredExpr {
                    id: NodeId::fresh(),
                    definition_use: UseId::fresh(),
                    location: ExpressionLocation::new(ExpressionInstance::Root, &headers[&name]),
                    kind: ExprKind::List,
                    roles: AuthoredChildRoles::Positional,
                    secrecy_operator: None,
                    first_conversion: records
                        .iter()
                        .find_map(|child| child.authored.first_conversion.clone()),
                    parse_ownership: ParseOwnership::Ordinary,
                    children: records,
                }),
                payload: Projection::Pending,
            };
            (name, value)
        })
        .collect();
    children
        .into_iter()
        .map(|child| {
            if let ExprKind::NestedBlock { name, header } = &child.authored.kind {
                ExprValue {
                    authored: Arc::new(AuthoredExpr {
                        id: NodeId::fresh(),
                        definition_use: UseId::fresh(),
                        location: ExpressionLocation::new(ExpressionInstance::Root, header),
                        kind: ExprKind::Sink { name: name.clone() },
                        roles: AuthoredChildRoles::Sink {
                            value: groups[name].node_id(),
                            key: None,
                            placement: SinkPlacement::Value,
                        },
                        secrecy_operator: None,
                        first_conversion: groups[name].authored.first_conversion.clone(),
                        parse_ownership: ParseOwnership::Ordinary,
                        children: vec![groups[name].clone()],
                    }),
                    payload: Projection::Pending,
                }
            } else {
                child
            }
        })
        .collect()
}

/// Exact parser-token identity while one parse is alive. Pest's Hash input
/// encodes its token queue, input allocation and token ordinal. Retaining those
/// bytes avoids hash collisions and never uses diagnostic locations as identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::parser) struct ParseNodeKey(Vec<u8>);
impl ParseNodeKey {
    fn of(pair: &Pair<'_, Rule>) -> Self {
        use std::hash::{Hash, Hasher};
        #[derive(Default)]
        struct IdentityBytes(Vec<u8>);
        impl Hasher for IdentityBytes {
            fn write(&mut self, bytes: &[u8]) {
                self.0.extend_from_slice(bytes);
            }
            fn finish(&self) -> u64 {
                unreachable!("identity bytes are never hashed to a digest")
            }
        }
        let mut identity = IdentityBytes::default();
        pair.hash(&mut identity);
        Self(identity.0)
    }
}

#[derive(Default)]
pub(in crate::parser) struct CapturedNodes {
    inventory: bool,
    nodes: HashMap<ParseNodeKey, ExprValue>,
    bound_for_sites: HashSet<ParseNodeKey>,
    resource_declarations: HashMap<NodeId, SlotDeclarationId>,
    values: std::sync::RwLock<IndexMap<NodeId, Value<ParserPlaceholder, RelativeBindingName>>>,
    conditions: ConditionRecorder,
}
impl std::fmt::Debug for CapturedNodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CapturedNodes(..)")
    }
}
impl CapturedNodes {
    fn condition_snapshot(&self) -> ConditionSnapshot {
        self.conditions.snapshot()
    }
    pub(in crate::parser) fn record_selected_condition(
        &self,
        pair: &Pair<'_, Rule>,
        selected: bool,
    ) {
        if let Some(id) = self.node_id(pair) {
            self.conditions.extend([(id, selected)]);
        }
        self.record(
            pair,
            Value::Concrete(crate::resource::ConcreteValue::Bool(selected)),
        );
    }
    fn get(&self, key: &ParseNodeKey) -> Option<&ExprValue> {
        self.nodes.get(key)
    }
    pub(in crate::parser) fn reject_boundary(&self, pair: &Pair<'_, Rule>) -> bool {
        self.get(&ParseNodeKey::of(pair))
            .and_then(|node| node.authored.first_conversion.as_ref())
            .is_some_and(|site| site.parsing.recoverable())
    }
    pub(in crate::parser) fn recover_evaluation(
        &self,
        pair: &Pair<'_, Rule>,
        result: Result<crate::eval_value::EvalValue, super::ParseError>,
    ) -> Result<crate::eval_value::EvalValue, super::ParseError> {
        let error = match result {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(expression) = self.get(&ParseNodeKey::of(pair)) else {
            return Err(error);
        };
        let Some(site) = expression.authored.first_conversion.as_ref() else {
            return Err(error);
        };
        match expression.authored.parse_ownership {
            ParseOwnership::Conversion => {
                site.parsing.operand_failed();
                Err(error)
            }
            ParseOwnership::Ordinary => Err(error),
            ParseOwnership::Covered => {
                let value = self
                    .values
                    .read()
                    .expect("retained projections lock")
                    .get(&site.node)
                    .cloned();
                match value {
                    Some(value) if site.parsing.recoverable() => {
                        Ok(crate::eval_value::EvalValue::from_value(value))
                    }
                    Some(_) | None => Err(error),
                }
            }
        }
    }
    pub(in crate::parser) fn resource_declaration(
        &self,
        pair: &Pair<'_, Rule>,
    ) -> Option<SlotDeclarationId> {
        self.resource_declarations
            .get(&self.node_id(pair)?)
            .cloned()
    }
    pub(in crate::parser) fn node_id(&self, pair: &Pair<'_, Rule>) -> Option<NodeId> {
        self.get(&ParseNodeKey::of(pair)).map(ExprValue::node_id)
    }
    fn insert(&mut self, key: ParseNodeKey, value: ExprValue) {
        if matches!(value.authored.kind, ExprKind::Resource { .. })
            && !self.resource_declarations.contains_key(&value.node_id())
        {
            let address = SlotDeclarationId::new(
                value.authored.location.authored.file(),
                self.resource_declarations.len(),
            );
            self.resource_declarations.insert(value.node_id(), address);
        }
        self.nodes.insert(key, value);
    }
    pub(in crate::parser) fn record(
        &self,
        pair: &Pair<'_, Rule>,
        value: Value<ParserPlaceholder, RelativeBindingName>,
    ) {
        let Some(mut expression) = self.get(&ParseNodeKey::of(pair)) else {
            return;
        };
        let mut values = self.values.write().expect("retained projections lock");
        loop {
            values.insert(expression.node_id(), value.clone());
            if matches!(
                expression.authored.kind,
                ExprKind::Alias | ExprKind::Parens | ExprKind::Convert { .. }
            ) && let [child] = expression.authored.children.as_slice()
            {
                expression = child;
            } else {
                break;
            }
        }
    }
}

fn compile(
    mut pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
    captures: &mut CapturedNodes,
) -> ExprValue {
    // Grammar forwarding is iterative. Deep authored alternatives must not
    // multiply the Rust stack by every precedence/alias grammar level.
    let mut wrappers = Vec::new();
    let mut expression = loop {
        let key = ParseNodeKey::of(&pair);
        if let Some(expression) = captures.get(&key) {
            break expression.clone();
        }
        if let Some(child) = forwarding_child(&pair) {
            wrappers.push(pair);
            pair = child;
        } else {
            let expression = compile_form(pair, file, source, scope, captures);
            captures.insert(key, expression.clone());
            break expression;
        }
    };
    for pair in wrappers.into_iter().rev() {
        let kind = if pair.as_rule() == Rule::require_condition {
            ExprKind::RequireCondition
        } else if pair.as_rule() == Rule::primary && pair.as_str().trim_start().starts_with('(') {
            ExprKind::Parens
        } else {
            ExprKind::Alias
        };
        expression = node(&pair, kind, vec![expression], file, source);
        captures.insert(ParseNodeKey::of(&pair), expression.clone());
    }
    expression
}

fn forwarding_child<'i>(pair: &Pair<'i, Rule>) -> Option<Pair<'i, Rule>> {
    let forwarding = matches!(
        pair.as_rule(),
        Rule::expression
            | Rule::primary
            | Rule::coalesce_expr
            | Rule::pipe_expr
            | Rule::additive_expr
            | Rule::multiplicative_expr
            | Rule::compose_expr
            | Rule::postfix_expr
            | Rule::subscripted_id
            | Rule::validate_expr
            | Rule::require_condition
            | Rule::validate_primary
            | Rule::validate_or_expr
            | Rule::validate_and_expr
            | Rule::validate_comparison
            | Rule::validate_additive_expr
            | Rule::validate_multiplicative_expr
    ) || (pair.as_rule() == Rule::validate_not_expr
        && !pair.as_str().trim_start().starts_with('!'));
    if !forwarding {
        return None;
    }
    let mut parts = pair.clone().into_inner();
    let child = parts.next()?;
    parts.next().is_none().then_some(child)
}

/// Authored diagnostic source is retained only when its parsed graph contains
/// no intrinsic secrets. A protected graph cannot contribute any raw source.
#[derive(Debug, Clone)]
pub(crate) struct DiagnosticSourceText(Option<super::AuthoredExpressionText>);

impl DiagnosticSourceText {
    pub(crate) fn into_text(self) -> Option<Arc<str>> {
        self.0.map(|text| Arc::from(text.as_str().trim_end()))
    }
}

#[cfg(test)]
impl From<&'static str> for DiagnosticSourceText {
    fn from(text: &'static str) -> Self {
        // Test metadata supplies literal display text, often just a partial
        // header. Production construction below always consumes parsed pairs.
        Self(Some(super::AuthoredExpressionText::literal_value(text)))
    }
}

pub(super) fn diagnostic_source_text(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source_file: &Path,
) -> DiagnosticSourceText {
    fn contains_secret(expression: &ExprValue) -> bool {
        matches!(expression.authored.kind, ExprKind::Secret)
            || expression.authored.children.iter().any(contains_secret)
    }
    let expression = compile(
        pair.clone(),
        file,
        source_file,
        &HashMap::new(),
        &mut CapturedNodes::default(),
    );
    if contains_secret(&expression) {
        return DiagnosticSourceText(None);
    }
    DiagnosticSourceText(Some(super::AuthoredExpressionText::redact(
        pair,
        Some(file),
    )))
}

fn reference_form(pair: Pair<'_, Rule>) -> Option<Pair<'_, Rule>> {
    match pair.as_rule() {
        Rule::variable_ref | Rule::namespaced_id => Some(pair),
        Rule::expression
        | Rule::coalesce_expr
        | Rule::pipe_expr
        | Rule::additive_expr
        | Rule::multiplicative_expr
        | Rule::compose_expr
        | Rule::postfix_expr
        | Rule::primary => {
            let mut children = pair.into_inner();
            let child = children.next()?;
            children
                .next()
                .is_none()
                .then(|| reference_form(child))
                .flatten()
        }
        _ => None,
    }
}

pub(in crate::parser) fn record_interpolation_binding(
    pair: Pair<'_, Rule>,
    ctx: &super::ParseContext<'_>,
    mode: &super::context::ExpressionMode<'_>,
) {
    let mut parts = pair
        .clone()
        .into_inner()
        .flat_map(|quoted| quoted.into_inner());
    let Some(part) = parts.next() else { return };
    if parts.next().is_some() {
        return;
    }
    let Some(interpolation) = part.into_inner().next() else {
        return;
    };
    if interpolation.as_rule() != Rule::interpolation {
        return;
    }
    let Some(expression) = interpolation
        .into_inner()
        .next()
        .and_then(|wrapper| wrapper.into_inner().next())
    else {
        return;
    };
    let Some(reference) = reference_form(expression) else {
        return;
    };
    if reference.as_rule() != Rule::variable_ref {
        return;
    }
    let mut parts = reference.into_inner();
    let Some(name) = parts.next() else { return };
    if parts.next().is_none() {
        // Keep provenance only. The interpolation's judgement decides whether
        // this use is an assignment or an operand rendered to text.
        let _ =
            mode.record_bare_binding_at(ctx, name.as_str(), &name, ctx.declaration_location(&pair));
    }
}

fn compile_children(
    pair: &Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
    captures: &mut CapturedNodes,
) -> Vec<ExprValue> {
    let mut scope = scope.clone();
    let mut children = Vec::new();
    for child in pair.clone().into_inner().flat_map(|child| {
        if child.as_rule() == Rule::block_content {
            child.into_inner().collect::<Vec<_>>()
        } else {
            vec![child]
        }
    }) {
        if matches!(
            child.as_rule(),
            Rule::local_binding
                | Rule::fn_local_let
                | Rule::for_local_binding
                | Rule::if_local_binding
        ) {
            let mut parts = child.clone().into_inner();
            let name = parts
                .next()
                .expect("local binding name")
                .as_str()
                .to_string();
            if let Some(rhs) = parts.find(|part| part.as_rule() == Rule::expression) {
                let expression = compile(rhs, file, source, &scope, captures);
                scope.insert(name, expression.clone());
                children.push(expression);
            }
        } else {
            children.push(compile(child, file, source, &scope, captures));
        }
    }
    lower_blocks(children)
}

fn compile_form(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
    captures: &mut CapturedNodes,
) -> ExprValue {
    match pair.as_rule() {
        Rule::if_expr => {
            let mut parts = pair.clone().into_inner();
            let condition = compile(
                parts.next().expect("if grammar condition"),
                file,
                source,
                scope,
                captures,
            );
            let condition_id = condition.node_id();
            let mut children = vec![condition];
            children.extend(parts.map(|part| compile(part, file, source, scope, captures)));
            node_with_roles(
                &pair,
                ExprKind::If,
                children,
                AuthoredChildRoles::If {
                    condition: condition_id,
                },
                file,
                source,
            )
        }
        Rule::list => {
            let children = compile_children(&pair, file, source, scope, captures);
            node(&pair, ExprKind::List, children, file, source)
        }
        Rule::fn_body | Rule::if_body | Rule::else_clause | Rule::for_body => {
            let children = compile_children(&pair, file, source, scope, captures);
            node(&pair, ExprKind::Sequence, children, file, source)
        }
        Rule::map => compile_map(pair, file, source, scope, captures),
        Rule::coalesce_expr => {
            let operands = pair.clone().into_inner().collect::<Vec<_>>();
            let operators = operands
                .windows(2)
                .map(|operands| {
                    DeclarationLocation::coalesce_operator(&operands[0], &operands[1], file, source)
                })
                .collect::<Vec<_>>();
            let mut children = compile_children(&pair, file, source, scope, captures);
            let mut right = children.pop().expect("coalesce operand");
            for (left, operator) in children.into_iter().zip(operators).rev() {
                right = node(&pair, ExprKind::Coalesce, vec![left, right], file, source);
                Arc::make_mut(&mut right.authored).secrecy_operator = Some(operator);
            }
            right
        }
        rule => compile_remaining_form(rule, pair, file, source, scope, captures),
    }
}

fn compile_map(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
    captures: &mut CapturedNodes,
) -> ExprValue {
    let entries = pair
        .clone()
        .into_inner()
        .map(|entry| {
            if entry.as_rule() == Rule::map_entry {
                let mut parts = entry.clone().into_inner();
                let key = parts.next().expect("map key");
                let key_syntax = (key.as_rule() == Rule::string)
                    .then(|| compile(key.clone(), file, source, scope, captures));
                let name = super::AttributeKey::literal_in_file(key, file);
                let value = compile(
                    parts.next().expect("map value"),
                    file,
                    source,
                    scope,
                    captures,
                );
                let key =
                    key_syntax.filter(|key| !matches!(key.authored.kind, ExprKind::Scalar(_)));
                sink_node(&entry, name, value, key, file, source)
            } else {
                compile(entry, file, source, scope, captures)
            }
        })
        .collect::<Vec<_>>();
    let entries = lower_blocks(entries);
    let keys = entries
        .iter()
        .map(|entry| {
            let ExprKind::Sink { name, .. } = &entry.authored.kind else {
                unreachable!("compiled map entry");
            };
            name.clone()
        })
        .collect();
    node(&pair, ExprKind::Map(keys), entries, file, source)
}

fn compile_remaining_form(
    rule: Rule,
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
    captures: &mut CapturedNodes,
) -> ExprValue {
    let mut children = || compile_children(&pair, file, source, scope, captures);
    match rule {
        Rule::expression | Rule::primary => {
            let children = children();
            if children.len() == 1 {
                node(&pair, ExprKind::Alias, children, file, source)
            } else {
                node(&pair, ExprKind::Sequence, children, file, source)
            }
        }
        Rule::pipe_expr => {
            let mut parts = pair.clone().into_inner();
            let mut value = compile(
                parts.next().expect("pipe operand"),
                file,
                source,
                scope,
                captures,
            );
            for function in parts {
                let mut call = compile(function, file, source, scope, captures);
                let authored = Arc::make_mut(&mut call.authored);
                if matches!(authored.kind, ExprKind::Secret) {
                    // The piped input is part of the intrinsic's protected subtree.
                    authored.location = ExpressionLocation::new(
                        ExpressionInstance::Root,
                        &DeclarationLocation::expression_node(&pair, file, source),
                    );
                }
                authored.children.push(value);
                value = call;
            }
            value
        }
        Rule::additive_expr
        | Rule::multiplicative_expr
        | Rule::validate_additive_expr
        | Rule::validate_multiplicative_expr => {
            let mut parts = pair.clone().into_inner();
            let mut value = compile(
                parts.next().expect("integer operand"),
                file,
                source,
                scope,
                captures,
            );
            while let Some(operator) = parts.next() {
                use crate::integer_arithmetic::IntegerOp;
                let op = match operator.as_str() {
                    "+" => IntegerOp::Add,
                    "-" => IntegerOp::Subtract,
                    "*" => IntegerOp::Multiply,
                    "/" => IntegerOp::Divide,
                    "%" => IntegerOp::Remainder,
                    _ => unreachable!("integer operator"),
                };
                let location = DeclarationLocation::expression_node(&operator, file, source);
                let right = compile(
                    parts.next().expect("right integer operand"),
                    file,
                    source,
                    scope,
                    captures,
                );
                value = node(
                    &pair,
                    ExprKind::Integer { op, location },
                    vec![value, right],
                    file,
                    source,
                );
                captures.insert(ParseNodeKey::of(&operator), value.clone());
            }
            value
        }
        Rule::compose_expr => {
            let parts = pair
                .clone()
                .into_inner()
                .filter(|child| !matches!(child.as_rule(), Rule::add_op | Rule::multiplicative_op))
                .map(|child| compile(child, file, source, scope, captures))
                .collect::<Vec<_>>();
            if parts.len() == 1 {
                parts.into_iter().next().unwrap()
            } else {
                node(&pair, ExprKind::Compose, parts, file, source)
            }
        }
        Rule::postfix_expr | Rule::subscripted_id | Rule::for_iterable => {
            let mut parts = pair.clone().into_inner();
            let mut value = compile(
                parts.next().expect("access base"),
                file,
                source,
                scope,
                captures,
            );
            for step in parts {
                if step.as_rule() == Rule::as_keyword {
                    continue;
                }
                if step.as_rule() == Rule::conversion_type_path {
                    let target = Located::new(
                        DeclarationLocation::expression_node(&step, file, source),
                        crate::conversion::ConversionTypePath::new(step.as_str()),
                    );
                    value = node_at(
                        DeclarationLocation::conversion_expression(&pair, &step, file, source),
                        ExprKind::Convert {
                            target: Box::new(target),
                            parsing: conversion::ConversionParsing::new(
                                file.protected_regions()
                                    .owns(step.as_span().start()..step.as_span().end()),
                            ),
                        },
                        vec![value],
                    );
                    continue;
                }
                let operand = step
                    .clone()
                    .into_inner()
                    .find(|child| child.as_rule() == Rule::expression)
                    .map(|child| compile(child, file, source, scope, captures));
                let kind = access_step(step)
                    .map(ExprKind::Access)
                    .unwrap_or(ExprKind::DynamicAccess);
                let mut operands = vec![value];
                operands.extend(operand);
                value = node(&pair, kind, operands, file, source);
            }
            let kind = if pair.as_rule() == Rule::for_iterable {
                ExprKind::Iterable
            } else {
                ExprKind::Alias
            };
            node(&pair, kind, vec![value], file, source)
        }
        Rule::variable_ref => {
            let mut parts = pair.clone().into_inner().peekable();
            let name = parts.next().expect("variable name");
            if !scope.contains_key(name.as_str())
                && parts
                    .peek()
                    .is_some_and(|step| step.as_rule() == Rule::field_access)
            {
                let attribute = parts
                    .next()
                    .expect("first reference field")
                    .as_str()
                    .trim_start_matches('.')
                    .to_string();
                let mut value = node(
                    &pair,
                    ExprKind::Reference {
                        binding: RelativeBindingName::new(name.as_str()),
                        authored_binding: RelativeBindingName::new(name.as_str()),
                        attribute,
                        segments: Vec::new(),
                    },
                    Vec::new(),
                    file,
                    source,
                );
                for step in parts {
                    let operand = step
                        .clone()
                        .into_inner()
                        .find(|child| child.as_rule() == Rule::expression)
                        .map(|child| compile(child, file, source, scope, captures));
                    let kind = access_step(step)
                        .map(ExprKind::Access)
                        .unwrap_or(ExprKind::DynamicAccess);
                    let mut operands = vec![value];
                    operands.extend(operand);
                    value = node(&pair, kind, operands, file, source);
                }
                return value;
            }
            let mut value = match scope.get(name.as_str()) {
                Some(definition) => node(
                    &name,
                    ExprKind::Definition,
                    vec![definition.clone()],
                    file,
                    source,
                ),
                None => node(
                    &name,
                    ExprKind::Binding {
                        binding: RelativeBindingName::new(name.as_str()),
                        authored_binding: RelativeBindingName::new(name.as_str()),
                    },
                    vec![],
                    file,
                    source,
                ),
            };
            for step in parts {
                let operand = step
                    .clone()
                    .into_inner()
                    .find(|child| child.as_rule() == Rule::expression)
                    .map(|child| compile(child, file, source, scope, captures));
                let kind = access_step(step)
                    .map(ExprKind::Access)
                    .unwrap_or(ExprKind::DynamicAccess);
                let mut operands = vec![value];
                operands.extend(operand);
                value = node(&pair, kind, operands, file, source);
            }
            value
        }
        Rule::namespaced_id => {
            let mut parts = pair.as_str().split('.');
            let binding = RelativeBindingName::new(parts.next().expect("reference binding"));
            let attribute = parts.next().expect("reference attribute").to_string();
            let segments: Vec<_> = parts
                .map(|name| PathSegment::Field {
                    name: name.to_string(),
                })
                .collect();
            if let Some(definition) = scope.get(binding.as_str()) {
                let mut value = node(
                    &pair,
                    ExprKind::Definition,
                    vec![definition.clone()],
                    file,
                    source,
                );
                for step in std::iter::once(PathSegment::Field { name: attribute }).chain(segments)
                {
                    value = node(&pair, ExprKind::Access(step), vec![value], file, source);
                }
                return value;
            }
            node(
                &pair,
                ExprKind::Reference {
                    authored_binding: binding.clone(),
                    binding,
                    attribute,
                    segments,
                },
                vec![],
                file,
                source,
            )
        }
        Rule::number => node(&pair, ExprKind::Scalar(TypeExpr::Int), vec![], file, source),
        Rule::float => node(
            &pair,
            ExprKind::Scalar(TypeExpr::Float),
            vec![],
            file,
            source,
        ),
        Rule::boolean => node(
            &pair,
            ExprKind::Scalar(TypeExpr::Bool),
            vec![],
            file,
            source,
        ),
        Rule::duration_literal => node(
            &pair,
            ExprKind::Scalar(TypeExpr::Duration),
            vec![],
            file,
            source,
        ),
        Rule::string => {
            let mut has_literal_parts = false;
            let mut reference_parts = 0;
            let expressions = pair
                .clone()
                .into_inner()
                .flat_map(|quoted| quoted.into_inner())
                .filter(|part| part.as_rule() == Rule::string_part)
                .filter_map(|part| part.into_inner().next())
                .inspect(|part| has_literal_parts |= part.as_rule() == Rule::string_literal)
                .filter(|part| part.as_rule() == Rule::interpolation)
                .map(|part| {
                    let expression = part
                        .clone()
                        .into_inner()
                        .next()
                        .and_then(|wrapper| wrapper.into_inner().next());
                    match expression {
                        Some(expression) => {
                            reference_parts +=
                                usize::from(reference_form(expression.clone()).is_some());
                            compile(expression, file, source, scope, captures)
                        }
                        None => node(&part, ExprKind::EmptyInterpolation, vec![], file, source),
                    }
                })
                .collect::<Vec<_>>();
            node(
                &pair,
                if expressions.is_empty() {
                    ExprKind::Scalar(TypeExpr::String)
                } else {
                    ExprKind::Interpolation {
                        single_reference: !has_literal_parts
                            && expressions.len() == 1
                            && reference_parts == 1,
                    }
                },
                expressions,
                file,
                source,
            )
        }
        Rule::fn_def => {
            let return_type = pair
                .clone()
                .into_inner()
                .find(|part| part.as_rule() == Rule::type_expr)
                .and_then(|annotation| {
                    super::parse_type_expr_str(
                        annotation.as_str(),
                        &super::ProviderContext::new(crate::EnvironmentSnapshot::new([])),
                    )
                });
            let mut scope = scope.clone();
            let mut parameters = Vec::new();
            let mut defaults = Vec::new();
            let mut parameter_nodes = Vec::new();
            let mut default_nodes = Vec::new();
            for parameter in pair
                .clone()
                .into_inner()
                .flatten()
                .filter(|part| part.as_rule() == Rule::fn_param)
            {
                if let Some(default) = parameter
                    .clone()
                    .into_inner()
                    .find(|part| part.as_rule() == Rule::expression)
                {
                    let default = compile(default, file, source, &scope, captures);
                    default_nodes.push(Some(default.node_id()));
                    defaults.push(default);
                } else {
                    default_nodes.push(None);
                }
                let mut parts = parameter.into_inner();
                let name = parts.next().expect("function parameter name");
                if let Some(annotation) = parts.find(|part| part.as_rule() == Rule::type_expr)
                    && let Some(ty) = super::parse_type_expr_str(
                        annotation.as_str(),
                        &super::ProviderContext::new(crate::EnvironmentSnapshot::new([])),
                    )
                {
                    parameters.push((name.as_str().to_string(), Some(ty.clone())));
                    scope.insert(
                        name.as_str().to_string(),
                        node(&name, ExprKind::Parameter(ty), vec![], file, source),
                    );
                } else {
                    parameters.push((name.as_str().to_string(), None));
                    scope.insert(
                        name.as_str().to_string(),
                        node(&name, ExprKind::UntypedParameter, vec![], file, source),
                    );
                }
                parameter_nodes.push(scope[name.as_str()].node_id());
            }
            let body_syntax = pair
                .clone()
                .into_inner()
                .find(|part| part.as_rule() == Rule::fn_body)
                .expect("function body grammar");
            let body_expression = compile(body_syntax, file, source, &scope, captures);
            let body = body_expression.node_id();
            let mut children = vec![body_expression];
            children.extend(defaults);
            node_with_roles(
                &pair,
                ExprKind::Function {
                    name: pair
                        .clone()
                        .into_inner()
                        .next()
                        .unwrap()
                        .as_str()
                        .to_string(),
                    signature: Arc::new(FunctionSignature {
                        parameters,
                        return_type,
                        parameter_nodes,
                        default_nodes,
                    }),
                },
                children,
                AuthoredChildRoles::Function { body },
                file,
                source,
            )
        }
        Rule::for_expr => {
            let mut parts = pair.clone().into_inner();
            let binding = parts.next().expect("loop binding");
            let binding_form = super::expressions::for_expr::parse_for_binding(binding.clone())
                .expect("retained loop binding follows the grammar");
            let iterable_pair = parts.next().expect("loop iterable");
            let iterable_text =
                super::AuthoredExpressionText::redact(iterable_pair.clone(), Some(file));
            let iterable = compile(iterable_pair, file, source, scope, captures);
            let mut scope = scope.clone();
            let pattern = binding.into_inner().next().expect("loop pattern");
            let names = pattern.clone().into_inner().collect::<Vec<_>>();
            for (index, name) in names.iter().enumerate() {
                if name.as_str() == "_" {
                    continue;
                }
                let kind = match (pattern.as_rule(), index) {
                    (Rule::for_indexed_binding, 0) => ExprKind::LoopSlot(TypeExpr::Int),
                    (Rule::for_map_binding, 0) => ExprKind::LoopSlot(TypeExpr::String),
                    _ => ExprKind::Element,
                };
                scope.insert(
                    name.as_str().to_string(),
                    node(name, kind, vec![iterable.clone()], file, source),
                );
            }
            let body = compile(
                parts.next().expect("loop body"),
                file,
                source,
                &scope,
                captures,
            );
            node(
                &pair,
                ExprKind::For {
                    binding: binding_form,
                    bound: captures.bound_for_sites.contains(&ParseNodeKey::of(&pair)),
                    iterable: iterable_text,
                },
                vec![iterable, body],
                file,
                source,
            )
        }
        Rule::function_call | Rule::validate_function_call => {
            let mut parts = pair.clone().into_inner();
            let name = parts.next().expect("function name").as_str().to_string();
            node(
                &pair,
                match crate::builtins::Builtin::from_name(&name) {
                    Some(crate::builtins::Builtin::Secret) => ExprKind::Secret,
                    _ => ExprKind::Call(name),
                },
                parts
                    .map(|child| compile(child, file, source, scope, captures))
                    .collect(),
                file,
                source,
            )
        }
        Rule::resource_expr | Rule::anonymous_resource | Rule::read_resource_expr => {
            let full_type = pair
                .clone()
                .into_inner()
                .next()
                .expect("resource type")
                .as_str();
            let (provider, resource_type) =
                full_type.split_once('.').expect("qualified resource type");
            node(
                &pair,
                ExprKind::Resource {
                    provider: provider.to_string(),
                    resource_type: resource_type.to_string(),
                    read: pair.as_rule() == Rule::read_resource_expr,
                },
                // The qualified resource type is a declaration, not a value
                // reference whose provider head can be shadowed by a binding.
                children().into_iter().skip(1).map(|mut child| {
                    if matches!(&child.authored.kind, ExprKind::Sink { name } if name.transport_identity() == "directives")
                        && let AuthoredChildRoles::Sink { placement, .. } = &mut Arc::make_mut(&mut child.authored).roles
                    {
                        *placement = SinkPlacement::Boundary;
                    }
                    child
                }).collect(),
                file,
                source,
            )
        }
        Rule::attribute => {
            let mut parts = pair.clone().into_inner();
            let name = parts.next().expect("attribute name");
            let key_syntax = (name.as_rule() == Rule::string)
                .then(|| compile(name.clone(), file, source, scope, captures));
            let name = super::AttributeKey::literal_in_file(name, file);
            let value = compile(
                parts.next().expect("attribute value"),
                file,
                source,
                scope,
                captures,
            );
            let key = key_syntax.filter(|key| !matches!(key.authored.kind, ExprKind::Scalar(_)));
            sink_node(&pair, name, value, key, file, source)
        }
        Rule::wait_depends_on_attr | Rule::wait_until_attr | Rule::validation_condition_attr => {
            node_with_roles(
                &pair,
                ExprKind::Declaration,
                children(),
                AuthoredChildRoles::Computed,
                file,
                source,
            )
        }
        Rule::wait_expr => {
            let mut parts = pair.clone().into_inner();
            let target = parts.next().expect("wait target");
            let target = scope.get(target.as_str()).cloned().unwrap_or_else(|| {
                node(
                    &target,
                    ExprKind::Binding {
                        binding: RelativeBindingName::new(target.as_str()),
                        authored_binding: RelativeBindingName::new(target.as_str()),
                    },
                    vec![],
                    file,
                    source,
                )
            });
            let mut operands = vec![target];
            operands.extend(parts.map(|part| compile(part, file, source, scope, captures)));
            node(&pair, ExprKind::Wait, operands, file, source)
        }
        Rule::validate_expr | Rule::validate_primary => {
            node(&pair, ExprKind::Alias, children(), file, source)
        }
        Rule::validate_or_expr
        | Rule::validate_and_expr
        | Rule::validate_not_expr
        | Rule::validate_comparison => {
            let values = pair
                .clone()
                .into_inner()
                .filter(|part| part.as_rule() != Rule::compare_op)
                .map(|part| compile(part, file, source, scope, captures))
                .collect::<Vec<_>>();
            let kind = if values.len() == 1 && !pair.as_str().trim_start().starts_with('!') {
                ExprKind::Alias
            } else {
                ExprKind::Boolean
            };
            node(&pair, kind, values, file, source)
        }
        Rule::let_binding
        | Rule::local_binding
        | Rule::for_local_binding
        | Rule::if_local_binding
        | Rule::fn_local_let => {
            let values: Vec<_> = pair
                .clone()
                .into_inner()
                .filter(|part| matches!(part.as_rule(), Rule::expression | Rule::use_expr))
                .map(|part| compile(part, file, source, scope, captures))
                .collect();
            let kind = if values.is_empty()
                || pair
                    .clone()
                    .into_inner()
                    .any(|part| part.as_rule() == Rule::use_expr)
            {
                ExprKind::Declaration
            } else {
                ExprKind::Sequence
            };
            node(&pair, kind, values, file, source)
        }
        Rule::attributes_param
        | Rule::exports_param
        | Rule::module_call_arg
        | Rule::arg_default_attr => {
            let values = pair
                .clone()
                .into_inner()
                .filter(|part| part.as_rule() == Rule::expression)
                .map(|part| compile(part, file, source, scope, captures))
                .collect();
            node(&pair, ExprKind::Declaration, values, file, source)
        }
        Rule::module_call => node(&pair, ExprKind::ModuleCall, children(), file, source),
        Rule::nested_block => {
            let name_pair = pair.clone().into_inner().next().expect("nested block name");
            let name = super::AttributeKey::authored_in_file(name_pair.clone(), file);
            let header = DeclarationLocation::expression_node(&name_pair, file, source);
            let members = children().into_iter().skip(1).collect::<Vec<_>>();
            let (fields, mut locals): (Vec<_>, Vec<_>) = members
                .into_iter()
                .partition(|member| matches!(member.authored.kind, ExprKind::Sink { .. }));
            let keys = fields
                .iter()
                .map(|field| match &field.authored.kind {
                    ExprKind::Sink { name, .. } => name.clone(),
                    _ => unreachable!(),
                })
                .collect();
            let record = node(&pair, ExprKind::Map(keys), fields, file, source);
            locals.push(record);
            let record = node(&pair, ExprKind::Sequence, locals, file, source);
            node(
                &pair,
                ExprKind::NestedBlock { name, header },
                vec![record],
                file,
                source,
            )
        }
        Rule::provider_block | Rule::provider_expr => node(
            &pair,
            ExprKind::ProviderDeclaration,
            children(),
            file,
            source,
        ),
        Rule::arguments_block
        | Rule::arguments_param
        | Rule::arguments_param_block
        | Rule::arguments_param_attr
        | Rule::arg_description_attr
        | Rule::arg_validation_block
        | Rule::validation_block_attr
        | Rule::validation_error_message_attr
        | Rule::attributes_block
        | Rule::exports_block
        | Rule::backend_block
        | Rule::require_statement
        | Rule::require_keyword
        | Rule::wait_attr
        | Rule::wait_timeout_attr
        | Rule::upstream_state_expr
        | Rule::use_expr
        | Rule::import_state_block
        | Rule::import_state_attr
        | Rule::import_to_attr
        | Rule::import_id_attr
        | Rule::removed_block
        | Rule::removed_attr
        | Rule::moved_block
        | Rule::moved_attr
        | Rule::moved_from_attr
        | Rule::moved_to_attr
        | Rule::resource_address
        | Rule::type_expr
        | Rule::type_expr_atom
        | Rule::type_simple
        | Rule::type_generic
        | Rule::type_ref
        | Rule::resource_type_path
        | Rule::type_struct
        | Rule::struct_field_list
        | Rule::struct_field
        | Rule::type_string_literal
        | Rule::identifier
        | Rule::discard_pattern
        | Rule::reference_prefix
        | Rule::reference_field
        | Rule::null_literal
        | Rule::EOI => node(&pair, ExprKind::Declaration, children(), file, source),
        rule @ (Rule::COMMENT
        | Rule::WHITESPACE
        | Rule::add_op
        | Rule::as_keyword
        | Rule::block_comment
        | Rule::block_content
        | Rule::coalesce_expr
        | Rule::coalesce_separator
        | Rule::coalesce_token
        | Rule::compare_op
        | Rule::conversion_type_path
        | Rule::divide_op
        | Rule::double_quoted_string
        | Rule::duration_unit
        | Rule::else_clause
        | Rule::field_access
        | Rule::file
        | Rule::fn_body
        | Rule::fn_param
        | Rule::fn_params
        | Rule::for_binding
        | Rule::for_body
        | Rule::for_indexed_binding
        | Rule::for_iterable_primary
        | Rule::for_map_binding
        | Rule::for_simple_binding
        | Rule::if_body
        | Rule::if_expr
        | Rule::index_access
        | Rule::inline_ws
        | Rule::interpolation
        | Rule::interpolation_expr
        | Rule::line_comment
        | Rule::list
        | Rule::map
        | Rule::map_entry
        | Rule::minus_op
        | Rule::multiplicative_op
        | Rule::multiply_op
        | Rule::numeric_tail
        | Rule::plus_op
        | Rule::remainder_op
        | Rule::single_quoted_char
        | Rule::single_quoted_content
        | Rule::single_quoted_string
        | Rule::statement
        | Rule::string_char
        | Rule::string_literal
        | Rule::string_part
        | Rule::use_keyword
        | Rule::binding_keyword
        | Rule::block_comment_close
        | Rule::block_comment_content
        | Rule::block_comment_open
        | Rule::call_open
        | Rule::conditional_keyword
        | Rule::double_quote
        | Rule::escape_start
        | Rule::for_head
        | Rule::identifier_continue
        | Rule::if_head
        | Rule::interpolation_close
        | Rule::interpolation_open
        | Rule::iterable_keyword
        | Rule::iteration_keyword
        | Rule::let_head
        | Rule::line_comment_open
        | Rule::paren_close
        | Rule::paren_open
        | Rule::pipe_step
        | Rule::pipe_token
        | Rule::require_condition
        | Rule::require_head
        | Rule::single_quote) => node(
            &pair,
            ExprKind::Unsupported(format!("{rule:?}")),
            children(),
            file,
            source,
        ),
    }
}

fn access_step(pair: Pair<'_, Rule>) -> Option<PathSegment> {
    match pair.as_rule() {
        Rule::field_access => Some(PathSegment::Field {
            name: pair.as_str().trim_start_matches('.').to_string(),
        }),
        Rule::index_access => {
            fn literal(pair: Pair<'_, Rule>) -> Option<Pair<'_, Rule>> {
                if matches!(pair.as_rule(), Rule::number | Rule::string) {
                    return Some(pair);
                }
                let mut children = pair.into_inner();
                let child = children.next()?;
                if children.next().is_some() {
                    return None;
                }
                literal(child)
            }
            let index = literal(pair)?;
            let subscript = match index.as_rule() {
                Rule::number => crate::resource::Subscript::Int {
                    index: index.as_str().parse().ok()?,
                },
                Rule::string => crate::resource::Subscript::Str {
                    key: super::expressions::string_literal::parse_string_literal(index)
                        .ok()?
                        .into(),
                },
                _ => return None,
            };
            Some(PathSegment::Subscript { index: subscript })
        }
        _ => None,
    }
}

use judgement::ConversionEvidence;
pub(in crate::parser) use judgement::unjudged as missing_judgement;
pub(crate) use judgement::{AdmissionContext, admit_ordinary_with_context, admit_with_context};
pub use judgement::{
    CollectionKind, ExpressionAdmission, ExpressionCheckOwner, ExpressionReport, JudgedExpr,
    JudgedExprData, NoConversion, NoTypeProjection, OrdinaryJudgedExpr,
    ProviderAuthorityUnavailable, SinkJudgedExpr, TypeProjection, admit_expression,
};
pub(crate) use judgement::{judge_expressions, judge_expressions_with_upstream};
/// One authored loop's iterable evidence, independent of expansion timing.
pub(crate) struct AuthoredForIterable<'a> {
    pub binding: &'a super::ForBinding,
    pub bound: bool,
    pub location: &'a DeclarationLocation,
    pub expression: String,
    pub judged: Result<&'a JudgedExpr, MissingEvidence>,
    pub key_judged: Result<&'a JudgedExpr, MissingEvidence>,
}

#[cfg(any(test, feature = "test-utils"))]
pub(crate) fn value_dependent_judgement_for_test() -> JudgedExpr {
    judgement::constraint_fixture()
}

// Constructors for judgement evidence are confined to this module. In
// particular, the syntax compiler and provisional evaluators cannot mint it.
pub use conversion::flow::{ConversionRejection, SinkRejection};
mod judgement {
    #[cfg(test)]
    mod diagnostic_order_tests;
    #[cfg(test)]
    mod round2_tests;

    use super::*;
    use crate::static_type::{InferenceError, InferenceIssue, NotInferableReason};
    use crate::validation::inference::{self, InferenceBinding, InferenceBindings};

    #[derive(Clone, Copy)]
    pub(crate) enum EmptyCoalesceDefault {
        None,
        List,
        Map,
    }

    pub(crate) fn coalesce_type(
        left: Result<TypeExpr, InferenceIssue>,
        default: Result<TypeExpr, InferenceIssue>,
        empty: EmptyCoalesceDefault,
    ) -> Result<TypeExpr, InferenceIssue> {
        match (left, default) {
            (Ok(left), Ok(right)) if left == right => Ok(left),
            (Ok(left), Ok(right)) => Err(InferenceError::IncompatibleCoalesceTypes {
                types: crate::schema::MismatchTypeNames::for_type_exprs(&left, &right),
            }
            .into()),
            (Ok(left @ TypeExpr::List(_)), Err(_))
                if matches!(empty, EmptyCoalesceDefault::List) =>
            {
                Ok(left)
            }
            (Ok(left @ TypeExpr::Map(_)), Err(_)) if matches!(empty, EmptyCoalesceDefault::Map) => {
                Ok(left)
            }
            (
                _,
                Err(
                    error @ InferenceIssue::Failed(InferenceError::IncompatibleCoalesceTypes {
                        ..
                    }),
                ),
            ) => Err(error),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Classification {
        Known,
        Deferred,
    }

    impl From<bool> for Classification {
        fn from(known: bool) -> Self {
            if known { Self::Known } else { Self::Deferred }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum InferredKind {
        Ordinary(Classification),
        // Schema-projected references retain the existing binding check and
        // apply-time value validation, rather than acquiring a new nominal
        // declaration obligation at a function boundary.
        Reference(Classification),
        Literal,
        EnumIdentifier,
        StringConstruction(Classification),
        // Builtins keep ordinary assignment diagnostics outside string sinks.
        BuiltinConstruction(Classification),
        BuiltinStringList(Classification),
        List(Classification),
        Map(Classification),
        KeyedGroup(Classification),
        // Loop slots are inferred from the iterable, never annotated sources.
        LoopSlot,
    }

    impl InferredKind {
        fn knowledge(self) -> Classification {
            match self {
                Self::Ordinary(knowledge)
                | Self::Reference(knowledge)
                | Self::StringConstruction(knowledge)
                | Self::BuiltinConstruction(knowledge)
                | Self::BuiltinStringList(knowledge)
                | Self::List(knowledge)
                | Self::Map(knowledge)
                | Self::KeyedGroup(knowledge) => knowledge,
                Self::Literal | Self::EnumIdentifier => Classification::Known,
                Self::LoopSlot => Classification::Deferred,
            }
        }
    }

    /// The intrinsic expression shape does not depend on nominal identity or on
    /// inferring a common element type. In particular an empty list still has
    /// a list shape, and a heterogeneous record is still a map.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ValueShape {
        String,
        Int,
        Float,
        Bool,
        Duration,
        List,
        Map,
        KeyedGroup,
        Handle,
    }

    impl ValueShape {
        fn of_type(ty: &TypeExpr) -> Option<Self> {
            use crate::schema::{RawShape, RefinedScalarKind};
            Some(match ty {
                TypeExpr::String | TypeExpr::StringLiteral(_) => Self::String,
                TypeExpr::Int => Self::Int,
                TypeExpr::Float => Self::Float,
                TypeExpr::Bool => Self::Bool,
                TypeExpr::Duration => Self::Duration,
                TypeExpr::List(_) => Self::List,
                TypeExpr::Map(_) | TypeExpr::Struct { .. } => Self::Map,
                TypeExpr::Ref(_) => Self::Handle,
                TypeExpr::SchemaType { owned } => match owned.scalar_kind() {
                    RefinedScalarKind::String => Self::String,
                    RefinedScalarKind::Int => Self::Int,
                    RefinedScalarKind::Float => Self::Float,
                },
                TypeExpr::Simple(name) => match name.declaration().raw_shape() {
                    RawShape::String { .. } | RawShape::Reference { .. } => Self::String,
                    RawShape::Int { .. } => Self::Int,
                    RawShape::Float { .. } => Self::Float,
                    RawShape::Bool => Self::Bool,
                    RawShape::Duration => Self::Duration,
                    RawShape::List { .. } => Self::List,
                    RawShape::Map { .. } | RawShape::Struct { .. } => Self::Map,
                    _ => return None,
                },
                TypeExpr::Union(members) => {
                    let shape = Self::of_type(members.first()?)?;
                    if !members
                        .iter()
                        .all(|member| Self::of_type(member) == Some(shape))
                    {
                        return None;
                    }
                    shape
                }
                TypeExpr::DottedUnresolved(_) | TypeExpr::UnloadedSchemaType(_) => return None,
            })
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FailureOwner {
        Expression,
        Boundary,
        BindingIndex,
        BindingScope,
        Reported,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum ExprJudgement {
        Typed {
            ty: TypeExpr,
            kind: InferredKind,
            owner: FailureOwner,
        },
        Failed {
            error: InferenceError,
            owner: FailureOwner,
        },
        // A valid source type rejected at this authored sink. Keeping the source
        // lets the diagnostic owner describe it without re-inferring a value.
        Rejected {
            source: Box<ExprJudgement>,
            error: InferenceError,
            owner: FailureOwner,
        },
        ValueDependent {
            reason: NotInferableReason,
            shape: Option<(CollectionKind, Classification)>,
            owner: FailureOwner,
        },
        AuthorityUnavailable(ProviderAuthorityUnavailable),
        Declaration,
    }

    impl ExprJudgement {
        fn static_type(&self) -> Result<&TypeExpr, InferenceIssue> {
            match self {
                ExprJudgement::Typed { ty, .. } => Ok(ty),
                ExprJudgement::Failed { error, .. } => Err(error.clone().into()),
                ExprJudgement::Rejected { source, .. } => source.static_type(),
                ExprJudgement::ValueDependent { reason, .. } => {
                    Err(InferenceIssue::NotInferable(reason.clone()))
                }
                ExprJudgement::AuthorityUnavailable(authority) => {
                    Err(InferenceIssue::NotInferable(match &authority.0 {
                        UnavailableSchema::Resource { binding } => {
                            NotInferableReason::SchemaUnavailable {
                                binding: binding.clone(),
                            }
                        }
                        UnavailableSchema::Named { identity } => {
                            NotInferableReason::CustomTypeSchemaUnavailable {
                                identity: identity.clone(),
                            }
                        }
                    }))
                }
                ExprJudgement::Declaration => Err(InferenceError::UnknownType {
                    reason: "declaration is not a value".into(),
                }
                .into()),
            }
        }
    }

    /// Admission evidence, constructible only by the recursive judge.
    ///
    /// ```compile_fail,E0616
    /// use carina_core::parser::retained::JudgedExpr;
    /// fn overwrite_judgement(expression: &mut JudgedExpr) { expression.judgement = unreachable!(); }
    /// ```
    ///
    /// Construction categories require a sink-judged view.
    /// ```compile_fail,E0599
    /// use carina_core::parser::retained::JudgedExpr;
    /// fn construction(expression: &JudgedExpr) { expression.is_single_string_construction(); }
    /// ```
    /// ```compile_fail,E0599
    /// use carina_core::parser::retained::JudgedExpr;
    /// fn members(expression: &JudgedExpr) { expression.has_constructed_members(); }
    /// ```
    /// ```compile_fail,E0599
    /// use carina_core::parser::retained::JudgedExpr;
    /// fn collection(expression: &JudgedExpr) { expression.collection_kind(); }
    /// ```
    /// ```compile_fail,E0599
    /// use carina_core::parser::retained::JudgedExpr;
    /// fn kind(expression: &JudgedExpr) { expression.kind(); }
    /// ```
    /// ```compile_fail,E0599
    /// use carina_core::parser::retained::JudgedExpr;
    /// fn assignment(expression: &JudgedExpr) { expression.requires_declared_assignment(); }
    /// ```
    #[derive(Debug, Clone)]
    pub struct JudgedExpr {
        data: Arc<JudgedExprData>,
        conversions: conversion::flow::ConversionSources,
    }
    #[derive(Debug, Clone)]
    struct InferredExpr {
        data: Arc<JudgedExprData>,
    }
    impl From<&JudgedExpr> for InferredExpr {
        fn from(expression: &JudgedExpr) -> Self {
            Self {
                data: expression.data.clone(),
            }
        }
    }
    impl std::ops::Deref for InferredExpr {
        type Target = JudgedExprData;
        fn deref(&self) -> &Self::Target {
            &self.data
        }
    }
    /// A metadata query has no authority to admit a carrying expression.
    pub enum TypeProjection<'a> {
        Available(OrdinaryJudgedExpr<'a>),
        NoProjection(NoTypeProjection),
    }
    pub struct NoTypeProjection {
        _private: (),
    }
    /// A conversion-free source frame. The flag owner alone can construct it.
    /// Child selections stay within that frame, including declaration metadata
    /// behind a deliberate NonCrossed edge.
    ///
    /// ```compile_fail,E0451
    /// use carina_core::parser::retained::OrdinaryJudgedExpr;
    /// let _ = OrdinaryJudgedExpr { data: unreachable!(), proof: unreachable!() };
    /// ```
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct OrdinaryJudgedExpr<'a> {
        data: &'a JudgedExprData,
        proof: NoConversion,
    }
    impl std::ops::Deref for OrdinaryJudgedExpr<'_> {
        type Target = JudgedExprData;
        fn deref(&self) -> &Self::Target {
            self.data
        }
    }
    impl<'a> OrdinaryJudgedExpr<'a> {
        pub(crate) fn element(self, index: usize) -> Result<Self, MissingEvidence> {
            self.data.element(index).map(|child| Self {
                data: &child.data,
                proof: self.proof,
            })
        }
        pub(crate) fn field(self, key: &str) -> Result<Self, MissingEvidence> {
            self.data.field(key).map(|child| Self {
                data: &child.data,
                proof: self.proof,
            })
        }
        pub(crate) fn branch(self, index: usize) -> Result<Self, MissingEvidence> {
            self.data.branch(index).map(|child| Self {
                data: &child.data,
                proof: self.proof,
            })
        }
        pub(crate) fn coalesce_operand(self, index: usize) -> Result<Self, MissingEvidence> {
            self.data.coalesce_operand(index).map(|child| Self {
                data: &child.data,
                proof: self.proof,
            })
        }
        pub(crate) fn for_sink<'s>(
            &'s self,
            sink: &'s TypeExpr,
            config: &'s super::super::ProviderContext,
        ) -> OrdinarySinkJudgedExpr<'s> {
            OrdinarySinkJudgedExpr {
                expression: self.data.for_sink(sink, config),
                proof: self.proof,
            }
        }
    }
    pub(crate) struct OrdinarySinkJudgedExpr<'a> {
        expression: SinkJudgedExpr<'a>,
        proof: NoConversion,
    }
    impl<'a> std::ops::Deref for OrdinarySinkJudgedExpr<'a> {
        type Target = SinkJudgedExpr<'a>;
        fn deref(&self) -> &Self::Target {
            &self.expression
        }
    }
    impl OrdinarySinkJudgedExpr<'_> {
        pub(crate) fn source(&self) -> OrdinaryJudgedExpr<'_> {
            OrdinaryJudgedExpr {
                data: &self.expression.expression,
                proof: self.proof,
            }
        }
    }
    impl PartialEq for JudgedExpr {
        fn eq(&self, other: &Self) -> bool {
            self.data == other.data
                && self.contains_any_conversion() == other.contains_any_conversion()
        }
    }

    /// Proof required before an ordinary judgement can admit a value. Only a
    /// conversion-free judgement or an operational projected stage can mint it.
    ///
    /// ```compile_fail,E0451
    /// use carina_core::parser::retained::NoConversion;
    /// let _ = NoConversion { _private: unreachable!() };
    /// ```
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct NoConversion {
        _private: NoConversionOrigin,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum NoConversionOrigin {
        Judgement(usize),
        Operational(super::ProjectedPhase),
    }
    pub(super) enum ConversionEvidence<'a> {
        Free(NoConversion),
        Carried(conversion::flow::CarriedConversion<'a>),
    }
    impl NoConversion {
        pub(crate) fn operational(phase: super::ProjectedPhase) -> Self {
            Self {
                _private: NoConversionOrigin::Operational(phase),
            }
        }
    }

    #[doc(hidden)]
    #[derive(Clone)]
    pub struct JudgedExprData {
        diagnostic_scope: DiagnosticScope,
        origin: Option<ExpressionLocation>,
        boundary_owner: Option<Arc<DeclarationLocation>>,
        judgement: ExprJudgement,
        authority: Option<Arc<super::super::ProviderContext>>,
        coalesce: Option<Arc<[JudgedExpr; 2]>>,
        alternatives: Arc<Vec<JudgedExpr>>,
        member: Option<Arc<JudgedExpr>>,
        elements: Arc<Vec<JudgedExpr>>,
        fields: Arc<crate::parser::ConfigurationAttributes<JudgedExpr>>,
        converted: Option<Arc<conversion::ConversionTypeJudgement>>,
        structural_source: bool,
        static_source: Option<Arc<StaticSourceReceipt>>,
    }

    impl std::fmt::Debug for JudgedExprData {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if self.diagnostic_scope.is_secret() {
                return formatter.write_str(crate::schema::SECRET_VALUE_MASK);
            }
            formatter
                .debug_struct("JudgedExprData")
                .field("origin", &self.origin)
                .field("boundary_owner", &self.boundary_owner)
                .field("judgement", &self.judgement)
                .field("authority", &self.authority)
                .field("coalesce", &self.coalesce)
                .field("alternatives", &self.alternatives)
                .field("member", &self.member)
                .field("elements", &self.elements)
                .field("fields", &self.fields)
                .finish()
        }
    }

    impl PartialEq for JudgedExprData {
        fn eq(&self, other: &Self) -> bool {
            self.diagnostic_scope == other.diagnostic_scope
                && self.origin == other.origin
                && self.boundary_owner == other.boundary_owner
                && self.judgement == other.judgement
                && match (&self.authority, &other.authority) {
                    (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                    (None, None) => true,
                    (Some(_), None) | (None, Some(_)) => false,
                }
                && self.coalesce == other.coalesce
                && self.alternatives == other.alternatives
                && self.member == other.member
                && self.elements == other.elements
                && self.fields == other.fields
        }
    }

    impl std::ops::Deref for JudgedExpr {
        type Target = JudgedExprData;
        fn deref(&self) -> &Self::Target {
            &self.data
        }
    }

    impl JudgedExpr {
        fn build(
            mut data: Arc<JudgedExprData>,
            edges: conversion::flow::JudgementEdges<'_>,
        ) -> Self {
            match &edges {
                conversion::flow::JudgementEdges::Convert {
                    site,
                    target,
                    operand,
                    bare,
                } => {
                    Arc::make_mut(&mut data).converted =
                        Some(Arc::new(conversion::ConversionTypeJudgement::new(
                            site,
                            target,
                            (*operand).clone(),
                            *bare,
                        )));
                }
                conversion::flow::JudgementEdges::NonCrossed(_)
                | conversion::flow::JudgementEdges::MissingReference(_) => {
                    Arc::make_mut(&mut data).converted = None
                }
                conversion::flow::JudgementEdges::Leaf
                | conversion::flow::JudgementEdges::Follow { .. }
                | conversion::flow::JudgementEdges::Selected(_) => {}
            }
            Self {
                data,
                conversions: edges.sources(),
            }
        }
        fn failed(&self) -> bool {
            self.data.failed()
        }
        pub(crate) fn statically_checked(&self) -> bool {
            self.static_source.as_ref().is_some_and(|receipt| {
                self.converted.is_some()
                    || matches!(
                        receipt.result,
                        Err(static_admission::JudgeSourceError::Conversion)
                    )
            })
        }
        pub(in crate::parser::retained) fn static_rejection(&self) -> Option<&StaticSourceReceipt> {
            self.static_source.as_deref().filter(|receipt| {
                matches!(
                    receipt.result,
                    Err(static_admission::JudgeSourceError::Conversion)
                )
            })
        }
        pub(in crate::parser::retained) fn conversion_type_judgement(
            &self,
        ) -> Option<&conversion::ConversionTypeJudgement> {
            self.converted.as_deref()
        }
        pub(super) fn conversion_evidence(&self) -> ConversionEvidence<'_> {
            match self.conversions.carried() {
                Some(carried) => ConversionEvidence::Carried(carried),
                None => ConversionEvidence::Free(NoConversion {
                    _private: NoConversionOrigin::Judgement(self.judgement_stamp()),
                }),
            }
        }
        pub(super) fn judgement_stamp(&self) -> usize {
            Arc::as_ptr(&self.data) as usize
        }
        /// Metadata queries cannot publish findings for this parse's NodeIds.
        pub fn type_projection(&self) -> TypeProjection<'_> {
            match self.conversion_evidence() {
                ConversionEvidence::Free(proof) => TypeProjection::Available(OrdinaryJudgedExpr {
                    data: &self.data,
                    proof,
                }),
                ConversionEvidence::Carried(_) => {
                    TypeProjection::NoProjection(NoTypeProjection { _private: () })
                }
            }
        }
        pub(crate) fn operational_sink(
            &self,
            phase: super::ProjectedPhase,
            _boundary: conversion::flow::OperationalExportBoundary,
        ) -> OrdinaryJudgedExpr<'_> {
            OrdinaryJudgedExpr {
                data: &self.data,
                proof: NoConversion::operational(phase),
            }
        }
        pub fn ordinary(&self) -> Result<OrdinaryJudgedExpr<'_>, ConversionRejection> {
            match self.no_conversion() {
                Some(proof) => Ok(OrdinaryJudgedExpr {
                    data: &self.data,
                    proof,
                }),
                None => Err(ConversionRejection::at_sink(self)),
            }
        }
        pub fn no_conversion(&self) -> Option<NoConversion> {
            match self.conversion_evidence() {
                ConversionEvidence::Free(proof) => Some(proof),
                ConversionEvidence::Carried(_) => None,
            }
        }
        pub(super) fn conversion_sources(&self) -> &conversion::flow::ConversionSources {
            &self.conversions
        }

        fn sink_owner(&self, sink: &TypeExpr) -> FailureOwner {
            if self
                .authority
                .as_ref()
                .is_some_and(|config| self.for_sink(sink, config).requires_declared_assignment())
                && self.static_type() == Ok(&TypeExpr::String)
                && sink.is_string_shaped()
                && !matches!(sink, TypeExpr::String)
            {
                return FailureOwner::Expression;
            }
            if let Some((_, owner)) = self.failure() {
                return owner;
            }
            if let Some(branch) = self
                .alternatives
                .iter()
                .find(|branch| admit_expression(branch, sink).is_err())
            {
                return branch.sink_owner(sink);
            }
            match self.check_owner() {
                ExpressionCheckOwner::BindingIndex => FailureOwner::BindingIndex,
                ExpressionCheckOwner::BindingScope => FailureOwner::BindingScope,
                ExpressionCheckOwner::Reported => FailureOwner::Reported,
                ExpressionCheckOwner::Expression if self.concrete() => FailureOwner::Boundary,
                ExpressionCheckOwner::Expression => FailureOwner::Expression,
            }
        }

        pub(crate) fn contains_any_conversion(&self) -> bool {
            !self.conversions.is_empty()
        }
    }

    impl JudgedExprData {
        /// Interpret builtin evidence as an ordinary result at other sinks.
        /// This also preserves the original collection diagnostic granularity.
        pub fn for_sink<'a>(
            &'a self,
            sink: &'a TypeExpr,
            config: &'a super::super::ProviderContext,
        ) -> SinkJudgedExpr<'a> {
            fn string_sink(actual: &TypeExpr, sink: &TypeExpr) -> bool {
                match sink {
                    TypeExpr::Union(members) => {
                        members.iter().any(|sink| string_sink(actual, sink))
                    }
                    TypeExpr::List(inner) => match actual {
                        TypeExpr::List(member) => string_sink(member, inner),
                        TypeExpr::String
                        | TypeExpr::Bool
                        | TypeExpr::Int
                        | TypeExpr::Float
                        | TypeExpr::Duration
                        | TypeExpr::Simple(_)
                        | TypeExpr::UnloadedSchemaType(_)
                        | TypeExpr::Map(_)
                        | TypeExpr::Ref(_)
                        | TypeExpr::DottedUnresolved(_)
                        | TypeExpr::SchemaType { .. }
                        | TypeExpr::Struct { .. }
                        | TypeExpr::StringLiteral(_)
                        | TypeExpr::Union(_) => false,
                    },
                    TypeExpr::String
                    | TypeExpr::Bool
                    | TypeExpr::Int
                    | TypeExpr::Float
                    | TypeExpr::Duration
                    | TypeExpr::Simple(_)
                    | TypeExpr::UnloadedSchemaType(_)
                    | TypeExpr::Map(_)
                    | TypeExpr::Ref(_)
                    | TypeExpr::DottedUnresolved(_)
                    | TypeExpr::SchemaType { .. }
                    | TypeExpr::Struct { .. }
                    | TypeExpr::StringLiteral(_) => {
                        actual == &TypeExpr::String && sink.is_string_shaped()
                    }
                }
            }
            let expression = if matches!(
                self.judgement,
                ExprJudgement::Typed {
                    kind: InferredKind::BuiltinConstruction(_) | InferredKind::BuiltinStringList(_),
                    ..
                }
            ) && let (Ok(actual), Ok(sink)) = (
                self.static_type(),
                crate::validation::resolve_type_expr(sink, config),
            ) && !string_sink(actual, &sink)
            {
                let mut ordinary = self.clone();
                if let ExprJudgement::Typed { kind, .. } = &mut ordinary.judgement {
                    *kind = InferredKind::Ordinary(kind.knowledge());
                }
                ordinary.member = None;
                std::borrow::Cow::Owned(ordinary)
            } else {
                std::borrow::Cow::Borrowed(self)
            };
            SinkJudgedExpr {
                expression,
                sink,
                config,
            }
        }
    }

    trait InferenceMut: std::ops::Deref<Target = JudgedExprData> + Sized {
        fn data_mut(&mut self) -> &mut JudgedExprData;
        fn report_at(&mut self, owner: FailureOwner) {
            match &mut self.data_mut().judgement {
                ExprJudgement::Typed { owner: current, .. }
                | ExprJudgement::Failed { owner: current, .. }
                | ExprJudgement::Rejected { owner: current, .. }
                | ExprJudgement::ValueDependent { owner: current, .. } => *current = owner,
                ExprJudgement::AuthorityUnavailable(_) | ExprJudgement::Declaration => {}
            }
        }
        fn with_kind(mut self, kind: impl FnOnce(Classification) -> InferredKind) -> Self {
            self.set_kind(kind);
            self
        }
        fn set_kind(&mut self, kind: impl FnOnce(Classification) -> InferredKind) {
            if let ExprJudgement::Typed { kind: current, .. } = &mut self.data_mut().judgement {
                *current = kind(current.knowledge());
            }
        }
        fn materialize_output<
            P: crate::resource::PlaceholderKind,
            B: crate::resource::BindingAddress,
        >(
            &mut self,
            _value: &crate::resource::knowledge::KnownValue<'_, P, B>,
        ) {
            if let ExprJudgement::Typed { kind, .. } = &mut self.data_mut().judgement {
                *kind = match *kind {
                    InferredKind::StringConstruction(_) => {
                        InferredKind::StringConstruction(Classification::Known)
                    }
                    InferredKind::BuiltinConstruction(_) => {
                        InferredKind::BuiltinConstruction(Classification::Known)
                    }
                    InferredKind::BuiltinStringList(_) => {
                        InferredKind::BuiltinStringList(Classification::Known)
                    }
                    other => other,
                };
            }
        }
        fn inherit_output_materialization(&mut self, previous: &JudgedExpr) {
            // Refresh carries an already-judged output fact. It neither
            // classifies a value nor constructs a new value-knowledge proof.
            if previous.concrete()
                && let ExprJudgement::Typed { kind, .. } = &mut self.data_mut().judgement
            {
                *kind = match *kind {
                    InferredKind::StringConstruction(_) => {
                        InferredKind::StringConstruction(Classification::Known)
                    }
                    InferredKind::BuiltinConstruction(_) => {
                        InferredKind::BuiltinConstruction(Classification::Known)
                    }
                    InferredKind::BuiltinStringList(_) => {
                        InferredKind::BuiltinStringList(Classification::Known)
                    }
                    other => other,
                };
            }
        }
        fn reject(&mut self, error: InferenceError, owner: FailureOwner) {
            self.data_mut().judgement = ExprJudgement::Rejected {
                source: Box::new(self.judgement.clone()),
                error,
                owner,
            };
        }
        fn mark_failed(&mut self, error: InferenceError, owner: FailureOwner) {
            self.data_mut().judgement = ExprJudgement::Failed { error, owner };
        }
        fn owned_by_bindings(mut self) -> Self {
            self.report_at(FailureOwner::BindingIndex);
            self
        }
    }
    impl InferenceMut for JudgedExpr {
        fn data_mut(&mut self) -> &mut JudgedExprData {
            Arc::make_mut(&mut self.data)
        }
    }
    impl InferenceMut for InferredExpr {
        fn data_mut(&mut self) -> &mut JudgedExprData {
            Arc::make_mut(&mut self.data)
        }
    }
    impl JudgedExprData {
        pub(crate) fn concrete(&self) -> bool {
            matches!(self.judgement, ExprJudgement::Typed { kind, .. }
                if kind.knowledge() == Classification::Known)
                || matches!(
                    self.judgement,
                    ExprJudgement::ValueDependent {
                        shape: Some((_, Classification::Known)),
                        ..
                    }
                )
        }
        fn failed(&self) -> bool {
            self.failure().is_some()
        }
        fn failure(&self) -> Option<(&InferenceError, FailureOwner)> {
            match &self.judgement {
                ExprJudgement::Failed { error, owner }
                | ExprJudgement::Rejected { error, owner, .. } => Some((error, *owner)),
                ExprJudgement::Typed { .. }
                | ExprJudgement::ValueDependent { .. }
                | ExprJudgement::AuthorityUnavailable(_)
                | ExprJudgement::Declaration => None,
            }
        }
        pub(crate) fn diagnostic_is_secret(&self) -> bool {
            self.diagnostic_scope.is_secret()
        }
        pub fn static_type(&self) -> Result<&TypeExpr, InferenceIssue> {
            self.judgement.static_type()
        }
        pub(crate) fn has_list_elements(&self) -> bool {
            !self.elements.is_empty()
        }
        pub(crate) fn elements(&self) -> impl Iterator<Item = &JudgedExpr> {
            self.elements
                .iter()
                .chain(self.member.iter().map(Arc::as_ref))
        }
        pub(crate) fn fields(
            &self,
        ) -> impl Iterator<Item = (&crate::parser::AttributeKey, &JudgedExpr)> {
            self.fields.iter()
        }
        pub fn coalesce_operands(&self) -> Option<&[JudgedExpr; 2]> {
            self.coalesce.as_deref()
        }
        pub fn element(&self, index: usize) -> Result<&JudgedExpr, MissingEvidence> {
            self.elements
                .get(index)
                .or(self.member.as_deref())
                .ok_or_else(|| MissingEvidence::new(format!("list element {index}")))
        }
        pub fn field(&self, key: &str) -> Result<&JudgedExpr, MissingEvidence> {
            self.fields
                .get(key)
                .or(self.member.as_deref())
                .ok_or_else(|| MissingEvidence::new(format!("field {key}")))
        }
        pub fn branch(&self, index: usize) -> Result<&JudgedExpr, MissingEvidence> {
            self.alternatives
                .get(index)
                .ok_or_else(|| MissingEvidence::new(format!("branch {index}")))
        }
        pub(crate) fn assignment_source_type(&self) -> Option<&TypeExpr> {
            match &self.judgement {
                ExprJudgement::Typed { ty, .. } => Some(ty),
                ExprJudgement::Rejected { source, .. } => match source.as_ref() {
                    ExprJudgement::Typed { ty, .. } => Some(ty),
                    ExprJudgement::Rejected { .. }
                    | ExprJudgement::Failed { .. }
                    | ExprJudgement::ValueDependent { .. }
                    | ExprJudgement::AuthorityUnavailable(_)
                    | ExprJudgement::Declaration => None,
                },
                ExprJudgement::Failed { .. }
                | ExprJudgement::ValueDependent { .. }
                | ExprJudgement::AuthorityUnavailable(_)
                | ExprJudgement::Declaration => None,
            }
        }
        fn is_enum_identifier(&self) -> bool {
            fn is_enum(judgement: &ExprJudgement) -> bool {
                match judgement {
                    ExprJudgement::Typed {
                        kind: InferredKind::EnumIdentifier,
                        ..
                    } => true,
                    ExprJudgement::Rejected { source, .. } => is_enum(source),
                    _ => false,
                }
            }
            is_enum(&self.judgement)
        }
        fn preserves_reference_assignment(&self) -> bool {
            fn reference(judgement: &ExprJudgement) -> bool {
                match judgement {
                    ExprJudgement::Typed {
                        kind: InferredKind::Ordinary(_) | InferredKind::Reference(_),
                        ..
                    } => true,
                    ExprJudgement::Rejected { source, .. } => reference(source),
                    _ => false,
                }
            }
            reference(&self.judgement) && self.alternatives.is_empty()
        }
        pub fn assignment_value<'a, P: ValueKind, B: BindingAddress>(
            &self,
            value: &'a Value<P, B>,
        ) -> &'a Value<P, B> {
            let reference_assignment = self.preserves_reference_assignment()
                || (!self.alternatives.is_empty()
                    && self
                        .alternatives
                        .iter()
                        .all(|expression| expression.preserves_reference_assignment()));
            if reference_assignment
                && let Value::Deferred(crate::resource::DeferredValue::Interpolation(parts)) = value
                && let [crate::resource::InterpolationPart::Expr(inner)] = parts.as_slice()
            {
                return inner;
            }
            value
        }
        pub(crate) fn is_value_dependent(&self) -> bool {
            matches!(
                self.judgement,
                ExprJudgement::ValueDependent { shape: None, .. }
            )
        }
        pub fn check_owner(&self) -> ExpressionCheckOwner {
            match &self.judgement {
                ExprJudgement::Failed { owner, .. }
                | ExprJudgement::Rejected { owner, .. }
                | ExprJudgement::Typed { owner, .. }
                | ExprJudgement::ValueDependent { owner, .. } => match owner {
                    FailureOwner::Expression | FailureOwner::Boundary => {
                        ExpressionCheckOwner::Expression
                    }
                    FailureOwner::BindingIndex => ExpressionCheckOwner::BindingIndex,
                    FailureOwner::BindingScope => ExpressionCheckOwner::BindingScope,
                    FailureOwner::Reported => ExpressionCheckOwner::Reported,
                },
                ExprJudgement::AuthorityUnavailable(_) | ExprJudgement::Declaration => {
                    ExpressionCheckOwner::Expression
                }
            }
        }
        pub fn boundary_owner(&self) -> Option<&DeclarationLocation> {
            self.boundary_owner.as_deref()
        }
        pub fn coalesce_operand(&self, index: usize) -> Result<&JudgedExpr, MissingEvidence> {
            self.coalesce
                .as_ref()
                .and_then(|operands| operands.get(index))
                .ok_or_else(|| MissingEvidence::new(format!("coalesce operand {index}")))
        }
        fn value_shape(&self) -> Option<ValueShape> {
            if let Some(first) = self.alternatives.first() {
                let shape = first.value_shape()?;
                return self
                    .alternatives
                    .iter()
                    .all(|branch| branch.value_shape() == Some(shape))
                    .then_some(shape);
            }
            fn collection_shape(judgement: &ExprJudgement) -> Option<ValueShape> {
                match judgement {
                    ExprJudgement::Typed {
                        kind: InferredKind::List(_),
                        ..
                    }
                    | ExprJudgement::ValueDependent {
                        shape: Some((CollectionKind::List, _)),
                        ..
                    } => Some(ValueShape::List),
                    ExprJudgement::Typed {
                        kind: InferredKind::Map(_),
                        ..
                    }
                    | ExprJudgement::ValueDependent {
                        shape: Some((CollectionKind::Map, _)),
                        ..
                    } => Some(ValueShape::Map),
                    ExprJudgement::Typed {
                        kind: InferredKind::KeyedGroup(_),
                        ..
                    }
                    | ExprJudgement::ValueDependent {
                        shape: Some((CollectionKind::KeyedGroup, _)),
                        ..
                    } => Some(ValueShape::KeyedGroup),
                    ExprJudgement::Rejected { source, .. } => collection_shape(source),
                    _ => None,
                }
            }
            collection_shape(&self.judgement)
                .or_else(|| self.static_type().ok().and_then(ValueShape::of_type))
        }
        fn type_description(&self) -> String {
            self.static_type()
                .map(ToString::to_string)
                .unwrap_or_else(|_| {
                    self.value_shape()
                        .map(|shape| match shape {
                            ValueShape::KeyedGroup => "keyed loop group".into(),
                            shape => format!("{shape:?}"),
                        })
                        .unwrap_or_else(|| "value-dependent expression".into())
                })
        }
    }
    /// Expression evidence interpreted at a particular sink. Only this view
    /// exposes construction and assignment categories.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) struct StaticSourceReceipt {
        expected: TypeExpr,
        result: Result<ExpressionAdmission, static_admission::JudgeSourceError>,
    }
    impl StaticSourceReceipt {
        pub(crate) fn binding_result(&self, expected: &TypeExpr) -> Result<(), InferenceError> {
            if expected != &self.expected {
                return Err(MissingEvidence::new("static source receipt destination").into());
            }
            match &self.result {
                Ok(_) | Err(static_admission::JudgeSourceError::Conversion) => Ok(()),
                Err(static_admission::JudgeSourceError::Ordinary(error)) => Err(error.clone()),
            }
        }
    }
    pub struct SinkJudgedExpr<'a> {
        expression: std::borrow::Cow<'a, JudgedExprData>,
        sink: &'a TypeExpr,
        config: &'a super::super::ProviderContext,
    }

    impl std::ops::Deref for SinkJudgedExpr<'_> {
        type Target = JudgedExprData;
        fn deref(&self) -> &Self::Target {
            &self.expression
        }
    }

    impl JudgedExpr {
        pub(crate) fn binding_diagnostic_owner(
            &self,
            expected: Option<&TypeExpr>,
            fallback: &TypeExpr,
            config: &crate::parser::ProviderContext,
        ) -> BindingDiagnosticOwner<'_> {
            let Some(proof) = self.no_conversion() else {
                return match self.static_source.as_deref() {
                    Some(receipt) => {
                        BindingDiagnosticOwner::JudgedConversion(Arc::new(receipt.clone()))
                    }
                    None => BindingDiagnosticOwner::WalkOwnedConversion,
                };
            };
            let ordinary = OrdinaryJudgedExpr {
                data: &self.data,
                proof,
            };
            let view = self.for_sink(expected.unwrap_or(fallback), config);
            if view.is_interpolation()
                || (view.is_single_string_construction()
                    && expected.is_some_and(|sink| admit_expression(self, sink).is_ok()))
            {
                BindingDiagnosticOwner::AuthoredExpression
            } else {
                BindingDiagnosticOwner::OrdinarySource(ordinary)
            }
        }
    }

    impl SinkJudgedExpr<'_> {
        fn kind(&self) -> Option<InferredKind> {
            fn read(judgement: &ExprJudgement) -> Option<InferredKind> {
                match judgement {
                    ExprJudgement::Typed { kind, .. } => Some(*kind),
                    ExprJudgement::Rejected { source, .. } => read(source),
                    ExprJudgement::ValueDependent {
                        shape: Some((CollectionKind::List, knowledge)),
                        ..
                    } => Some(InferredKind::List(*knowledge)),
                    ExprJudgement::ValueDependent {
                        shape: Some((CollectionKind::Map, knowledge)),
                        ..
                    } => Some(InferredKind::Map(*knowledge)),
                    ExprJudgement::ValueDependent {
                        shape: Some((CollectionKind::KeyedGroup, knowledge)),
                        ..
                    } => Some(InferredKind::KeyedGroup(*knowledge)),
                    _ => None,
                }
            }
            read(&self.expression.judgement)
        }

        pub(crate) fn requires_declared_assignment(&self) -> bool {
            matches!(
                self.kind(),
                Some(InferredKind::Ordinary(_) | InferredKind::Reference(_))
            ) && self.alternatives.is_empty()
        }
        /// This entire value is one constructed string, never a collection.
        pub fn is_single_string_construction(&self) -> bool {
            self.alternatives.is_empty()
                && matches!(
                    self.kind(),
                    Some(
                        InferredKind::StringConstruction(_) | InferredKind::BuiltinConstruction(_)
                    )
                )
        }

        pub(crate) fn is_interpolation(&self) -> bool {
            self.alternatives.is_empty()
                && matches!(self.kind(), Some(InferredKind::StringConstruction(_)))
        }

        /// Collection members carry evidence at their own element sinks.
        /// This never grants scalar string admission.
        pub fn has_constructed_members(&self) -> bool {
            self.constructed_members(false)
        }

        pub(crate) fn has_builtin_constructed_members(&self) -> bool {
            self.constructed_members(true)
        }

        fn constructed_members(&self, builtin_only: bool) -> bool {
            let Ok(sink) = crate::validation::resolve_type_expr(self.sink, self.config) else {
                return false;
            };
            match sink {
                TypeExpr::List(inner) if self.collection_kind() == Some(CollectionKind::List) => {
                    self.elements().any(|child| {
                        let child = child.for_sink(&inner, self.config);
                        (child.is_single_string_construction()
                            && (!builtin_only
                                || matches!(
                                    child.kind(),
                                    Some(InferredKind::BuiltinConstruction(_))
                                )))
                            || child.constructed_members(builtin_only)
                    })
                }
                TypeExpr::Union(members) => members.iter().any(|sink| {
                    self.expression
                        .for_sink(sink, self.config)
                        .constructed_members(builtin_only)
                }),
                _ => false,
            }
        }

        pub fn collection_kind(&self) -> Option<CollectionKind> {
            match self.kind() {
                Some(InferredKind::List(_)) => Some(CollectionKind::List),
                Some(InferredKind::BuiltinStringList(_))
                    if matches!(self.static_type(), Ok(TypeExpr::List(_))) =>
                {
                    Some(CollectionKind::List)
                }
                Some(InferredKind::Map(_)) => Some(CollectionKind::Map),
                Some(InferredKind::KeyedGroup(_)) => Some(CollectionKind::KeyedGroup),
                _ => None,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum CollectionKind {
        List,
        Map,
        /// A structural loop group with string subscripts, never a map value.
        KeyedGroup,
    }

    /// The validator responsible for a diagnostic. This does not change
    /// admission: failed and missing evidence remain errors at every sink.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ExpressionCheckOwner {
        Expression,
        BindingIndex,
        BindingScope,
        Reported,
    }

    /// Opaque proof that a provider schema is unavailable. Missing authored
    /// evidence cannot construct this token.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ProviderAuthorityUnavailable(UnavailableSchema);

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum UnavailableSchema {
        Resource {
            binding: String,
        },
        Named {
            identity: crate::schema::TypeIdentity,
        },
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ExpressionAdmission {
        ValueCheck,
        DeferredInterpolation,
        ProviderPending(ProviderAuthorityUnavailable),
    }

    fn combine_admission(
        left: ExpressionAdmission,
        right: ExpressionAdmission,
    ) -> ExpressionAdmission {
        use ExpressionAdmission::{DeferredInterpolation, ProviderPending, ValueCheck};
        match (left, right) {
            (
                pending @ ProviderPending(_),
                ValueCheck | DeferredInterpolation | ProviderPending(_),
            )
            | (ValueCheck | DeferredInterpolation, pending @ ProviderPending(_)) => pending,
            (DeferredInterpolation, ValueCheck | DeferredInterpolation)
            | (ValueCheck, DeferredInterpolation) => DeferredInterpolation,
            (ValueCheck, ValueCheck) => ValueCheck,
        }
    }
    fn ordinary_value_check(_proof: NoConversion) -> ExpressionAdmission {
        ExpressionAdmission::ValueCheck
    }

    /// Static admission requires the judge's evidence, never a projection.
    ///
    /// ```compile_fail,E0308
    /// use carina_core::{parser::{TypeExpr, retained::admit_expression}, resource::{Value, ConcreteValue}};
    /// let reduced = Value::<carina_core::resource::ParserPlaceholder, carina_core::resource::RelativeBindingName>::Concrete(ConcreteValue::String("selected".into()));
    /// admit_expression(&reduced, &TypeExpr::String);
    /// ```
    pub fn admit_expression(
        expression: &JudgedExpr,
        sink: &TypeExpr,
    ) -> Result<ExpressionAdmission, SinkRejection<InferenceError>> {
        match &expression.authority {
            Some(authority) => {
                admit_with_context(expression, sink, AdmissionContext::Assignment, authority)
            }
            None => Err(InferenceError::from(MissingEvidence::new(
                "expression judgement authority",
            ))
            .into()),
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum AdmissionContext {
        Assignment,
        Function,
        // Struct literal leaves retain their concrete value diagnostics.
        StructValue,
        Resource,
    }

    pub(crate) fn admit_with_context(
        expression: &JudgedExpr,
        sink: &TypeExpr,
        context: AdmissionContext,
        config: &super::super::ProviderContext,
    ) -> Result<ExpressionAdmission, SinkRejection<InferenceError>> {
        let source = expression.ordinary().map_err(SinkRejection::Conversion)?;
        admit_ordinary_with_context(source, sink, context, config).map_err(SinkRejection::Ordinary)
    }
    pub(crate) fn admit_ordinary_with_context(
        source: OrdinaryJudgedExpr<'_>,
        sink: &TypeExpr,
        context: AdmissionContext,
        config: &super::super::ProviderContext,
    ) -> Result<ExpressionAdmission, InferenceError> {
        admit_ordinary_data(source.data, source.proof, sink, context, config)
    }
    mod static_admission;
    use static_admission::{OrdinarySourceMode, StaticChild, StaticSourceMode};
    fn admit_ordinary_data(
        expression: &JudgedExprData,
        no_conversion: NoConversion,
        sink: &TypeExpr,
        context: AdmissionContext,
        config: &super::super::ProviderContext,
    ) -> Result<ExpressionAdmission, InferenceError> {
        admit_source_data(
            expression,
            sink,
            context,
            config,
            &mut OrdinarySourceMode(no_conversion),
            None,
        )
    }
    fn admit_source_data<'d, M: StaticSourceMode<'d>>(
        expression: &JudgedExprData,
        sink: &TypeExpr,
        context: AdmissionContext,
        config: &super::super::ProviderContext,
        mode: &mut M,
        declaration: Option<&'d crate::schema::CompiledAttributeType>,
    ) -> Result<ExpressionAdmission, M::Error> {
        use ExpressionAdmission::{DeferredInterpolation, ProviderPending, ValueCheck};
        let checked_source = mode.enter(expression, sink, context, declaration, config)?;
        let Some(expression) = checked_source.data() else {
            return Ok(mode.value_check());
        };
        let expression = expression.for_sink(sink, config);
        let (ty, kind, concrete) = match &expression.judgement {
            ExprJudgement::Failed { error, .. } | ExprJudgement::Rejected { error, .. } => {
                return Err(error.clone().into());
            }
            ExprJudgement::AuthorityUnavailable(authority) => {
                return Ok(ProviderPending(authority.clone()));
            }
            ExprJudgement::Declaration => return Err(assignment_error(&expression, sink).into()),
            ExprJudgement::Typed { ty, kind, .. } => (
                Some(ty),
                Some(kind),
                kind.knowledge() == Classification::Known,
            ),
            ExprJudgement::ValueDependent { .. } => (None, None, false),
        };
        if !expression.alternatives.is_empty() {
            return expression
                .alternatives
                .iter()
                .try_fold(ValueCheck, |admission, branch| {
                    Ok(combine_admission(
                        admission,
                        admit_source_data(branch, sink, context, config, mode, declaration)?,
                    ))
                });
        }
        if expression.is_value_dependent() {
            return Ok(mode.value_check());
        }
        let sink = crate::validation::resolve_type_expr(sink, config)
            .map_err(|error| InferenceError::TypeResolution { error })?;
        let sink = &sink;
        let resolved_actual = ty
            .map(|ty| crate::validation::resolve_type_expr(ty, config))
            .transpose()
            .map_err(|error| InferenceError::TypeResolution { error })?;
        let ty = resolved_actual.as_ref();
        if context == AdmissionContext::Resource
            && matches!(kind, Some(InferredKind::Reference(_)))
            && expression.check_owner() == ExpressionCheckOwner::BindingIndex
        {
            // Retain reference evidence for resource attribute admission. This
            // expression pass has neither the destination descriptor nor the
            // binding authority to decide that assignment a second time.
            return Ok(mode.value_check());
        }
        if context == AdmissionContext::Function
            && (matches!(ty, Some(TypeExpr::Ref(_)))
                || (matches!(kind, Some(InferredKind::Reference(_)))
                    && ty == Some(&TypeExpr::String)
                    && sink.is_string_shaped()))
        {
            // FunctionArgument owns handle identity; schema strings keep concrete refinement checks.
            return Ok(mode.value_check());
        }
        if let TypeExpr::Union(members) = sink {
            if expression.requires_declared_assignment()
                && ty == Some(&TypeExpr::String)
                && !members.is_empty()
                && members
                    .iter()
                    .all(|member| matches!(member, TypeExpr::StringLiteral(_)))
            {
                return Ok(mode.value_check());
            }
            let mut failures = Vec::new();
            for (index, member) in members.iter().enumerate() {
                let mut probe = mode.probe();
                let child = probe.child(declaration, StaticChild::Union(index))?;
                match admit_source_data(&expression, member, context, config, &mut probe, child) {
                    Ok(admission) => {
                        mode.commit(probe);
                        return Ok(admission);
                    }
                    Err(error) => failures.push((probe, error)),
                }
            }
            return Err(mode.failed_union(failures, assignment_error(&expression, sink)));
        }
        if let Some(shape) = expression.collection_kind() {
            let child_context = if matches!(
                context,
                AdmissionContext::Assignment | AdmissionContext::StructValue
            ) && matches!(sink, TypeExpr::Struct { .. })
            {
                AdmissionContext::StructValue
            } else {
                context
            };
            let pairs: Vec<_> = match (shape, sink) {
                (CollectionKind::List, TypeExpr::List(inner)) => expression
                    .elements()
                    .map(|child| (child, inner.as_ref(), StaticChild::List))
                    .collect(),
                (CollectionKind::Map, TypeExpr::Map(inner)) => expression
                    .fields()
                    .map(|(_, child)| child)
                    .chain(expression.member.iter().map(Arc::as_ref))
                    .map(|child| (child, inner.as_ref(), StaticChild::Map))
                    .collect(),
                (CollectionKind::Map, TypeExpr::Struct { fields }) => fields
                    .iter()
                    .filter(|(name, _)| {
                        context != AdmissionContext::Resource || expression.field(name).is_ok()
                    })
                    .map(|(name, sink)| {
                        expression
                            .field(name)
                            .map(|child| (child, sink, StaticChild::Field(name)))
                            .map_err(|_| InferenceError::UnknownType {
                                reason: format!("expected struct, missing field '{name}'"),
                            })
                    })
                    .collect::<Result<_, _>>()?,
                (CollectionKind::KeyedGroup, _)
                | (CollectionKind::List | CollectionKind::Map, _) => {
                    return Err(assignment_error(&expression, sink).into());
                }
            };
            return pairs
                .into_iter()
                .try_fold(ValueCheck, |admission, (child, sink, route)| {
                    Ok(combine_admission(
                        admission,
                        admit_source_data(
                            child,
                            sink,
                            child_context,
                            config,
                            mode,
                            mode.child(declaration, route)?,
                        )?,
                    ))
                });
        }
        let actual = ty.ok_or_else(|| MissingEvidence::new("collection judgement"))?;
        if let (
            TypeExpr::Ref(actual),
            TypeExpr::Ref(expected) | TypeExpr::DottedUnresolved(expected),
        ) = (actual, sink)
        {
            return if actual == expected {
                Ok(mode.value_check())
            } else {
                Err(assignment_error(&expression, sink).into())
            };
        }
        if let Some(authority) = unavailable_type(actual) {
            return if matches!(
                ValueShape::of_type(sink),
                Some(ValueShape::String | ValueShape::Int | ValueShape::Float)
            ) || unavailable_type(sink).is_some()
            {
                Ok(ProviderPending(authority))
            } else {
                Err(assignment_error(&expression, sink).into())
            };
        }
        if expression.requires_declared_assignment() {
            if *actual == TypeExpr::String && !matches!(sink, TypeExpr::String) {
                return Err(assignment_error(&expression, sink).into());
            }
            return match crate::validation::typecheck::inferred_type_assignment(
                sink,
                actual,
                &config.custom_types,
            ) {
                crate::schema::TypeAssignability::Assignable => Ok(mode.value_check()),
                crate::schema::TypeAssignability::Mismatch => {
                    Err(assignment_error(&expression, sink).into())
                }
                crate::schema::TypeAssignability::Unchecked(evidence) => evidence
                    .into_types()
                    .find_map(|ty| pending_type(&ty))
                    .map(ProviderPending)
                    .ok_or_else(|| assignment_error(&expression, sink).into()),
            };
        }
        if context == AdmissionContext::StructValue && concrete {
            return Ok(mode.value_check());
        }
        if let Some(authority) = unavailable_type(sink) {
            return if matches!(
                ValueShape::of_type(actual),
                Some(ValueShape::String | ValueShape::Int | ValueShape::Float)
            ) {
                Ok(ProviderPending(authority))
            } else {
                Err(assignment_error(&expression, sink).into())
            };
        }
        if matches!(
            kind,
            Some(
                InferredKind::StringConstruction(Classification::Deferred)
                    | InferredKind::BuiltinConstruction(Classification::Deferred)
            )
        ) {
            return if sink.is_string_shaped() || matches!(sink, TypeExpr::Ref(_)) {
                Ok(DeferredInterpolation)
            } else if expression.diagnostic_scope.is_secret() {
                Err(InferenceError::SecretAssignmentMismatch {
                    types: crate::schema::MismatchTypeNames::for_type_exprs(
                        sink,
                        &TypeExpr::String,
                    ),
                }
                .into())
            } else {
                Err(InferenceError::UnknownType {
                    reason: format!(
                        "cannot assign deferred interpolation: expected {sink}, got String"
                    ),
                }
                .into())
            };
        }
        let shaped = match actual {
            TypeExpr::String | TypeExpr::StringLiteral(_) => {
                sink.is_string_shaped()
                    || matches!(sink, TypeExpr::StringLiteral(_) | TypeExpr::Ref(_))
            }
            TypeExpr::Int => matches!(
                ValueShape::of_type(sink),
                Some(ValueShape::Int | ValueShape::Float)
            ),
            TypeExpr::Float => ValueShape::of_type(sink) == Some(ValueShape::Float),
            TypeExpr::Bool => ValueShape::of_type(sink) == Some(ValueShape::Bool),
            TypeExpr::Duration => ValueShape::of_type(sink) == Some(ValueShape::Duration),
            TypeExpr::Ref(_)
            | TypeExpr::List(_)
            | TypeExpr::Map(_)
            | TypeExpr::Struct { .. }
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Simple(_)
            | TypeExpr::Union(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::UnloadedSchemaType(_) => false,
        };
        if shaped {
            Ok(mode.value_check())
        } else {
            Err(assignment_error(&expression, sink).into())
        }
    }

    fn pending_type(ty: &TypeExpr) -> Option<ProviderAuthorityUnavailable> {
        match ty {
            TypeExpr::List(inner) | TypeExpr::Map(inner) => pending_type(inner),
            TypeExpr::Struct { fields } => fields.iter().find_map(|(_, ty)| pending_type(ty)),
            TypeExpr::Union(members) => members.iter().find_map(pending_type),
            _ => unavailable_type(ty),
        }
    }

    fn unavailable_type(ty: &TypeExpr) -> Option<ProviderAuthorityUnavailable> {
        let identity = match ty {
            TypeExpr::UnloadedSchemaType(unloaded) => unloaded.identity().clone(),
            TypeExpr::DottedUnresolved(path) => {
                let mut parts = path.resource_type.split('.').collect::<Vec<_>>();
                let kind = parts.pop()?;
                crate::schema::TypeIdentity::new(Some(path.provider.as_str()), parts, kind)
            }
            _ => return None,
        };
        Some(ProviderAuthorityUnavailable(UnavailableSchema::Named {
            identity,
        }))
    }

    fn assignment_error(expression: &JudgedExprData, sink: &TypeExpr) -> InferenceError {
        InferenceError::UnknownType {
            reason: expression.static_type().map_or_else(
                |_| format!("expected {sink}, got collection"),
                |actual| {
                    // Expression diagnostics keep authored collection spelling;
                    // equal nominal names still expand their differing bounds.
                    if sink.to_string() == actual.to_string() {
                        crate::schema::MismatchTypeNames::for_type_exprs(sink, actual).to_string()
                    } else {
                        format!("expected {sink}, got {actual}")
                    }
                },
            ),
        }
    }

    /// One pass over the syntax inventory, shared by all resource snapshots.
    /// Preserve first-authored-node wins when several edges share an origin.
    #[derive(Default)]
    struct SecrecyIndex {
        locations: HashMap<ExpressionLocation, NodeId>,
        authored_locations: HashMap<DeclarationLocation, Vec<(ExpressionInstance, NodeId)>>,
        attributes: HashMap<(ExpressionInstance, NodeId), DeclarationLocation>,
        functions: HashMap<(ExpressionInstance, String), NodeId>,
        modules: HashMap<String, ExpressionInstance>,
    }

    impl SecrecyIndex {
        fn build<B: BindingAddress>(nodes: &AuthoredNodes<B>) -> Self {
            #[cfg(test)]
            SECRECY_INDEX_BUILDS.update(|builds| builds + 1);
            let mut index = Self::default();
            for ((instance, id), node) in nodes {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    index.locations.entry(node.location.clone())
                {
                    entry.insert(*id);
                    index
                        .authored_locations
                        .entry(node.location.authored.clone())
                        .or_default()
                        .push((instance.clone(), *id));
                }
                if let Some(value) = node.sink_value() {
                    index
                        .attributes
                        .entry((instance.clone(), value.node_id()))
                        .or_insert_with(|| node.location.authored.clone());
                }
                if let ExprKind::Function { name, .. } = &node.kind {
                    index
                        .functions
                        .entry((instance.clone(), name.clone()))
                        .or_insert(*id);
                }
                if let ExpressionInstance::Module(environment) = instance {
                    index
                        .modules
                        .entry(environment.scope.to_string())
                        .or_insert_with(|| instance.clone());
                }
            }
            index
        }
    }

    #[derive(Clone)]
    pub struct ExpressionReport<B: BindingAddress = RelativeBindingName> {
        bindings: Arc<IndexMap<B, JudgedExpr>>,
        expressions: Arc<IndexMap<ExpressionLocation, JudgedExpr>>,
        pub(super) nodes: Arc<HashMap<(ExpressionInstance, NodeId), JudgedExpr>>,
        pub(crate) placements: Arc<crate::ConversionChannel>,
        pub(super) syntax_nodes: Arc<AuthoredNodes<B>>,
        secrecy_index: Arc<OnceLock<SecrecyIndex>>,
        pub(super) use_roots: Arc<Vec<cursor::UseRoot<B>>>,
        pub(super) use_interner: Arc<cursor::UseInterner>,
        pub(super) definitions: Arc<IndexMap<B, ExprValue<ParserPlaceholder, B>>>,
        pub(super) projections:
            Arc<IndexMap<(ExpressionInstance, NodeId), Value<ParserPlaceholder, B>>>,
        findings: Arc<Vec<Located<InferenceError>>>,
        function_errors: Arc<Vec<Located<InferenceError>>>,
        operands: Arc<HashMap<ExpressionLocation, [JudgedExpr; 2]>>,
        module_attributes: Arc<IndexMap<(B, String), JudgedExpr>>,
        resource_reports: Arc<IndexMap<crate::resource::ResourceId, Arc<ExpressionReport<B>>>>,
        parameters: Arc<HashMap<String, JudgedExpr>>,
        secrecy_arguments: Arc<IndexMap<B, Value<ParserPlaceholder, B>>>,
        secrecy_argument_modules: Arc<IndexMap<B, B>>,
        secrecy_argument_roots: Arc<IndexMap<ExpressionLocation, String>>,
        secrecy_calls: Arc<IndexMap<ExpressionInstance, crate::secrecy::CallSite>>,
        pub(super) instance: ExpressionInstance,
        authored: bool,
    }

    // Node/use identities and projection caches are transient authority, not
    // part of the pre-existing syntax/report equality contract.
    impl<B: BindingAddress> PartialEq for ExpressionReport<B> {
        fn eq(&self, other: &Self) -> bool {
            self.bindings == other.bindings
                && self.expressions == other.expressions
                && self.findings == other.findings
                && self.function_errors == other.function_errors
                && self.operands == other.operands
                && self.module_attributes == other.module_attributes
                && self.resource_reports == other.resource_reports
                && self.parameters == other.parameters
                && self.instance == other.instance
                && self.authored == other.authored
        }
    }

    impl<B: BindingAddress> std::fmt::Debug for ExpressionReport<B> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ExpressionReport")
                .field("findings", &self.findings)
                .finish_non_exhaustive()
        }
    }

    impl<B: BindingAddress> ExpressionReport<B> {
        pub(super) fn node(
            &self,
            instance: &ExpressionInstance,
            id: NodeId,
        ) -> Option<&JudgedExpr> {
            self.nodes.get(&(instance.clone(), id))
        }
        pub(super) fn syntax_node(
            &self,
            instance: &ExpressionInstance,
            id: NodeId,
        ) -> Option<&Arc<AuthoredExpr<ParserPlaceholder, B>>> {
            self.syntax_nodes.get(&(instance.clone(), id))
        }
        pub(super) fn shared_for_resource(
            &self,
            id: &crate::resource::ResourceId,
        ) -> Result<Arc<Self>, MissingEvidence> {
            self.resource_reports
                .get(id)
                .cloned()
                .ok_or_else(|| MissingEvidence::for_resource(id))
        }
        pub(super) fn empty() -> Self {
            Self {
                bindings: Arc::default(),
                expressions: Arc::default(),
                nodes: Arc::default(),
                placements: Arc::default(),
                syntax_nodes: Arc::default(),
                secrecy_index: Arc::default(),
                use_roots: Arc::default(),
                use_interner: Arc::default(),
                definitions: Arc::default(),
                projections: Arc::default(),
                findings: Arc::default(),
                function_errors: Arc::default(),
                operands: Arc::default(),
                module_attributes: Arc::default(),
                resource_reports: Arc::default(),
                parameters: Arc::default(),
                secrecy_arguments: Arc::default(),
                secrecy_argument_modules: Arc::default(),
                secrecy_argument_roots: Arc::default(),
                secrecy_calls: Arc::default(),
                instance: ExpressionInstance::Root,
                authored: false,
            }
        }
        pub(super) fn extend(&mut self, other: Self) {
            self.secrecy_index = Arc::default();
            Arc::make_mut(&mut self.secrecy_argument_roots)
                .extend(Arc::unwrap_or_clone(other.secrecy_argument_roots));
            Arc::make_mut(&mut self.secrecy_argument_modules)
                .extend(Arc::unwrap_or_clone(other.secrecy_argument_modules));
            Arc::make_mut(&mut self.secrecy_arguments)
                .extend(Arc::unwrap_or_clone(other.secrecy_arguments));
            Arc::make_mut(&mut self.secrecy_calls)
                .extend(Arc::unwrap_or_clone(other.secrecy_calls));
            Arc::make_mut(&mut self.bindings).extend(Arc::unwrap_or_clone(other.bindings));
            Arc::make_mut(&mut self.expressions).extend(Arc::unwrap_or_clone(other.expressions));
            Arc::make_mut(&mut self.nodes).extend(Arc::unwrap_or_clone(other.nodes));
            Arc::make_mut(&mut self.placements).append(Arc::unwrap_or_clone(other.placements));
            Arc::make_mut(&mut self.syntax_nodes).extend(Arc::unwrap_or_clone(other.syntax_nodes));
            Arc::make_mut(&mut self.use_roots).extend(Arc::unwrap_or_clone(other.use_roots));
            Arc::make_mut(&mut self.definitions).extend(Arc::unwrap_or_clone(other.definitions));
            Arc::make_mut(&mut self.projections).extend(Arc::unwrap_or_clone(other.projections));
            Arc::make_mut(&mut self.findings).extend(Arc::unwrap_or_clone(other.findings));
            Arc::make_mut(&mut self.function_errors)
                .extend(Arc::unwrap_or_clone(other.function_errors));
            Arc::make_mut(&mut self.operands).extend(Arc::unwrap_or_clone(other.operands));
            Arc::make_mut(&mut self.module_attributes)
                .extend(Arc::unwrap_or_clone(other.module_attributes));
            Arc::make_mut(&mut self.resource_reports)
                .extend(Arc::unwrap_or_clone(other.resource_reports));
        }
        pub(super) fn map_bindings<C: BindingAddress>(
            &self,
            map: &mut impl FnMut(B) -> C,
        ) -> ExpressionReport<C> {
            let mut mapped = HashMap::new();
            let resource_reports = self
                .resource_reports
                .iter()
                .map(|(id, report)| {
                    let key = Arc::as_ptr(report) as usize;
                    let report = mapped
                        .entry(key)
                        .or_insert_with(|| Arc::new(report.map_bindings(map)))
                        .clone();
                    (id.clone(), report)
                })
                .collect::<IndexMap<_, _>>()
                .into();
            ExpressionReport {
                instance: self.instance.clone(),
                authored: self.authored,
                bindings: self
                    .bindings
                    .iter()
                    .map(|(name, expression)| (map(name.clone()), expression.clone()))
                    .collect::<IndexMap<_, _>>()
                    .into(),
                expressions: self.expressions.clone(),
                nodes: self.nodes.clone(),
                placements: self.placements.clone(),
                use_roots: {
                    let mut memo = HashMap::new();
                    Arc::new(
                        self.use_roots
                            .iter()
                            .map(|root| root.map_bindings(map, &mut memo))
                            .collect(),
                    )
                },
                use_interner: Arc::default(),
                syntax_nodes: {
                    let mut memo = HashMap::new();
                    Arc::new(
                        self.syntax_nodes
                            .iter()
                            .map(|(id, node)| {
                                (
                                    id.clone(),
                                    ExprValue {
                                        authored: node.clone(),
                                        payload: Projection::Pending,
                                    }
                                    .map_bindings(map, &mut memo)
                                    .authored,
                                )
                            })
                            .collect(),
                    )
                },
                definitions: {
                    let mut memo = HashMap::new();
                    Arc::new(
                        self.definitions
                            .iter()
                            .map(|(name, value)| {
                                (
                                    map(name.clone()),
                                    value.clone().map_bindings(map, &mut memo),
                                )
                            })
                            .collect(),
                    )
                },
                projections: Arc::new(
                    self.projections
                        .iter()
                        .map(|(id, value)| (id.clone(), value.clone().map_bindings(map)))
                        .collect(),
                ),
                findings: self.findings.clone(),
                function_errors: self.function_errors.clone(),
                operands: self.operands.clone(),
                module_attributes: self
                    .module_attributes
                    .iter()
                    .map(|((binding, attribute), expression)| {
                        (
                            (map(binding.clone()), attribute.clone()),
                            expression.clone(),
                        )
                    })
                    .collect::<IndexMap<_, _>>()
                    .into(),
                resource_reports,
                secrecy_index: self.secrecy_index.clone(),
                parameters: self.parameters.clone(),
                secrecy_arguments: Arc::new(
                    self.secrecy_arguments
                        .iter()
                        .map(|(name, value)| (map(name.clone()), value.clone().map_bindings(map)))
                        .collect(),
                ),
                secrecy_argument_modules: Arc::new(
                    self.secrecy_argument_modules
                        .iter()
                        .map(|(name, module)| (map(name.clone()), map(module.clone())))
                        .collect(),
                ),
                secrecy_argument_roots: self.secrecy_argument_roots.clone(),
                secrecy_calls: self.secrecy_calls.clone(),
            }
        }
        pub(crate) fn has_failed_expressions(&self) -> bool {
            self.expressions.values().any(JudgedExpr::failed)
        }
        pub(crate) fn secrecy_instance(&self) -> &ExpressionInstance {
            &self.instance
        }
        pub(crate) fn secrecy_condition_value(
            &self,
            expression: &ExprValue<ParserPlaceholder, B>,
        ) -> Option<bool> {
            let index = expression.secrecy_condition()?;
            let condition = expression.secrecy_children().get(index)?;
            self.secrecy_boolean(condition)
        }
        pub(crate) fn secrecy_boolean(
            &self,
            expression: &ExprValue<ParserPlaceholder, B>,
        ) -> Option<bool> {
            let value = self
                .projections
                .get(&(expression.secrecy_instance().clone(), expression.node_id()))?;
            let proof = value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .ok()?
                .into_classification()
                .into_known()
                .ok()?;
            crate::resource::knowledge::known_bool(&proof)
        }

        pub(crate) fn secrecy_string(
            &self,
            expression: &ExprValue<ParserPlaceholder, B>,
        ) -> Option<String> {
            let value = self
                .projections
                .get(&(expression.secrecy_instance().clone(), expression.node_id()))?;
            let proof = value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .ok()?
                .into_classification()
                .into_known()
                .ok()?;
            proof.visit(
                |view| match view {
                    crate::resource::knowledge::KnownView::String(value) => {
                        Some(value.expose_plaintext().to_owned())
                    }
                    _ => None,
                },
                |_| None,
            )
        }

        pub(crate) fn secrecy_non_list(
            &self,
            expression: &ExprValue<ParserPlaceholder, B>,
        ) -> bool {
            fn non_list(ty: &TypeExpr) -> bool {
                match ty {
                    TypeExpr::String
                    | TypeExpr::Bool
                    | TypeExpr::Int
                    | TypeExpr::Float
                    | TypeExpr::Duration
                    | TypeExpr::Simple(_)
                    | TypeExpr::SchemaType { .. }
                    | TypeExpr::Map(_)
                    | TypeExpr::Struct { .. }
                    | TypeExpr::StringLiteral(_) => true,
                    TypeExpr::Union(types) => types.iter().all(non_list),
                    TypeExpr::List(_)
                    | TypeExpr::Ref(_)
                    | TypeExpr::DottedUnresolved(_)
                    | TypeExpr::UnloadedSchemaType(_) => false,
                }
            }
            self.node(expression.secrecy_instance(), expression.node_id())
                .and_then(|node| node.static_type().ok())
                .is_some_and(non_list)
        }

        pub(crate) fn secrecy_definition(
            &self,
            binding: &B,
        ) -> Option<&ExprValue<ParserPlaceholder, B>> {
            self.definitions.get(binding)
        }
        pub(crate) fn secrecy_builtin_closure(
            &self,
            instance: &ExpressionInstance,
            name: &str,
        ) -> Option<(
            crate::builtins::Builtin,
            Vec<ExprValue<ParserPlaceholder, B>>,
        )> {
            builtin_closure(&self.definitions, instance, name)
        }
        pub(crate) fn secrecy_argument(&self, binding: &B) -> Option<&Value<ParserPlaceholder, B>> {
            self.secrecy_arguments.get(binding)
        }
        pub(crate) fn secrecy_argument_module(&self, binding: &B) -> Option<&B> {
            self.secrecy_argument_modules.get(binding)
        }
        pub(crate) fn secrecy_call(
            &self,
            instance: &ExpressionInstance,
        ) -> Option<&crate::secrecy::CallSite> {
            self.secrecy_calls.get(instance)
        }
        fn secrecy_index(&self) -> &SecrecyIndex {
            self.secrecy_index
                .get_or_init(|| SecrecyIndex::build(&self.syntax_nodes))
        }
        fn secrecy_at_in(
            &self,
            location: &ExpressionLocation,
        ) -> Option<ExprValue<ParserPlaceholder, B>> {
            let id = self.secrecy_index().locations.get(location)?;
            self.secrecy_node(&location.instance, *id)
        }
        pub(crate) fn secrecy_at(
            &self,
            location: &DeclarationLocation,
        ) -> Option<ExprValue<ParserPlaceholder, B>> {
            self.secrecy_at_in(&ExpressionLocation::new(self.instance.clone(), location))
        }
        pub(crate) fn secrecy_at_instances<'a>(
            &'a self,
            location: &'a DeclarationLocation,
        ) -> impl Iterator<Item = ExprValue<ParserPlaceholder, B>> + 'a {
            self.secrecy_index()
                .authored_locations
                .get(location)
                .into_iter()
                .flatten()
                .filter_map(move |(instance, id)| self.secrecy_node(instance, *id))
        }
        pub(crate) fn secrecy_literal(&self, expression: &ExprValue<ParserPlaceholder, B>) -> bool {
            self.node(expression.secrecy_instance(), expression.node_id())
                .is_some_and(|judged| {
                    matches!(
                        judged.judgement,
                        ExprJudgement::Typed {
                            kind: InferredKind::Literal | InferredKind::EnumIdentifier,
                            ..
                        }
                    )
                })
        }
        pub(crate) fn secrecy_is_structural(
            &self,
            source: &ExprValue<ParserPlaceholder, B>,
        ) -> bool {
            self.node(source.secrecy_instance(), source.node_id())
                .is_some_and(|expression| expression.structural_source)
        }
        pub(crate) fn secrecy_node(
            &self,
            instance: &ExpressionInstance,
            id: NodeId,
        ) -> Option<ExprValue<ParserPlaceholder, B>> {
            self.syntax_nodes
                .get(&(instance.clone(), id))
                .map(|authored| ExprValue {
                    authored: authored.clone(),
                    payload: Projection::Pending,
                })
        }
        pub(crate) fn secrecy_functions(
            &self,
        ) -> impl Iterator<Item = ExprValue<ParserPlaceholder, B>> + '_ {
            self.syntax_nodes
                .values()
                .filter(|authored| matches!(authored.kind, ExprKind::Function { .. }))
                .map(|authored| ExprValue {
                    authored: authored.clone(),
                    payload: Projection::Pending,
                })
        }
        pub(crate) fn secrecy_attribute_location(
            &self,
            id: &crate::resource::ResourceId,
            value: NodeId,
        ) -> Option<DeclarationLocation> {
            let instance = self
                .resource_reports
                .get(id)
                .map_or(&self.instance, |report| &report.instance);
            self.secrecy_index()
                .attributes
                .get(&(instance.clone(), value))
                .cloned()
        }
        pub(crate) fn secrecy_root(
            &self,
            id: &crate::resource::ResourceId,
            node: NodeId,
        ) -> Option<ExprValue<ParserPlaceholder, B>> {
            let instance = self
                .resource_reports
                .get(id)
                .map_or(&self.instance, |report| &report.instance);
            self.secrecy_node(instance, node)
        }
        pub(crate) fn secrecy_arguments_authored(
            &self,
        ) -> impl Iterator<Item = (String, ExprValue<ParserPlaceholder, B>)> + '_ {
            self.secrecy_argument_roots
                .iter()
                .filter_map(move |(location, name)| {
                    self.secrecy_at_in(location)
                        .map(|node| (name.clone(), node))
                })
        }
        pub(crate) fn secrecy_lexical_function(
            &self,
            module: Option<&B>,
            name: &str,
        ) -> Option<ExprValue<ParserPlaceholder, B>> {
            let instance = if let Some(module) = module {
                self.secrecy_index().modules.get(module.as_str())?.clone()
            } else {
                ExpressionInstance::Root
            };
            self.secrecy_function(&instance, name)
        }
        pub(crate) fn secrecy_resources(
            &self,
        ) -> impl Iterator<Item = ExprValue<ParserPlaceholder, B>> + '_ {
            self.syntax_nodes
                .values()
                .filter(|node| {
                    matches!(
                        node.kind,
                        ExprKind::Resource {
                            provider: _,
                            resource_type: _,
                            read: _
                        }
                    )
                })
                .map(|node| ExprValue {
                    authored: node.clone(),
                    payload: Projection::Pending,
                })
        }
        pub(crate) fn secrecy_conditions(
            &self,
        ) -> impl Iterator<Item = ExprValue<ParserPlaceholder, B>> + '_ {
            self.syntax_nodes
                .values()
                .filter(|node| matches!(node.kind, ExprKind::If))
                .filter_map(|node| node.condition().cloned())
        }
        pub(crate) fn secrecy_function(
            &self,
            instance: &ExpressionInstance,
            name: &str,
        ) -> Option<ExprValue<ParserPlaceholder, B>> {
            let id = self
                .secrecy_index()
                .functions
                .get(&(instance.clone(), name.to_string()))?;
            self.secrecy_node(instance, *id)
        }
        pub(crate) fn secrecy_output(
            &self,
            binding: &B,
            name: &str,
        ) -> Option<ExprValue<ParserPlaceholder, B>> {
            let origin = self
                .module_attributes
                .get(&(binding.clone(), name.to_string()))?
                .origin
                .as_ref()?;
            self.secrecy_at_in(origin)
        }
        pub(crate) fn has_authored_judgement(&self) -> bool {
            self.authored
        }
        pub(super) fn admit_sink(&mut self, location: &DeclarationLocation, sink: &TypeExpr) {
            let expression = match self.expression(location) {
                Ok(expression) => expression,
                Err(error) => {
                    Arc::make_mut(&mut self.findings)
                        .push(Located::new(location.clone(), error.into()));
                    return;
                }
            };
            let admission = admit_expression(expression, sink);
            if let Err(error) = admission {
                let error = match error {
                    SinkRejection::Ordinary(error) => error,
                    SinkRejection::Conversion(rejection) => {
                        Arc::make_mut(&mut self.placements).append(rejection.into_channel());
                        return;
                    }
                };
                let owner = expression.sink_owner(sink);
                let owner = if owner == FailureOwner::Expression {
                    Arc::make_mut(&mut self.findings)
                        .push(Located::new(location.clone(), error.clone()));
                    FailureOwner::Reported
                } else {
                    owner
                };
                if let Some(expression) = Arc::make_mut(&mut self.expressions)
                    .get_mut(&ExpressionLocation::new(self.instance.clone(), location))
                {
                    expression.reject(error, owner);
                }
            }
        }
        pub fn binding(&self, name: &B) -> Result<&JudgedExpr, MissingEvidence> {
            self.bindings
                .get(name)
                .ok_or_else(|| MissingEvidence::new(format!("binding {name}")))
        }
        #[cfg(test)]
        pub(crate) fn types_at_for_test(&self, location: &DeclarationLocation) -> Vec<TypeExpr> {
            self.expressions
                .iter()
                .filter(|(key, _)| &key.authored == location)
                .filter_map(|(_, expr)| expr.static_type().ok().cloned())
                .collect()
        }
        pub fn expression(
            &self,
            location: &DeclarationLocation,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            self.expression_in(&self.instance, location)
        }
        #[cfg(test)]
        pub(crate) fn instances_at(
            &self,
            location: &DeclarationLocation,
        ) -> Result<Vec<&JudgedExpr>, MissingEvidence> {
            let instances: Vec<_> = self
                .expressions
                .iter()
                .filter(|(key, _)| &key.authored == location)
                .map(|(_, expression)| expression)
                .collect();
            if instances.is_empty() {
                Err(MissingEvidence::new(format!(
                    "all instances at {location:?}"
                )))
            } else {
                Ok(instances)
            }
        }
        pub(crate) fn authored_for_iterables<'a>(
            &'a self,
            key_report: &'a Self,
        ) -> impl Iterator<Item = AuthoredForIterable<'a>> {
            self.syntax_nodes
                .iter()
                .filter_map(move |((instance, _), syntax)| {
                    let ExprKind::For {
                        binding,
                        bound,
                        iterable,
                    } = &syntax.kind
                    else {
                        return None;
                    };
                    let operand = syntax
                        .children
                        .first()
                        .expect("authored for has an iterable");
                    let location = operand.location();
                    let expression = self
                        .projections
                        .get(&(instance.clone(), operand.node_id()))
                        .map(crate::value::format_value)
                        .unwrap_or_else(|| iterable.as_str().trim().to_string());
                    let judgement = |report: &'a Self| {
                        report.node(instance, operand.node_id()).ok_or_else(|| {
                            MissingEvidence::new(format!("for iterable at {location:?}"))
                        })
                    };
                    Some(AuthoredForIterable {
                        binding,
                        bound: *bound,
                        location,
                        expression,
                        judged: judgement(self),
                        key_judged: judgement(key_report),
                    })
                })
        }
        pub(crate) fn expression_in(
            &self,
            instance: &ExpressionInstance,
            location: &DeclarationLocation,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            self.expressions
                .get(&ExpressionLocation::new(instance.clone(), location))
                .ok_or_else(|| MissingEvidence::new(format!("{instance:?} at {location:?}")))
        }
        pub fn for_resource(
            &self,
            id: &crate::resource::ResourceId,
        ) -> Result<&Self, MissingEvidence> {
            self.resource_reports
                .get(id)
                .map(Arc::as_ref)
                .ok_or_else(|| MissingEvidence::new(format!("resource {id}")))
        }
        pub fn module_attribute(
            &self,
            binding: &B,
            name: &str,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            self.module_attributes
                .get(&(binding.clone(), name.to_string()))
                .ok_or_else(|| MissingEvidence::new(format!("module {binding} attribute {name}")))
        }
        pub(crate) fn locations(&self) -> HashMap<DeclarationLocation, JudgedExpr> {
            self.expressions
                .iter()
                .filter(|(key, _)| key.instance == self.instance)
                .map(|(key, value)| (key.authored.clone(), value.clone()))
                .collect()
        }
        pub(crate) fn source_node(
            &self,
            instance: &ExpressionInstance,
            node: NodeId,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            self.node(instance, node)
                .ok_or_else(|| MissingEvidence::new("conversion binding source node"))
        }
        pub(crate) fn parameter(&self, name: &str) -> Result<&JudgedExpr, MissingEvidence> {
            self.parameters
                .get(name)
                .ok_or_else(|| MissingEvidence::new(format!("parameter {name}")))
        }
        pub(crate) fn record_instance<
            E,
            P: crate::resource::PlaceholderKind,
            T: crate::resource::PlaceholderKind,
            D,
        >(
            &mut self,
            file: &super::super::File<E, P, T, D, B>,
            binding: Option<&B>,
        ) where
            D: super::super::DeferredForPhaseTraversal<P, T, B>,
        {
            // Prototype IDs are local to the unexpanded module. Publish only
            // identities present in this contribution, retaining their scope.
            let ids: std::collections::HashSet<_> = file
                .iter_all_resources()
                .chain(file.deferred_for_phase.analysis_resource_refs())
                .map(|resource| resource.id().clone())
                .collect();
            Arc::make_mut(&mut self.resource_reports).retain(|id, _| ids.contains(id));
            if let Some(binding) = binding {
                for declaration in file.parser_binding_metadata.syntax().declarations() {
                    if declaration.detail().kind()
                        != crate::config_loader::DeclarationKind::ModuleAttribute
                    {
                        continue;
                    }
                    let evidence = declaration
                        .detail()
                        .value_location()
                        .ok_or_else(|| MissingEvidence::new("module attribute value location"))
                        .and_then(|location| self.expression(location))
                        .cloned();
                    match evidence {
                        Ok(mut expression) => {
                            let _resolved = file
                                .compositions
                                .iter()
                                .filter(|composition| composition.binding.as_ref() == Some(binding))
                                .filter_map(|composition| {
                                    composition
                                        .signature
                                        .attributes
                                        .get(declaration.detail().name())
                                })
                                .filter_map(|attribute| attribute.value())
                                .find_map(|value|value.knowledge(&crate::resource::knowledge::KnowledgeContext::validation()).map(|proof| proof.into_classification()).ok().and_then(|value|value.into_known().ok()).map(|known|expression.materialize_output(&known)));
                            Arc::make_mut(&mut self.module_attributes).insert(
                                (binding.clone(), declaration.detail().name().to_string()),
                                expression,
                            );
                        }
                        Err(error) => {
                            Arc::make_mut(&mut self.findings)
                                .push(Located::new(declaration.location().clone(), error.into()));
                        }
                    }
                }
            }
            self.record_resources(file);
        }
        pub(super) fn record_resources<
            E,
            P: crate::resource::PlaceholderKind,
            T: crate::resource::PlaceholderKind,
            D,
        >(
            &mut self,
            file: &super::super::File<E, P, T, D, B>,
        ) where
            D: super::super::DeferredForPhaseTraversal<P, T, B>,
        {
            let mut snapshots = HashMap::new();
            for resource in file
                .iter_all_resources()
                .chain(file.deferred_for_phase.analysis_resource_refs())
            {
                if self.resource_reports.contains_key(resource.id()) {
                    continue;
                }
                let source = match resource {
                    super::super::ResourceRef::Resource(resource) => {
                        resource.module_source.as_ref()
                    }
                    super::super::ResourceRef::DataSource(source) => source.module_source.as_ref(),
                    super::super::ResourceRef::Deferred { resource, .. } => {
                        resource.as_resource().module_source.as_ref()
                    }
                    super::super::ResourceRef::LiveComposition(_) => None,
                };
                let instance = source
                    .and_then(crate::resource::ModuleSource::identity_scope)
                    .map(|scope| ExpressionInstance::for_scope(&scope))
                    .unwrap_or_else(|| self.instance.clone());
                let snapshot = snapshots.entry(instance.clone()).or_insert_with(|| {
                    let mut snapshot = self.clone();
                    snapshot.instance = instance;
                    snapshot.resource_reports = Arc::default();
                    Arc::new(snapshot)
                });
                Arc::make_mut(&mut self.resource_reports)
                    .insert(resource.id().clone(), snapshot.clone());
            }
        }
        pub fn operand(
            &self,
            location: &DeclarationLocation,
            side: crate::integer_arithmetic::IntegerOperandSide,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            let index = match side {
                crate::integer_arithmetic::IntegerOperandSide::Left => 0,
                crate::integer_arithmetic::IntegerOperandSide::Right => 1,
            };
            self.operands
                .get(&ExpressionLocation::new(self.instance.clone(), location))
                .map(|operands| &operands[index])
                .ok_or_else(|| {
                    MissingEvidence::new(format!("integer operand {side:?} at {location:?}"))
                })
        }
        pub fn access<P: ValueKind>(
            &self,
            access: &crate::resource::AccessExpr<P, B>,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            let location = access
                .access_locations()
                .ok_or_else(|| MissingEvidence::new("access location"))?;
            self.expression(location.expression_location())
        }
        pub fn findings(&self) -> &[Located<InferenceError>] {
            &self.findings
        }
        pub(crate) fn first_function_error(&self) -> Option<super::super::ParseError> {
            first_error(&self.function_errors)
        }
        pub fn first_error(&self) -> Option<super::super::ParseError> {
            first_error(&self.findings)
        }
    }

    fn first_error(findings: &[Located<InferenceError>]) -> Option<super::super::ParseError> {
        findings.first().map(|finding| {
            super::super::ParseError::InvalidExpression {
                line: finding.location().span().start_line(),
                message: finding.detail().to_string(),
            }
            .at(finding.location())
        })
    }

    pub fn judge_expressions<
        E,
        P: crate::resource::PlaceholderKind,
        T: crate::resource::PlaceholderKind,
        D,
        B: BindingAddress,
    >(
        file: &super::super::File<E, P, T, D, B>,
        schemas: &crate::schema::SchemaRegistry,
        config: &super::super::ProviderContext,
    ) -> ExpressionReport<B>
    where
        D: super::super::DeferredForPhaseTraversal<P, T, B>,
    {
        judge_expressions_with_upstream(file, schemas, config, None)
    }

    pub fn judge_expressions_with_upstream<
        E,
        P: crate::resource::PlaceholderKind,
        T: crate::resource::PlaceholderKind,
        D,
        B: BindingAddress,
    >(
        file: &super::super::File<E, P, T, D, B>,
        schemas: &crate::schema::SchemaRegistry,
        config: &super::super::ProviderContext,
        upstream: Option<&crate::upstream_exports::UpstreamExports<B>>,
    ) -> ExpressionReport<B>
    where
        D: super::super::DeferredForPhaseTraversal<P, T, B>,
    {
        let expressions = file.parser_binding_metadata.syntax().expressions();
        let mut bindings = inference::bindings_from_parsed(file);
        for wait in &file.wait_bindings {
            if let Some(target) = bindings.get(wait.target.as_str()).cloned() {
                bindings.insert(wait.binding.to_string(), target);
            }
        }
        fn index_authored<'a, B: BindingAddress>(
            expression: &'a ExprValue<ParserPlaceholder, B>,
            authored: &mut IndexMap<ExpressionLocation, &'a ExprValue<ParserPlaceholder, B>>,
            visited: &mut std::collections::HashSet<usize>,
            pending: &mut Vec<&'a ExprValue<ParserPlaceholder, B>>,
        ) {
            pending.push(expression);
            while let Some(expression) = pending.pop() {
                if !visited.insert(Arc::as_ptr(&expression.authored) as usize) {
                    continue;
                }
                authored
                    .entry(expression.authored.location.clone())
                    .or_insert(expression);
                pending.extend(expression.authored.children.iter().rev());
            }
        }
        let mut authored = IndexMap::new();
        let mut visited = std::collections::HashSet::new();
        let mut pending = Vec::new();
        for expression in expressions
            .roots
            .iter()
            .chain(expressions.definitions.values())
        {
            index_authored(expression, &mut authored, &mut visited, &mut pending);
        }
        let mut functions = HashMap::new();
        for expression in authored.values() {
            if let ExprKind::Function {
                name, signature, ..
            } = &expression.authored.kind
            {
                // Match the parser seed rule when duplicate declarations survive analysis.
                functions
                    .entry((expression.authored.location.instance.clone(), name.clone()))
                    .or_insert_with(|| signature.clone());
            }
        }
        let authority = Arc::new(config.clone());
        let mut judge = Judge {
            authority: authority.clone(),
            authored,
            expressions,
            bindings: &bindings,
            functions,
            schemas,
            config,
            memo: HashMap::new(),
            work: Vec::new(),
            active: std::collections::HashSet::new(),
            cycle_seeds: std::collections::HashSet::new(),
            scheduling: false,
            demand: JudgementDemand::Value,
            scope: DiagnosticScope::Public,
            placement_owner: None,
            by_location: IndexMap::new(),
            by_node: HashMap::new(),
            syntax_nodes: IndexMap::new(),
            visiting: Vec::new(),
            findings: Vec::new(),
            placements: conversion::flow::PlacementFindings::new(
                expressions.template_placements.clone(),
            ),
            function_errors: Vec::new(),
            operands: HashMap::new(),
            module_attributes: Some(&expressions.report.module_attributes),
            upstream,
        };
        let mut judged_bindings = IndexMap::new();
        for (name, expression) in &expressions.definitions {
            judged_bindings.insert(name.clone(), judge.judge(expression));
        }
        for expression in &expressions.roots {
            judge.judge(expression);
        }
        let module_attributes = expressions
            .report
            .module_attributes
            .iter()
            .map(|(key, evidence)| {
                let refreshed = judge.refresh_module_output(evidence);
                (key.clone(), refreshed)
            })
            .collect();
        let mut report = ExpressionReport {
            instance: expressions.instance.clone(),
            authored: true,
            bindings: Arc::new(judged_bindings),
            expressions: Arc::new(judge.by_location),
            nodes: Arc::new(judge.by_node),
            placements: Arc::new(judge.placements.into_findings()),
            syntax_nodes: Arc::new(judge.syntax_nodes),
            secrecy_index: Arc::default(),
            use_roots: Arc::default(),
            use_interner: Arc::default(),
            definitions: Arc::new(expressions.definitions.clone()),
            projections: expressions.projections.clone(),
            findings: Arc::new(judge.findings),
            function_errors: Arc::new(judge.function_errors),
            operands: Arc::new(judge.operands),
            module_attributes: Arc::new(module_attributes),
            resource_reports: Arc::default(),
            secrecy_arguments: {
                let mut arguments = (*expressions.report.secrecy_arguments).clone();
                arguments.extend(expressions.secrecy_arguments.clone());
                Arc::new(arguments)
            },
            secrecy_argument_modules: {
                let mut modules = (*expressions.report.secrecy_argument_modules).clone();
                modules.extend(expressions.secrecy_argument_modules.clone());
                Arc::new(modules)
            },
            secrecy_argument_roots: {
                let mut roots = (*expressions.report.secrecy_argument_roots).clone();
                roots.extend(expressions.secrecy_argument_roots.clone());
                Arc::new(roots)
            },
            secrecy_calls: {
                let mut calls = (*expressions.report.secrecy_calls).clone();
                calls.extend(expressions.secrecy_calls.clone());
                Arc::new(calls)
            },
            parameters: file
                .arguments
                .iter()
                .map(|argument| {
                    (argument.name.clone(), {
                        let mut parameter = declared(Ok(argument.static_type.clone()), false);
                        parameter.data_mut().authority = Some(authority.clone());
                        JudgedExpr::build(
                            parameter.data,
                            conversion::flow::JudgementEdges::NonCrossed(
                                conversion::flow::NonCrossedEdge::ModuleArgument,
                            ),
                        )
                    })
                })
                .collect::<HashMap<_, _>>()
                .into(),
        };
        let instance = ExpansionInstanceId::fresh();
        let generation = ProjectionGeneration::fresh();
        let definitions: std::collections::HashSet<_> = expressions
            .definitions
            .values()
            .map(ExprValue::node_id)
            .collect();
        let roots = Arc::make_mut(&mut report.use_roots);
        roots.extend(expressions.roots.iter().map(|expression| cursor::UseRoot {
            node: expression.authored.clone(),
            occurrence: expression.authored.definition_use,
            instance,
            generation,
            definition: definitions.contains(&expression.node_id())
                || matches!(expression.authored.kind, ExprKind::Definition),
            environment: expression.authored.location.instance.clone(),
            conditions: ConditionSnapshot::default(),
        }));
        let mut resources = std::collections::HashSet::new();
        for resource in file
            .iter_all_resources()
            .chain(file.deferred_for_phase.analysis_resource_refs())
        {
            if !resources.insert(resource.id().clone()) {
                continue;
            }
            let (slots, source) = match resource {
                super::super::ResourceRef::Resource(resource) => {
                    (&resource.authored_slots, resource.module_source.as_ref())
                }
                super::super::ResourceRef::DataSource(source) => {
                    (&source.authored_slots, source.module_source.as_ref())
                }
                super::super::ResourceRef::Deferred { resource, .. } => (
                    &resource.as_resource().authored_slots,
                    resource.as_resource().module_source.as_ref(),
                ),
                super::super::ResourceRef::LiveComposition(_) => continue,
            };
            let environment = source
                .and_then(crate::resource::ModuleSource::identity_scope)
                .map(|scope| ExpressionInstance::for_scope(&scope))
                .unwrap_or_else(|| expressions.instance.clone());
            roots.extend(slots.use_roots().map(|mut root| {
                root.environment = environment.clone();
                root
            }));
        }
        let mut snapshots = HashMap::new();
        for (id, previous) in expressions.report.resource_reports.iter() {
            let snapshot = snapshots
                .entry(previous.instance.clone())
                .or_insert_with(|| {
                    let mut snapshot = report.clone();
                    snapshot.instance = previous.instance.clone();
                    snapshot.use_roots = Arc::default();
                    snapshot.resource_reports = Arc::default();
                    Arc::new(snapshot)
                });
            Arc::make_mut(&mut report.resource_reports).insert(id.clone(), snapshot.clone());
        }
        report.record_instance(file, None);
        report
    }

    /// Directory conditions and dynamic keys keep main's ordinary diagnostics.
    /// Their syntax supplies placement facts without publishing a second set
    /// of ordinary expression judgements.
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    enum JudgementDemand {
        Value,
        PlacementSyntax,
    }

    struct JudgeWork<B: BindingAddress> {
        node: Arc<AuthoredExpr<ParserPlaceholder, B>>,
        scope: DiagnosticScope,
        demand: JudgementDemand,
        owner: Option<Arc<ConversionSite>>,
        finish: bool,
    }

    struct Judge<'a, B: BindingAddress> {
        authority: Arc<super::super::ProviderContext>,
        authored: IndexMap<ExpressionLocation, &'a ExprValue<ParserPlaceholder, B>>,
        expressions: &'a Expressions<B>,
        bindings: &'a InferenceBindings,
        functions: HashMap<(ExpressionInstance, String), Arc<FunctionSignature>>,
        schemas: &'a crate::schema::SchemaRegistry,
        config: &'a super::super::ProviderContext,
        memo: HashMap<(usize, DiagnosticScope, JudgementDemand), JudgedExpr>,
        work: Vec<JudgeWork<B>>,
        active: std::collections::HashSet<usize>,
        cycle_seeds: std::collections::HashSet<(usize, DiagnosticScope, JudgementDemand)>,
        scheduling: bool,
        demand: JudgementDemand,
        scope: DiagnosticScope,
        placement_owner: Option<Arc<ConversionSite>>,
        by_location: IndexMap<ExpressionLocation, JudgedExpr>,
        by_node: HashMap<(ExpressionInstance, NodeId), JudgedExpr>,
        syntax_nodes: AuthoredNodes<B>,
        visiting: Vec<usize>,
        findings: Vec<Located<InferenceError>>,
        placements: conversion::flow::PlacementFindings,
        function_errors: Vec<Located<InferenceError>>,
        operands: HashMap<ExpressionLocation, [JudgedExpr; 2]>,
        module_attributes: Option<&'a IndexMap<(B, String), JudgedExpr>>,
        upstream: Option<&'a crate::upstream_exports::UpstreamExports<B>>,
    }

    impl<B: BindingAddress> Judge<'_, B> {
        fn is_list_literal(mut expression: &ExprValue<ParserPlaceholder, B>) -> bool {
            while matches!(
                expression.authored.kind,
                ExprKind::Alias | ExprKind::Parens | ExprKind::Definition
            ) && let [inner] = expression.authored.children.as_slice()
            {
                expression = inner;
            }
            matches!(expression.authored.kind, ExprKind::List)
        }

        fn placement_owner(
            root: &AuthoredExpr<ParserPlaceholder, B>,
            outer: &Option<Arc<ConversionSite>>,
        ) -> Option<Arc<ConversionSite>> {
            match &root.kind {
                ExprKind::Convert { .. } => outer.clone().or_else(|| root.first_conversion.clone()),
                ExprKind::Alias | ExprKind::Parens | ExprKind::Sequence => outer.clone(),
                ExprKind::Scalar(_)
                | ExprKind::Parameter(_)
                | ExprKind::LoopSlot(_)
                | ExprKind::UntypedParameter
                | ExprKind::Binding { .. }
                | ExprKind::Reference { .. }
                | ExprKind::List
                | ExprKind::Map(_)
                | ExprKind::NestedBlock { .. }
                | ExprKind::Interpolation { .. }
                | ExprKind::EmptyInterpolation
                | ExprKind::Coalesce
                | ExprKind::Secret
                | ExprKind::Call(_)
                | ExprKind::Function { .. }
                | ExprKind::Access(_)
                | ExprKind::Integer { .. }
                | ExprKind::Definition
                | ExprKind::Element
                | ExprKind::Iterable
                | ExprKind::Resource { .. }
                | ExprKind::Sink { .. }
                | ExprKind::If
                | ExprKind::For { .. }
                | ExprKind::Wait
                | ExprKind::Compose
                | ExprKind::DynamicAccess
                | ExprKind::ModuleCall
                | ExprKind::Boolean
                | ExprKind::Declaration
                | ExprKind::RequireCondition
                | ExprKind::ProviderDeclaration
                | ExprKind::Unsupported(_) => None,
            }
        }

        // Follow syntax and reference dependencies on a heap worklist. In
        // particular, sibling-file order must not turn a long reference chain
        // into one Rust stack frame per hop (#4474). The existing node judge
        // still owns inference, diagnostic order, scope and placement policy.
        fn judge(&mut self, expression: &ExprValue<ParserPlaceholder, B>) -> JudgedExpr {
            if self.scheduling {
                return self.judge_ready(expression);
            }
            if let Some(judged) = self.cached_judgement(&expression.authored) {
                return judged.clone();
            }
            let outer_scope = self.scope;
            let outer_demand = self.demand;
            let outer_owner = self.placement_owner.clone();
            let mut work = std::mem::take(&mut self.work);
            let mut active = std::mem::take(&mut self.active);
            let mut cycle_seeds = std::mem::take(&mut self.cycle_seeds);
            work.push(JudgeWork {
                node: expression.authored.clone(),
                scope: outer_scope,
                demand: outer_demand,
                owner: outer_owner.clone(),
                finish: false,
            });
            self.scheduling = true;
            while let Some(item) = work.pop() {
                let id = Arc::as_ptr(&item.node) as usize;
                let scope = if item.scope.is_secret() || matches!(item.node.kind, ExprKind::Secret)
                {
                    DiagnosticScope::Secret
                } else {
                    DiagnosticScope::Public
                };
                let key = (id, scope, item.demand);
                if item.finish {
                    active.remove(&id);
                    if cycle_seeds.remove(&key) {
                        self.memo.remove(&key);
                    }
                    self.scope = item.scope;
                    self.demand = item.demand;
                    self.placement_owner = item.owner;
                    self.judge_ready(&ExprValue {
                        authored: item.node,
                        payload: Projection::Pending,
                    });
                    continue;
                }
                if self.memo.contains_key(&key)
                    || (item.demand == JudgementDemand::PlacementSyntax
                        && self.memo.contains_key(&(id, scope, JudgementDemand::Value)))
                {
                    continue;
                }
                if item.node.children.is_empty()
                    && !matches!(
                        item.node.kind,
                        ExprKind::Binding { .. } | ExprKind::Reference { .. }
                    )
                {
                    self.scope = item.scope;
                    self.demand = item.demand;
                    self.placement_owner = item.owner;
                    self.judge_ready(&ExprValue {
                        authored: item.node,
                        payload: Projection::Pending,
                    });
                    continue;
                }
                if !active.insert(id) {
                    // Use the same binding-cycle verdict as the recursive
                    // judge. This seed exists only until its ancestor finishes.
                    self.visiting.push(id);
                    self.scope = item.scope;
                    self.demand = item.demand;
                    let failure = self.judge_ready(&ExprValue {
                        authored: item.node,
                        payload: Projection::Pending,
                    });
                    self.visiting.pop();
                    self.memo.insert(key, failure);
                    cycle_seeds.insert(key);
                    continue;
                }
                let child_owner = Self::placement_owner(&item.node, &item.owner);
                let child_demand = item.demand;
                let key_node = item.node.key_node();
                work.push(JudgeWork {
                    node: item.node.clone(),
                    scope: item.scope,
                    demand: item.demand,
                    owner: item.owner,
                    finish: true,
                });
                // Push dependencies directly in reverse execution order;
                // a temporary Vec for every parent adds no traversal state.
                if let ExprKind::Reference {
                    binding,
                    attribute,
                    authored_binding: _,
                    segments: _,
                } = &item.node.kind
                    && let Some(output) = self
                        .module_attributes
                        .and_then(|outputs| outputs.get(&(binding.clone(), attribute.clone())))
                    && let Some(origin) = output.origin.as_ref()
                    && let Some(authored) = self.authored.get(origin)
                {
                    work.push(JudgeWork {
                        node: authored.authored.clone(),
                        scope,
                        demand: child_demand,
                        owner: child_owner.clone(),
                        finish: false,
                    });
                }
                if let ExprKind::Reference {
                    binding,
                    authored_binding: _,
                    attribute: _,
                    segments: _,
                }
                | ExprKind::Binding {
                    binding,
                    authored_binding: _,
                } = &item.node.kind
                    && self.parameter_type(binding).is_none()
                    && let Some(definition) = self.expressions.definitions.get(binding)
                {
                    work.push(JudgeWork {
                        node: definition.authored.clone(),
                        scope,
                        demand: child_demand,
                        owner: child_owner.clone(),
                        finish: false,
                    });
                }
                for child in item.node.children.iter().rev() {
                    work.push(JudgeWork {
                        node: child.authored.clone(),
                        scope,
                        demand: if key_node == Some(child.node_id()) {
                            JudgementDemand::PlacementSyntax
                        } else {
                            child_demand
                        },
                        owner: child_owner.clone(),
                        finish: false,
                    });
                }
            }
            self.work = work;
            active.clear();
            cycle_seeds.clear();
            self.active = active;
            self.cycle_seeds = cycle_seeds;
            self.scheduling = false;
            self.scope = outer_scope;
            self.demand = outer_demand;
            self.placement_owner = outer_owner;
            self.judge_ready(expression)
        }

        fn cached_judgement(
            &self,
            root: &Arc<AuthoredExpr<ParserPlaceholder, B>>,
        ) -> Option<&JudgedExpr> {
            let id = Arc::as_ptr(root) as usize;
            let scope = if self.scope.is_secret() || matches!(root.kind, ExprKind::Secret) {
                DiagnosticScope::Secret
            } else {
                DiagnosticScope::Public
            };
            self.memo.get(&(id, scope, self.demand)).or_else(|| {
                if self.demand == JudgementDemand::PlacementSyntax {
                    self.memo.get(&(id, scope, JudgementDemand::Value))
                } else {
                    None
                }
            })
        }

        fn judge_ready(&mut self, expression: &ExprValue<ParserPlaceholder, B>) -> JudgedExpr {
            let root = &expression.authored;
            let id = Arc::as_ptr(root) as usize;
            let scope = if self.scope.is_secret() || matches!(root.kind, ExprKind::Secret) {
                DiagnosticScope::Secret
            } else {
                DiagnosticScope::Public
            };
            let demand = self.demand;
            if let Some(judged) = self.cached_judgement(root) {
                return judged.clone();
            }
            if self.visiting.contains(&id) {
                let mut failure = declared(
                    Err(InferenceError::CyclicVirtualBinding {
                        cycle: vec!["cyclic authored binding".into()],
                    }
                    .into()),
                    false,
                );
                failure.report_at(FailureOwner::BindingIndex);
                return JudgedExpr::build(
                    failure.data,
                    conversion::flow::JudgementEdges::MissingReference(
                        conversion::flow::MissingReferenceEdge::BindingCycle,
                    ),
                );
            }
            let outer_placement_owner = self.placement_owner.clone();
            self.placement_owner = Self::placement_owner(root, &outer_placement_owner);
            let outer_scope = self.scope;
            self.scope = scope;
            self.visiting.push(id);
            let key = root.key_node();
            let mut children = root
                .children
                .iter()
                .map(|child| {
                    self.demand = if key == Some(child.node_id()) {
                        JudgementDemand::PlacementSyntax
                    } else {
                        demand
                    };
                    self.judge(child)
                })
                .collect::<Vec<_>>();
            self.demand = demand;
            if matches!(&root.kind, ExprKind::Call(name) if name == "join"
                && !self.functions.contains_key(&(root.location.instance.clone(), name.clone())))
                && root.children.get(1).is_some_and(Self::is_list_literal)
                && let Some(list) = children.get_mut(1)
                && list.failed()
                && !list.elements.is_empty()
                && list.elements.iter().all(|element| {
                    !element.failed()
                        && matches!(
                            element.value_shape(),
                            Some(
                                ValueShape::String
                                    | ValueShape::Int
                                    | ValueShape::Float
                                    | ValueShape::Bool
                                    | ValueShape::Duration
                            )
                        )
                })
            {
                // join converts each scalar independently; this argument does
                // not require a common element type. Only its list-shape
                // failure is discharged: every element retains its evidence,
                // and other uses of this collection retain their rejection.
                let knowledge = list
                    .elements
                    .iter()
                    .all(|expression| expression.concrete())
                    .into();
                list.data_mut().judgement = ExprJudgement::ValueDependent {
                    reason: NotInferableReason::ValueDependent {
                        reason: "join accepts independently rendered scalar elements".into(),
                    },
                    shape: Some((CollectionKind::List, knowledge)),
                    owner: FailureOwner::Boundary,
                };
            }
            let reference = if let ExprKind::Reference {
                binding,
                authored_binding: _,
                attribute: _,
                segments: _,
            }
            | ExprKind::Binding {
                binding,
                authored_binding: _,
            } = &root.kind
            {
                if self.parameter_type(binding).is_some() {
                    None
                } else {
                    self.expressions
                        .definitions
                        .get(binding)
                        .map(|definition| self.judge(definition))
                }
            } else {
                None
            };
            let inherited = reference
                .as_ref()
                .map_or(conversion::flow::ConversionSources::empty(), |source| {
                    source.conversion_sources().clone()
                });
            let rejected = conversion::flow::check_placement(
                root,
                &children,
                &inherited,
                scope.is_secret(),
                self.placement_owner.as_deref(),
                &mut self.placements,
            );
            let result = if rejected {
                value_dependent("conversion placement is rejected")
            } else {
                self.infer(expression, &children)
            };
            use conversion::flow::{JudgementEdges as E, NonCrossedEdge as N};

            let edges = match &root.kind {
                ExprKind::Convert { target, .. } => E::Convert {
                    site: root
                        .first_conversion
                        .as_deref()
                        .expect("Convert owns its site"),
                    target: target.detail(),
                    operand: children.first().expect("Convert has its operand judgement"),
                    bare: root
                        .children
                        .first()
                        .is_some_and(|operand| operand.authored.is_bare_source()),
                },
                ExprKind::Resource { .. } => E::NonCrossed(N::SchemaAttributeReference),
                ExprKind::Function { .. } | ExprKind::UntypedParameter => {
                    E::NonCrossed(N::FunctionParameter)
                }
                ExprKind::Parameter(_) => E::NonCrossed(N::FunctionParameter),
                ExprKind::Binding {
                    binding,
                    authored_binding: _,
                } if self.parameter_type(binding).is_some() => E::NonCrossed(N::ModuleArgument),
                ExprKind::Binding { .. } if reference.is_none() => {
                    E::MissingReference(conversion::flow::MissingReferenceEdge::UndefinedBinding)
                }
                ExprKind::Reference {
                    binding,
                    authored_binding: _,
                    attribute: _,
                    segments: _,
                } if reference.is_none() => {
                    let cut = match self.bindings.get(binding.as_str()) {
                        Some(InferenceBinding::Argument { .. }) => N::ModuleArgument,
                        Some(InferenceBinding::UpstreamState) => N::UpstreamStateReference,
                        Some(InferenceBinding::ModuleCall | InferenceBinding::Virtual { .. }) => {
                            N::ModuleOutputReference
                        }
                        Some(InferenceBinding::Resource { .. }) | None => {
                            N::SchemaAttributeReference
                        }
                        Some(InferenceBinding::UserFunction) => N::FunctionParameter,
                    };
                    E::NonCrossed(cut)
                }
                ExprKind::Sequence => {
                    match root.result_index().and_then(|index| children.get(index)) {
                        Some(result) => E::Selected(result),
                        None => E::Leaf,
                    }
                }
                ExprKind::Scalar(_)
                | ExprKind::Binding { .. }
                | ExprKind::LoopSlot(_)
                | ExprKind::Reference { .. }
                | ExprKind::List
                | ExprKind::Map(_)
                | ExprKind::NestedBlock { .. }
                | ExprKind::Interpolation { .. }
                | ExprKind::EmptyInterpolation
                | ExprKind::Coalesce
                | ExprKind::Secret
                | ExprKind::Call(_)
                | ExprKind::Access(_)
                | ExprKind::Integer { .. }
                | ExprKind::Alias
                | ExprKind::Parens
                | ExprKind::Definition
                | ExprKind::Element
                | ExprKind::Iterable
                | ExprKind::Sink { .. }
                | ExprKind::If
                | ExprKind::For { .. }
                | ExprKind::Wait
                | ExprKind::Compose
                | ExprKind::DynamicAccess
                | ExprKind::ModuleCall
                | ExprKind::Boolean
                | ExprKind::Declaration
                | ExprKind::RequireCondition
                | ExprKind::ProviderDeclaration
                | ExprKind::Unsupported(_) => E::Follow {
                    inherited: &inherited,
                    children: &children,
                },
            };
            let mut result = JudgedExpr::build(result.data, edges);
            if matches!(
                root.kind,
                ExprKind::Resource { .. }
                    | ExprKind::For { .. }
                    | ExprKind::Wait
                    | ExprKind::ModuleCall
            ) {
                result.data_mut().structural_source = true;
            }
            result.data_mut().origin = Some(root.location.clone());
            result.data_mut().authority = Some(self.authority.clone());
            if matches!(root.kind, ExprKind::Reference { .. }) {
                if matches!(
                    result.judgement,
                    ExprJudgement::Typed {
                        kind: InferredKind::Ordinary(_),
                        ..
                    }
                ) {
                    result.set_kind(InferredKind::Reference);
                }
                // The binding pass owns reference existence and field errors.
                // A containing operation preserves that ownership.
                if !result.is_enum_identifier() {
                    result.report_at(FailureOwner::BindingIndex);
                }
            }
            if let Some(failed) = children
                .iter()
                .enumerate()
                .filter(|(index, _)| key != Some(root.children[*index].node_id()))
                .map(|(_, child)| child)
                .find(|child| child.failed())
            {
                let (error, owner) = failed.failure().expect("failed child");
                let owner = match owner {
                    FailureOwner::Reported | FailureOwner::BindingScope => owner,
                    _ => match result.judgement {
                        ExprJudgement::Failed { owner, .. }
                        | ExprJudgement::Rejected { owner, .. }
                        | ExprJudgement::Typed { owner, .. }
                        | ExprJudgement::ValueDependent { owner, .. } => owner,
                        ExprJudgement::Declaration => FailureOwner::Boundary,
                        ExprJudgement::AuthorityUnavailable(_) => FailureOwner::Expression,
                    },
                };
                result.mark_failed(error.clone(), owner);
                result.data_mut().boundary_owner = failed.boundary_owner.clone();
            }
            if let ExprJudgement::Failed {
                error,
                owner: FailureOwner::Expression,
            } = &result.judgement
            {
                let location = match error {
                    InferenceError::InvalidIntegerArithmeticOperand { operator_span, .. } => {
                        operator_span.location().unwrap_or(&root.location.authored)
                    }
                    _ if matches!(root.kind, ExprKind::Coalesce) => root.children[1].location(),
                    _ => &root.location.authored,
                }
                .clone();
                self.push_finding(location, error.clone());
                result.report_at(FailureOwner::Reported);
            }
            if scope.is_secret() {
                fn protect(judgement: &mut ExprJudgement) {
                    match judgement {
                        ExprJudgement::Failed { error, .. } => {
                            *error = error.clone().inside_secret()
                        }
                        ExprJudgement::Rejected { source, error, .. } => {
                            protect(source);
                            *error = error.clone().inside_secret();
                        }
                        _ => {}
                    }
                }
                let data = result.data_mut();
                data.diagnostic_scope = DiagnosticScope::Secret;
                protect(&mut data.judgement);
            }
            self.visiting.pop();
            self.scope = outer_scope;
            self.placement_owner = outer_placement_owner;
            self.memo.insert((id, scope, demand), result.clone());
            if demand == JudgementDemand::Value {
                self.by_location
                    .insert(root.location.clone(), result.clone());
                self.by_node
                    .insert((root.location.instance.clone(), root.id), result.clone());
                self.syntax_nodes
                    .insert((root.location.instance.clone(), root.id), root.clone());
            }
            result
        }

        fn refresh_module_output(&mut self, evidence: &JudgedExpr) -> JudgedExpr {
            let Some(origin) = evidence.origin.as_ref() else {
                return evidence.clone();
            };
            let mut output = match self.authored.get(origin).copied() {
                Some(authored) => self.judge(authored),
                None => JudgedExpr::build(
                    incomplete("authored module output origin").data,
                    conversion::flow::JudgementEdges::Selected(evidence),
                ),
            };
            output.inherit_output_materialization(evidence);
            output
        }
        fn check_function_sink(
            &mut self,
            expression: &JudgedExpr,
            sink: &TypeExpr,
            location: &DeclarationLocation,
            owner: &DeclarationLocation,
            context: &str,
        ) -> Option<InferredExpr> {
            let error = match crate::validation::typecheck::admit_function_expression(
                expression,
                sink,
                self.config,
            ) {
                Ok(_) => return None,
                Err(SinkRejection::Conversion(rejection)) => {
                    self.placements.append_rejection(rejection);
                    return Some(InferredExpr::from(expression));
                }
                Err(SinkRejection::Ordinary(error)) => error,
            };
            let error = match error {
                crate::validation::typecheck::FunctionExpressionError::Annotation(error) => {
                    let mut failed = declared(
                        Err(InferenceError::UnknownType {
                            reason: error.to_string(),
                        }
                        .into()),
                        expression.concrete(),
                    );
                    // Declaration resolution owns this error. It still makes
                    // the function/call judgement invalid.
                    failed.report_at(FailureOwner::Reported);
                    return Some(failed);
                }
                crate::validation::typecheck::FunctionExpressionError::Expression(error) => error,
            };
            if expression.failed() {
                return Some(InferredExpr::from(expression));
            }
            let finding = Located::new(
                location.clone(),
                InferenceError::UnknownType {
                    reason: format!("{context}: {error}"),
                },
            );
            let finding = if self.scope.is_secret() {
                let (location, error) = finding.into_parts();
                Located::new(location, error.inside_secret())
            } else {
                finding
            };
            if expression.for_sink(sink, self.config).kind()
                == Some(InferredKind::StringConstruction(Classification::Deferred))
                && self.demand == JudgementDemand::Value
                && !self.function_errors.contains(&finding)
            {
                self.function_errors.push(finding.clone());
            }
            self.push_finding(finding.location().clone(), finding.detail().clone());
            let Some(origin) = expression.origin.as_ref() else {
                return Some(incomplete("function boundary origin"));
            };
            let Some(owned) = self.by_location.get_mut(origin) else {
                return Some(incomplete("function boundary evidence"));
            };
            owned.report_at(FailureOwner::Reported);
            let owner = Arc::new(owner.clone());
            owned.data_mut().boundary_owner = Some(owner.clone());
            let mut failed = declared(Err(finding.detail().clone().into()), expression.concrete());
            failed.report_at(FailureOwner::Reported);
            failed.data_mut().boundary_owner = Some(owner);
            Some(failed)
        }

        fn push_finding(&mut self, location: DeclarationLocation, error: InferenceError) {
            if self.demand == JudgementDemand::PlacementSyntax {
                return;
            }
            let error = if self.scope.is_secret() {
                error.inside_secret()
            } else {
                error
            };
            if !self
                .findings
                .iter()
                .any(|finding| finding.location() == &location && finding.detail() == &error)
            {
                self.findings.push(Located::new(location, error));
            }
        }

        fn parameter_type(&self, binding: &B) -> Option<&TypeExpr> {
            self.expressions.arguments.get(binding).or_else(|| {
                match self.bindings.get(binding.as_str()) {
                    Some(InferenceBinding::Argument { type_expr }) => Some(type_expr),
                    _ => None,
                }
            })
        }

        fn narrow_declared(
            &self,
            ty: &TypeExpr,
            steps: &[PathSegment],
            binding: &str,
        ) -> Result<TypeExpr, InferenceIssue> {
            let mut current = ty.clone();
            for step in steps {
                if let TypeExpr::Ref(path) | TypeExpr::DottedUnresolved(path) = &current
                    && self
                        .schemas
                        .get_kindless(&path.provider, &path.resource_type)
                        .is_none()
                {
                    return Err(InferenceIssue::NotInferable(
                        NotInferableReason::SchemaUnavailable {
                            binding: binding.to_string(),
                        },
                    ));
                }
                current = crate::validation::narrow_type_expr(
                    &current,
                    std::slice::from_ref(step),
                    Some(self.schemas),
                )
                .ok_or_else(|| InferenceError::InvalidAccess {
                    base_type: current.to_string(),
                    step: step.clone(),
                })?;
            }
            Ok(current)
        }

        // Every reference and postfix form selects from judgement evidence.
        // Reconstructing from static_type() alone loses dynamic results and
        // the authored children of heterogeneous records.
        fn access(
            &self,
            base: &JudgedExprData,
            step: Option<&PathSegment>,
            binding: &str,
        ) -> InferredExpr {
            if base.is_value_dependent()
                || base.failed()
                || matches!(base.judgement, ExprJudgement::AuthorityUnavailable(_))
                || base.is_enum_identifier()
            {
                return InferredExpr {
                    data: Arc::new(base.clone()),
                };
            }
            let Some(step) = step else {
                if let Some(member) = &base.member {
                    return InferredExpr::from(member.as_ref());
                }
                let candidates: Vec<_> = base
                    .elements
                    .iter()
                    .chain(base.fields.values())
                    .cloned()
                    .collect();
                let Some(first) = candidates.first() else {
                    return incomplete("dynamic access element");
                };
                let ty = first.static_type();
                if ty.is_err()
                    || candidates
                        .iter()
                        .any(|candidate| candidate.static_type() != ty)
                {
                    return value_dependent("dynamic access depends on the selected element");
                }
                return alternatives(ty.cloned(), &candidates);
            };
            let shape = base.value_shape();
            let selected = match shape {
                Some(ValueShape::KeyedGroup) => match step {
                    PathSegment::Subscript {
                        index: crate::resource::Subscript::Str { .. },
                    } => base.member.as_deref(),
                    PathSegment::Field { .. }
                    | PathSegment::Subscript {
                        index: crate::resource::Subscript::Int { .. },
                    } => None,
                },
                Some(
                    ValueShape::String
                    | ValueShape::Int
                    | ValueShape::Float
                    | ValueShape::Bool
                    | ValueShape::Duration
                    | ValueShape::List
                    | ValueShape::Map
                    | ValueShape::Handle,
                )
                | None => match step {
                    PathSegment::Subscript {
                        index: crate::resource::Subscript::Int { index },
                    } => usize::try_from(*index).ok().and_then(|index| {
                        base.elements.get(index).or_else(|| {
                            base.member
                                .as_deref()
                                .filter(|_| base.value_shape() == Some(ValueShape::List))
                        })
                    }),
                    PathSegment::Subscript {
                        index: crate::resource::Subscript::Str { key },
                    } => base.fields.get(key.expose_plaintext()).or_else(|| {
                        base.member
                            .as_deref()
                            .filter(|_| base.value_shape() == Some(ValueShape::Map))
                    }),
                    PathSegment::Field { name } => base.fields.get(name).or_else(|| {
                        base.member
                            .as_deref()
                            .filter(|_| base.value_shape() == Some(ValueShape::Map))
                    }),
                },
            };
            if let Some(selected) = selected {
                return InferredExpr::from(selected);
            }
            let ty = match shape {
                Some(ValueShape::KeyedGroup) => Err(InferenceError::InvalidAccess {
                    base_type: "keyed loop group".into(),
                    step: step.clone(),
                }
                .into()),
                Some(
                    ValueShape::String
                    | ValueShape::Int
                    | ValueShape::Float
                    | ValueShape::Bool
                    | ValueShape::Duration
                    | ValueShape::List
                    | ValueShape::Map
                    | ValueShape::Handle,
                )
                | None => base
                    .static_type()
                    .cloned()
                    .and_then(|ty| self.narrow_declared(&ty, std::slice::from_ref(step), binding)),
            };
            let mut result = declared(ty, base.concrete());
            result.report_at(
                if base.check_owner() == ExpressionCheckOwner::BindingIndex {
                    FailureOwner::BindingIndex
                } else {
                    FailureOwner::Boundary
                },
            );
            if matches!(
                base.static_type(),
                Ok(TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_))
            ) {
                result.set_kind(InferredKind::Reference);
            }
            result
        }

        fn access_path(
            &self,
            base: InferredExpr,
            steps: &[PathSegment],
            binding: &str,
        ) -> InferredExpr {
            steps
                .iter()
                .fold(base, |base, step| self.access(&base, Some(step), binding))
        }

        fn infer(
            &mut self,
            expression: &ExprValue<ParserPlaceholder, B>,
            children: &[JudgedExpr],
        ) -> InferredExpr {
            let root = &expression.authored;
            let ordinary = declared;
            let child_type = |index: usize| {
                children.get(index).map_or_else(
                    || incomplete("operand").static_type().cloned(),
                    |child| child.static_type().cloned(),
                )
            };
            match &root.kind {
                ExprKind::Convert { .. } => authored_child(children.first()),
                ExprKind::Scalar(ty) => {
                    ordinary(Ok(ty.clone()), true).with_kind(|_| InferredKind::Literal)
                }
                ExprKind::Parameter(ty) => ordinary(Ok(ty.clone()), false).owned_by_bindings(),
                ExprKind::LoopSlot(ty) => ordinary(Ok(ty.clone()), false)
                    .with_kind(|_| InferredKind::LoopSlot)
                    .owned_by_bindings(),
                ExprKind::UntypedParameter => value_dependent("unannotated function parameter"),
                ExprKind::Binding {
                    binding: name,
                    authored_binding: _,
                } => {
                    if let Some(ty) = self.parameter_type(name) {
                        ordinary(Ok(ty.clone()), false).owned_by_bindings()
                    } else if let Some(definition) = self.expressions.definitions.get(name) {
                        InferredExpr::from(&self.judge(definition))
                    } else {
                        value_dependent("binding has no authored definition")
                    }
                }
                ExprKind::Reference {
                    binding,
                    attribute,
                    segments,
                    authored_binding: _,
                } => {
                    let steps = std::iter::once(PathSegment::Field {
                        name: attribute.clone(),
                    })
                    .chain(segments.iter().cloned())
                    .collect::<Vec<_>>();
                    if let Some(ty) = self.parameter_type(binding) {
                        return self.access_path(
                            ordinary(Ok(ty.clone()), false).owned_by_bindings(),
                            &steps,
                            binding.as_str(),
                        );
                    }
                    if let Some(attribute) = self.module_attributes.and_then(|attributes| {
                        attributes.get(&(binding.clone(), attribute.clone()))
                    }) {
                        let attribute = self.refresh_module_output(attribute);
                        return self.access_path(
                            InferredExpr::from(&attribute),
                            segments,
                            binding.as_str(),
                        );
                    }
                    if let Some(ty) = self
                        .upstream
                        .and_then(|exports| exports.get(binding))
                        .and_then(|fields| fields.get(attribute))
                        .and_then(|entry| entry.static_type.declared_known())
                    {
                        return self.access_path(
                            ordinary(Ok(ty.clone()), false).owned_by_bindings(),
                            segments,
                            binding.as_str(),
                        );
                    }
                    if matches!(
                        self.bindings.get(binding.as_str()),
                        Some(InferenceBinding::UpstreamState | InferenceBinding::ModuleCall)
                    ) {
                        let mut result = value_dependent(
                            "external value is checked at its declared or resolved boundary",
                        );
                        result.data_mut().judgement = ExprJudgement::ValueDependent {
                            reason: NotInferableReason::NonInferableBinding {
                                binding: binding.to_string(),
                            },
                            shape: None,
                            owner: FailureOwner::Expression,
                        };
                        return result;
                    }
                    if let Some(InferenceBinding::Virtual { attributes }) =
                        self.bindings.get(binding.as_str())
                        && !attributes.contains(attribute)
                    {
                        return ordinary(
                            Err(InferenceError::UnknownAttribute {
                                binding: binding.to_string(),
                                attribute: attribute.clone(),
                            }
                            .into()),
                            false,
                        );
                    }
                    if self.bindings.contains_key(binding.as_str()) {
                        ordinary(
                            inference::infer_declared_reference_type(
                                binding.as_str(),
                                attribute,
                                segments,
                                self.bindings,
                                self.schemas,
                            ),
                            false,
                        )
                    } else if let Some(definition) = self.expressions.definitions.get(binding) {
                        let base = self.judge(definition);
                        self.access_path(InferredExpr::from(&base), &steps, binding.as_str())
                    } else {
                        ordinary(Ok(TypeExpr::String), true)
                            .with_kind(|_| InferredKind::EnumIdentifier)
                    }
                }
                ExprKind::EmptyInterpolation => ordinary(Ok(TypeExpr::String), false),
                ExprKind::Interpolation {
                    single_reference: true,
                } if children[0].value_shape() == Some(ValueShape::String)
                    || children[0].failed() =>
                {
                    // Only a sole String-family reference preserves the child's
                    // kind, type, and diagnostic owner. Other scalars stringify.
                    InferredExpr::from(&children[0])
                }
                ExprKind::Interpolation { .. } => ordinary(
                    Ok(TypeExpr::String),
                    children.iter().all(|expression| expression.concrete()),
                )
                .with_kind(InferredKind::StringConstruction),
                ExprKind::Coalesce => {
                    let left = child_type(0);
                    let right = child_type(1);
                    let right_expression = &children[1];
                    let empty = match right_expression.value_shape() {
                        Some(ValueShape::List)
                            if right_expression.elements.is_empty()
                                && right_expression.member.is_none() =>
                        {
                            EmptyCoalesceDefault::List
                        }
                        Some(ValueShape::Map)
                            if right_expression.fields.is_empty()
                                && right_expression.member.is_none() =>
                        {
                            EmptyCoalesceDefault::Map
                        }
                        _ => EmptyCoalesceDefault::None,
                    };
                    let ty = match (&left, &right) {
                        (Ok(_), Ok(_)) => coalesce_type(left, right, empty),
                        _ if matches!((children[0].value_shape(), children[1].value_shape()), (Some(left), Some(right)) if left != right) => {
                            Err(InferenceError::UnknownType {
                                reason: format!(
                                    "coalesce default type `{}` does not match left type `{}`",
                                    children[1].type_description(),
                                    children[0].type_description()
                                ),
                            }
                            .into())
                        }
                        _ => coalesce_type(left, right, empty),
                    };
                    let mut result = alternatives(ty, children);
                    result.data_mut().coalesce =
                        Some(Arc::new([children[0].clone(), children[1].clone()]));
                    result
                }
                ExprKind::List | ExprKind::Map(_) => {
                    let mut shapes = children
                        .iter()
                        .filter_map(|child| child.value_shape().map(|shape| (child, shape)));
                    let incompatible = if matches!(root.kind, ExprKind::List)
                        && let Some((first, shape)) = shapes.next()
                    {
                        shapes.find_map(|(next, next_shape)| {
                            (shape != next_shape).then(|| {
                                match (first.static_type(), next.static_type()) {
                                    (Ok(first), Ok(next)) => {
                                        InferenceError::HeterogeneousCollection {
                                            types: crate::schema::MismatchTypeNames::for_type_exprs(
                                                first, next,
                                            ),
                                        }
                                    }
                                    _ => InferenceError::UnknownType {
                                        reason: format!(
                                            "collection element types disagree: `{}` vs `{}`",
                                            first.type_description(),
                                            next.type_description()
                                        ),
                                    },
                                }
                            })
                        })
                    } else {
                        None
                    };
                    let ty = if let Some(error) = &incompatible {
                        Err(error.clone().into())
                    } else {
                        match children.first() {
                            Some(first) => first.static_type().cloned().and_then(|first| {
                                for child in &children[1..] {
                                    let next = child.static_type()?;
                                    if *next != first {
                                        return Err(InferenceError::HeterogeneousCollection {
                                            types: crate::schema::MismatchTypeNames::for_type_exprs(
                                                &first, next,
                                            ),
                                        }
                                        .into());
                                    }
                                }
                                Ok(if matches!(root.kind, ExprKind::List) {
                                    TypeExpr::List(Box::new(first))
                                } else {
                                    TypeExpr::Map(Box::new(first))
                                })
                            }),
                            None => Err(InferenceIssue::NotInferable(
                                NotInferableReason::MissingValue,
                            )),
                        }
                    };
                    let shape = if matches!(root.kind, ExprKind::List) {
                        CollectionKind::List
                    } else {
                        CollectionKind::Map
                    };
                    let mut result = match ty {
                        Err(error @ InferenceIssue::Failed(_))
                            if incompatible.is_some()
                                || (shape == CollectionKind::List
                                    && matches!(
                                        error,
                                        InferenceIssue::Failed(
                                            InferenceError::HeterogeneousCollection { .. }
                                        )
                                    )) =>
                        {
                            let mut failed = declared(Err(error), false);
                            failed.report_at(FailureOwner::Boundary);
                            failed
                        }
                        ty => collection(
                            ty,
                            shape,
                            children.iter().all(|expression| expression.concrete()),
                        ),
                    };
                    result.data_mut().member = None;
                    match &root.kind {
                        ExprKind::List => result.data_mut().elements = Arc::new(children.to_vec()),
                        ExprKind::Map(keys) => {
                            result.data_mut().fields = Arc::new(
                                keys.iter().cloned().zip(children.iter().cloned()).collect(),
                            )
                        }
                        _ => unreachable!("collection judgement"),
                    }
                    result
                }
                ExprKind::Function { name, signature } => {
                    let AuthoredChildRoles::Function { body } = root.roles else {
                        return incomplete("named function body");
                    };
                    let Some((syntax, body)) = root
                        .children
                        .iter()
                        .zip(children)
                        .find(|(syntax, _)| syntax.node_id() == body)
                    else {
                        return incomplete("named function body judgement");
                    };
                    let body = body.clone();
                    if let Some(sink) = &signature.return_type
                        && let Some(failure) = self.check_function_sink(
                            &body,
                            sink,
                            syntax.location(),
                            syntax.location(),
                            &format!("function '{name}': return value"),
                        )
                    {
                        return failure;
                    }
                    declaration()
                }
                ExprKind::RequireCondition => authored_child(children.first()),
                ExprKind::Sequence => {
                    authored_child(root.result_index().and_then(|index| children.get(index)))
                }
                ExprKind::Secret => {
                    builtin_result(crate::builtins::Builtin::Secret.result_typing(), children)
                }
                ExprKind::Call(name) => {
                    let mut argument_failure = None;
                    let signature = self
                        .functions
                        .get(&(root.location.instance.clone(), name.clone()))
                        .cloned();
                    if let Some(signature) = &signature {
                        for (((parameter, parameter_type), child), authored) in signature
                            .parameters
                            .iter()
                            .zip(children)
                            .zip(&root.children)
                        {
                            if let Some(sink) = parameter_type
                                && let Some(failure) = self.check_function_sink(
                                    child,
                                    sink,
                                    authored.location(),
                                    &root.location.authored,
                                    &format!("function '{name}': parameter '{parameter}'"),
                                )
                            {
                                argument_failure.get_or_insert(failure);
                            }
                        }
                    }
                    if let Some(failure) = argument_failure {
                        return failure;
                    }
                    if let Some(ty) = signature
                        .as_ref()
                        .and_then(|signature| signature.return_type.as_ref())
                    {
                        return ordinary(Ok(ty.clone()), false);
                    }
                    match crate::builtins::Builtin::from_name(name) {
                        Some(builtin) => builtin_result(builtin.result_typing(), children),
                        None if signature.is_some() => {
                            value_dependent("unannotated function result")
                        }
                        None => incomplete("function declaration or builtin signature"),
                    }
                }
                ExprKind::Access(step) => self.access(&children[0], Some(step), "access base"),
                ExprKind::Integer { op, location } => {
                    let operands = [children[0].clone(), children[1].clone()];
                    if self.demand == JudgementDemand::Value {
                        self.operands.insert(
                            ExpressionLocation::new(root.location.instance.clone(), location),
                            operands.clone(),
                        );
                    }
                    ordinary(
                        inference::judge_integer_operands(
                            *op,
                            location,
                            &operands,
                            self.schemas,
                            self.config,
                        ),
                        children.iter().all(|child| child.concrete()),
                    )
                }
                ExprKind::Iterable => {
                    fn root_binding<B: BindingAddress>(
                        expression: &ExprValue<ParserPlaceholder, B>,
                    ) -> Option<&B> {
                        match &expression.authored.kind {
                            ExprKind::Binding {
                                binding,
                                authored_binding: _,
                            }
                            | ExprKind::Reference {
                                binding,
                                authored_binding: _,
                                attribute: _,
                                segments: _,
                            } => Some(binding),
                            ExprKind::Alias
                            | ExprKind::Parens
                            | ExprKind::Definition
                            | ExprKind::Access(_)
                            | ExprKind::DynamicAccess => {
                                expression.authored.children.first().and_then(root_binding)
                            }
                            _ => None,
                        }
                    }
                    if let Some(binding) = root.children.first().and_then(root_binding)
                        && !self.bindings.contains_key(binding.as_str())
                        && !self.expressions.definitions.contains_key(binding)
                        && !self.expressions.arguments.contains_key(binding)
                    {
                        let mut failure = ordinary(
                            Err(InferenceError::UnknownBinding {
                                binding: binding.to_string(),
                            }
                            .into()),
                            false,
                        );
                        failure.report_at(FailureOwner::BindingScope);
                        return failure;
                    }
                    children
                        .first()
                        .map(InferredExpr::from)
                        .unwrap_or_else(|| incomplete("iterable operand"))
                }
                ExprKind::Alias | ExprKind::Parens | ExprKind::Definition | ExprKind::Wait => {
                    authored_child(children.first())
                }
                ExprKind::Sink { .. } => authored_child(root.sink_value().and_then(|value| {
                    root.children
                        .iter()
                        .zip(children)
                        .find_map(|(syntax, judged)| {
                            (syntax.node_id() == value.node_id()).then_some(judged)
                        })
                })),
                ExprKind::Element => {
                    let collection = &children[0];
                    if collection.is_value_dependent() {
                        return value_dependent(
                            "external iterable elements require resolved value checks",
                        );
                    }
                    if let Some(member) = &collection.member {
                        return loop_element(InferredExpr::from(member.as_ref()));
                    }
                    let members = collection
                        .elements
                        .iter()
                        .chain(collection.fields.values())
                        .cloned()
                        .collect::<Vec<_>>();
                    if !members.is_empty() {
                        let mut element = InferredExpr::from(&members[0]);
                        element.data_mut().alternatives = Arc::new(members);
                        return loop_element(element);
                    }
                    match child_type(0) {
                        Ok(TypeExpr::List(inner) | TypeExpr::Map(inner)) => {
                            ordinary(Ok(*inner), false).with_kind(|_| InferredKind::LoopSlot)
                        }
                        Ok(actual) => {
                            let mut failure = ordinary(
                                Err(InferenceError::UnknownType {
                                    reason: format!(
                                        "for iterable must be a list or map, got {actual}"
                                    ),
                                }
                                .into()),
                                false,
                            );
                            failure.report_at(FailureOwner::BindingScope);
                            failure
                        }
                        Err(error) => ordinary(Err(error), false),
                    }
                }
                ExprKind::Resource {
                    provider,
                    resource_type,
                    read,
                } => {
                    let kind = if *read {
                        crate::schema::SchemaKind::DataSource
                    } else {
                        crate::schema::SchemaKind::Resource
                    };
                    let rejection = self
                        .schemas
                        .get(provider, resource_type, kind)
                        .and_then(|schema| self.admit_resource_children(&root.children, schema));
                    let mut result = ordinary(
                        Ok(TypeExpr::Ref(super::super::ResourceTypePath::new(
                            provider,
                            resource_type,
                        ))),
                        false,
                    );
                    result.report_at(FailureOwner::Boundary);
                    if let Some(failure) = rejection {
                        let (error, owner) = failure.failure().expect("rejected resource sink");
                        result.reject(error.clone(), owner);
                    }
                    result
                }
                ExprKind::If => {
                    let Some(condition) = root.condition_index() else {
                        return incomplete("named if condition");
                    };
                    let branches = children
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| *index != condition)
                        .map(|(_, child)| child.clone())
                        .collect::<Vec<_>>();
                    let mut ty = branches
                        .first()
                        .map(|branch| branch.static_type().cloned())
                        .unwrap_or_else(|| incomplete("if branch").static_type().cloned());
                    for branch in branches.iter().skip(1) {
                        match (&ty, branch.static_type()) {
                            (Ok(left), Ok(right)) if left != right => {
                                let mut members = match left {
                                    TypeExpr::Union(members) => members.clone(),
                                    left => vec![left.clone()],
                                };
                                if !members.contains(right) {
                                    members.push(right.clone());
                                }
                                ty = Ok(TypeExpr::Union(members));
                            }
                            (Err(_), Ok(_))
                                if branches[0].elements.is_empty()
                                    && matches!(
                                        branches[0].judgement,
                                        ExprJudgement::Typed {
                                            kind: InferredKind::List(_),
                                            ..
                                        } | ExprJudgement::ValueDependent {
                                            shape: Some((CollectionKind::List, _)),
                                            ..
                                        }
                                    ) =>
                            {
                                ty = branch.static_type().cloned()
                            }
                            _ => {}
                        }
                    }
                    let mut result = alternatives(ty, &branches);
                    if !branches.iter().any(|expression| expression.failed()) && result.failed() {
                        result.data_mut().judgement = ExprJudgement::ValueDependent {
                            reason: NotInferableReason::ValueDependent {
                                reason: result.static_type().unwrap_err().to_string(),
                            },
                            shape: None,
                            owner: FailureOwner::Boundary,
                        };
                    }
                    if let ExprJudgement::Typed {
                        kind: InferredKind::Ordinary(knowledge),
                        ..
                    } = &mut result.data_mut().judgement
                    {
                        *knowledge = children
                            .iter()
                            .all(|expression| expression.concrete())
                            .into();
                    }
                    result
                }
                ExprKind::For { binding, bound, .. } => {
                    if let Some(collection) = children.first()
                        && collection.value_shape().is_some_and(|shape| {
                            !matches!(shape, ValueShape::List | ValueShape::Map)
                        })
                    {
                        let mut failure = ordinary(
                            Err(InferenceError::UnknownType {
                                reason: format!(
                                    "for iterable must be a list or map, got {}",
                                    collection.type_description()
                                ),
                            }
                            .into()),
                            false,
                        );
                        // Keep the operand's own type for the precise iterable
                        // diagnostic; the loop operation is invalid.
                        failure.report_at(FailureOwner::BindingScope);
                        return failure;
                    }
                    let Some(body) = children.last() else {
                        return incomplete("for body");
                    };
                    let mut result = match (binding, *bound) {
                        (super::super::ForBinding::Indexed(_, _), _)
                        | (super::super::ForBinding::Simple(_), false) => collection(
                            body.static_type()
                                .cloned()
                                .map(|ty| TypeExpr::List(Box::new(ty))),
                            CollectionKind::List,
                            false,
                        ),
                        (super::super::ForBinding::Map(_, _), _)
                        | (super::super::ForBinding::Simple(_), true) => collection(
                            Err(InferenceIssue::NotInferable(
                                NotInferableReason::ValueDependent {
                                    reason: "keyed loop group is not a value".into(),
                                },
                            )),
                            CollectionKind::KeyedGroup,
                            false,
                        ),
                    };
                    result.data_mut().member = Some(Arc::new(body.clone()));
                    result
                }
                ExprKind::DynamicAccess => self.access(&children[0], None, "dynamic access base"),
                ExprKind::Boolean => {
                    let mut result = ordinary(
                        Ok(TypeExpr::Bool),
                        children.iter().all(|child| child.concrete()),
                    );
                    // Conditions retain their dedicated wait/validation checks.
                    result.report_at(FailureOwner::Boundary);
                    result
                }
                ExprKind::NestedBlock { .. } => InferredExpr::from(&children[0]),
                ExprKind::ModuleCall => {
                    let mut result = value_dependent("module call result requires its signature");
                    result.report_at(FailureOwner::Boundary);
                    result
                }
                ExprKind::Declaration | ExprKind::ProviderDeclaration | ExprKind::Compose => {
                    declaration()
                }
                ExprKind::Unsupported(rule) => {
                    incomplete(&format!("unsupported authored rule {rule}"))
                }
            }
        }

        fn admit_resource_children(
            &mut self,
            children: &[ExprValue<ParserPlaceholder, B>],
            schema: crate::schema::OwnerBoundResourceSchema<'_>,
        ) -> Option<JudgedExpr> {
            let mut failure = None;
            for child in children {
                match &child.authored.kind {
                    ExprKind::Sink { name, .. } => {
                        if let Some(attribute) = name.effective_attribute(schema.schema()) {
                            if attribute.is_read_only() {
                                continue;
                            }
                            let expected = crate::validation::expression_sink_type(
                                attribute.field_type(),
                                schema.owner(),
                                schema.schema().as_schema().defs(),
                            );
                            let value = child.authored.sink_value().expect("named sink value");
                            let mut judged = self.judge(value);
                            let admission = static_admission::admit_judged_resource_source(
                                &judged,
                                &expected,
                                attribute.field_type(),
                                schema.schema().as_schema().defs(),
                                self.config,
                                &mut self.placements,
                                value.authored.is_bare_source(),
                            );
                            let admission = match admission {
                                static_admission::ResourceSourceAdmission::Ordinary(result) => {
                                    result
                                }
                                static_admission::ResourceSourceAdmission::WalkOwned(result) => {
                                    result
                                }
                                static_admission::ResourceSourceAdmission::JudgedStatically(
                                    result,
                                ) => {
                                    judged.data_mut().static_source =
                                        Some(Arc::new(StaticSourceReceipt {
                                            expected: expected.clone(),
                                            result: result.clone(),
                                        }));
                                    self.by_location
                                        .insert(value.authored.location.clone(), judged.clone());
                                    self.by_node.insert(
                                        (value.authored.location.instance.clone(), value.node_id()),
                                        judged.clone(),
                                    );
                                    self.memo.insert(
                                        (
                                            Arc::as_ptr(&value.authored) as usize,
                                            self.scope,
                                            self.demand,
                                        ),
                                        judged.clone(),
                                    );
                                    match result {
                                        Ok(admission) => Ok(admission),
                                        Err(static_admission::JudgeSourceError::Ordinary(
                                            error,
                                        )) => Err(error),
                                        Err(static_admission::JudgeSourceError::Conversion) => {
                                            continue;
                                        }
                                    }
                                }
                            };
                            if let Err(error) = admission {
                                // Direct resource parameters retain the binding
                                // check's resource, argument, and source context.
                                // Folded branches and other boundaries still
                                // need the judge to report their rejection.
                                let sink_judged = judged.for_sink(&expected, self.config);
                                let owner = if sink_judged.requires_declared_assignment()
                                    && matches!(sink_judged.kind(), Some(InferredKind::Ordinary(_)))
                                    && judged.check_owner() == ExpressionCheckOwner::BindingIndex
                                {
                                    FailureOwner::BindingIndex
                                } else {
                                    judged.sink_owner(&expected)
                                };
                                if owner == FailureOwner::Expression {
                                    self.push_finding(value.location().clone(), error.clone());
                                }
                                let mut rejected = judged;
                                rejected.reject(error, owner);
                                self.by_location
                                    .insert(value.authored.location.clone(), rejected.clone());
                                self.memo.insert(
                                    (
                                        Arc::as_ptr(&value.authored) as usize,
                                        self.scope,
                                        self.demand,
                                    ),
                                    rejected.clone(),
                                );
                                failure.get_or_insert(rejected);
                            }
                        }
                    }
                    ExprKind::Resource { .. } => {}
                    _ => {
                        if let Some(rejected) =
                            self.admit_resource_children(&child.authored.children, schema)
                        {
                            failure.get_or_insert(rejected);
                        }
                    }
                }
            }
            failure
        }
    }

    fn loop_element(mut expression: InferredExpr) -> InferredExpr {
        if matches!(
            expression.judgement,
            ExprJudgement::Typed {
                kind: InferredKind::Ordinary(_) | InferredKind::Reference(_),
                ..
            }
        ) {
            expression.set_kind(|_| InferredKind::LoopSlot);
        }
        expression.data_mut().alternatives = Arc::new(
            expression
                .alternatives
                .iter()
                .map(|child| {
                    JudgedExpr::build(
                        loop_element(InferredExpr::from(child)).data,
                        conversion::flow::JudgementEdges::Selected(child),
                    )
                })
                .collect(),
        );
        expression
    }

    /// Join source evidence independently of a selected projection. Exact
    /// containers keep evidence for every index/key any branch can produce.
    fn alternatives(ty: Result<TypeExpr, InferenceIssue>, branches: &[JudgedExpr]) -> InferredExpr {
        if branches.len() == 1 {
            return InferredExpr::from(&branches[0]);
        }
        let mut result = declared(ty, branches.iter().all(|branch| branch.concrete()));
        result.data_mut().authority = branches.first().and_then(|branch| branch.authority.clone());
        result.data_mut().alternatives = Arc::new(branches.to_vec());
        let member_sources = branches
            .iter()
            .filter_map(|branch| branch.member.as_deref())
            .cloned()
            .collect::<Vec<_>>();
        result.data_mut().member = member_sources.first().map(|first| {
            Arc::new(alternatives_following_sources(
                first.static_type().cloned(),
                &member_sources,
            ))
        });
        let max_len = branches
            .iter()
            .map(|branch| branch.elements.len())
            .max()
            .unwrap_or(0);
        result.data_mut().elements = Arc::new(
            (0..max_len)
                .map(|index| {
                    let sources = branches
                        .iter()
                        .filter_map(|branch| {
                            branch.elements.get(index).or(branch.member.as_deref())
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    alternatives_following_sources(sources[0].static_type().cloned(), &sources)
                })
                .collect(),
        );
        let mut keys = IndexMap::new();
        for branch in branches {
            for key in branch.fields.keys() {
                keys.insert(key.clone(), ());
            }
        }
        result.data_mut().fields = Arc::new(
            keys.into_keys()
                .map(|key| {
                    let sources = branches
                        .iter()
                        .filter_map(|branch| branch.fields.get(&key).or(branch.member.as_deref()))
                        .cloned()
                        .collect::<Vec<_>>();
                    (
                        key,
                        alternatives_following_sources(sources[0].static_type().cloned(), &sources),
                    )
                })
                .collect(),
        );
        result
    }

    fn alternatives_following_sources(
        ty: Result<TypeExpr, InferenceIssue>,
        sources: &[JudgedExpr],
    ) -> JudgedExpr {
        JudgedExpr::build(
            alternatives(ty, sources).data,
            conversion::flow::JudgementEdges::Follow {
                inherited: &conversion::flow::ConversionSources::empty(),
                children: sources,
            },
        )
    }

    /// Interpret the registry declaration once for both calls and intrinsics.
    fn builtin_result(
        result_typing: crate::builtins::BuiltinResultTyping,
        children: &[JudgedExpr],
    ) -> InferredExpr {
        use crate::builtins::BuiltinResultTyping as R;
        let concrete = children.iter().all(|expression| expression.concrete());
        match result_typing {
            R::Argument(index) => children
                .get(index)
                .map(InferredExpr::from)
                .unwrap_or_else(|| incomplete("builtin argument")),
            R::String | R::SecretString => declared(Ok(TypeExpr::String), concrete),
            R::ConstructedString => declared(Ok(TypeExpr::String), concrete)
                .with_kind(InferredKind::BuiltinConstruction),
            R::Int => declared(Ok(TypeExpr::Int), concrete),
            R::StringList => declared(Ok(TypeExpr::List(Box::new(TypeExpr::String))), concrete),
            R::ConstructedStringList => {
                let mut result = collection(
                    Ok(TypeExpr::List(Box::new(TypeExpr::String))),
                    CollectionKind::List,
                    concrete,
                );
                result.data_mut().member = Some(Arc::new(JudgedExpr::build(
                    declared(Ok(TypeExpr::String), concrete)
                        .with_kind(InferredKind::BuiltinConstruction)
                        .data,
                    conversion::flow::JudgementEdges::Follow {
                        inherited: &conversion::flow::ConversionSources::empty(),
                        children,
                    },
                )));
                result.with_kind(InferredKind::BuiltinStringList)
            }
            R::List => builtin_collection(CollectionKind::List, concrete, children),
            R::Map => builtin_collection(CollectionKind::Map, concrete, children),
            R::Any => value_dependent("builtin return type depends on arguments"),
        }
    }

    fn builtin_collection(
        shape: CollectionKind,
        concrete: bool,
        children: &[JudgedExpr],
    ) -> InferredExpr {
        let mut result = collection(
            value_dependent("builtin return type depends on arguments")
                .static_type()
                .cloned(),
            shape,
            concrete,
        );
        result.data_mut().member = Some(Arc::new(JudgedExpr::build(
            value_dependent("builtin elements require resolved constraint checks").data,
            conversion::flow::JudgementEdges::Follow {
                inherited: &conversion::flow::ConversionSources::empty(),
                children,
            },
        )));
        result
    }

    fn collection(
        ty: Result<TypeExpr, InferenceIssue>,
        shape: CollectionKind,
        concrete: bool,
    ) -> InferredExpr {
        let mut result = match ty {
            Err(reason) => {
                let mut result = value_dependent("collection has no common element type");
                result.data_mut().judgement = ExprJudgement::ValueDependent {
                    reason: match reason {
                        InferenceIssue::NotInferable(reason) => reason,
                        InferenceIssue::Failed(error) => NotInferableReason::ValueDependent {
                            reason: error.to_string(),
                        },
                    },
                    shape: Some((shape, concrete.into())),
                    owner: FailureOwner::Boundary,
                };
                result
            }
            ty => declared(ty, concrete),
        };
        result.set_kind(|knowledge| match shape {
            CollectionKind::List => InferredKind::List(knowledge),
            CollectionKind::Map => InferredKind::Map(knowledge),
            CollectionKind::KeyedGroup => InferredKind::KeyedGroup(knowledge),
        });
        result.report_at(match shape {
            // Binding-scope validation owns structural group value errors.
            CollectionKind::KeyedGroup => FailureOwner::BindingScope,
            CollectionKind::List | CollectionKind::Map => FailureOwner::Boundary,
        });
        result
    }

    pub(super) fn synthetic_judgement(source: super::SyntheticSource) -> JudgedExpr {
        match source {
            super::SyntheticSource::ResourcePrefix(_) => JudgedExpr::build(
                declared(Ok(TypeExpr::String), true).data,
                conversion::flow::JudgementEdges::Leaf,
            ),
        }
    }
    fn declared(ty: Result<TypeExpr, InferenceIssue>, concrete: bool) -> InferredExpr {
        let judgement = match ty {
            Ok(ty) => ExprJudgement::Typed {
                ty,
                kind: InferredKind::Ordinary(concrete.into()),
                owner: FailureOwner::Expression,
            },
            Err(InferenceIssue::Failed(error)) => ExprJudgement::Failed {
                error,
                owner: FailureOwner::Expression,
            },
            Err(InferenceIssue::NotInferable(NotInferableReason::SchemaUnavailable {
                binding,
            })) => ExprJudgement::AuthorityUnavailable(ProviderAuthorityUnavailable(
                UnavailableSchema::Resource { binding },
            )),
            Err(InferenceIssue::NotInferable(
                NotInferableReason::CustomTypeSchemaUnavailable { identity },
            )) => ExprJudgement::AuthorityUnavailable(ProviderAuthorityUnavailable(
                UnavailableSchema::Named { identity },
            )),
            Err(InferenceIssue::NotInferable(
                reason @ (NotInferableReason::ValueDependent { .. }
                | NotInferableReason::NonInferableBinding { .. }),
            )) => ExprJudgement::ValueDependent {
                reason,
                shape: None,
                owner: FailureOwner::Boundary,
            },
            Err(InferenceIssue::NotInferable(_)) => ExprJudgement::Failed {
                error: MissingEvidence::new("incomplete authored judgement").into(),
                owner: FailureOwner::Boundary,
            },
        };
        InferredExpr {
            data: Arc::new(JudgedExprData {
                diagnostic_scope: DiagnosticScope::Public,
                origin: None,
                boundary_owner: None,
                judgement,
                authority: None,
                coalesce: None,
                alternatives: Arc::default(),
                member: None,
                elements: Arc::default(),
                fields: Arc::default(),
                converted: None,
                structural_source: false,
                static_source: None,
            }),
        }
    }
    pub(crate) fn unjudged() -> Result<JudgedExpr, MissingEvidence> {
        Err(MissingEvidence::new("declaration has not been judged"))
    }
    fn authored_child(child: Option<&JudgedExpr>) -> InferredExpr {
        child
            .map(InferredExpr::from)
            .unwrap_or_else(|| incomplete("authored child"))
    }

    fn incomplete(request: &str) -> InferredExpr {
        declared(
            Err(InferenceError::from(MissingEvidence::new(request)).into()),
            false,
        )
    }
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn constraint_fixture() -> JudgedExpr {
        let mut result = value_dependent("constraint-check fixture");
        result.data_mut().judgement = ExprJudgement::ValueDependent {
            reason: NotInferableReason::NonInferableBinding {
                binding: "constraint-check fixture".into(),
            },
            shape: None,
            owner: FailureOwner::Expression,
        };
        result.data_mut().authority = Some(Arc::new(super::super::ProviderContext::new(
            crate::EnvironmentSnapshot::empty(),
        )));
        JudgedExpr::build(result.data, conversion::flow::JudgementEdges::Leaf)
    }
    #[test]
    fn pending_secret_output_cannot_gain_materialized_value_evidence() {
        let config = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let raw: crate::resource::Value =
            crate::resource::Value::root_resource_ref("producer", "value", vec![]).into_secret();
        let proof = raw
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(matches!(
            &proof,
            crate::resource::knowledge::Classification::Pending(_)
        ));
        let mut expression =
            declared(Ok(TypeExpr::String), false).with_kind(InferredKind::BuiltinConstruction);
        if let Ok(known) = proof.into_known() {
            expression.materialize_output(&known);
        }
        let expression = JudgedExpr::build(expression.data, conversion::flow::JudgementEdges::Leaf);
        assert_eq!(
            admit_with_context(
                &expression,
                &TypeExpr::String,
                AdmissionContext::Assignment,
                &config
            ),
            Ok(ExpressionAdmission::DeferredInterpolation)
        );
    }

    #[test]
    fn refreshed_output_preserves_materialized_admission_without_a_value_substitute() {
        let config = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        for (known, expected) in [
            (false, ExpressionAdmission::DeferredInterpolation),
            (true, ExpressionAdmission::ValueCheck),
        ] {
            let previous =
                declared(Ok(TypeExpr::String), known).with_kind(InferredKind::BuiltinConstruction);
            let mut refreshed =
                declared(Ok(TypeExpr::String), false).with_kind(InferredKind::BuiltinConstruction);
            let previous = JudgedExpr::build(previous.data, conversion::flow::JudgementEdges::Leaf);
            refreshed.inherit_output_materialization(&previous);
            let refreshed =
                JudgedExpr::build(refreshed.data, conversion::flow::JudgementEdges::Leaf);
            assert_eq!(
                admit_with_context(
                    &refreshed,
                    &TypeExpr::String,
                    AdmissionContext::Assignment,
                    &config,
                ),
                Ok(expected),
            );
        }
    }

    #[test]
    fn value_dependent_admission_requires_authority() {
        let mut expression = constraint_fixture();
        expression.data_mut().authority = None;
        assert!(
            crate::test_utils::ordinary_sink_result_for_test(admit_expression(
                &expression,
                &TypeExpr::String
            ))
            .unwrap_err()
            .to_string()
            .contains("expression judgement authority")
        );
    }

    fn value_dependent(reason: &str) -> InferredExpr {
        let mut result = declared(Ok(TypeExpr::String), false);
        result.data_mut().judgement = ExprJudgement::ValueDependent {
            reason: NotInferableReason::ValueDependent {
                reason: reason.into(),
            },
            shape: None,
            owner: FailureOwner::Expression,
        };
        result
    }
    fn declaration() -> InferredExpr {
        let mut result = value_dependent("declaration is not a value");
        result.data_mut().judgement = ExprJudgement::Declaration;
        result
    }
}

pub(crate) use judgement::StaticSourceReceipt;
