//! Authored expressions and their provisional projections live on separate axes.
//!
//! The syntax tree is captured before evaluation, including operands that an
//! enclosing operation does not evaluate. Only the judge can mint admission
//! evidence. A projection is useful for identities and expansion, but cannot
//! be turned back into an authored expression.

use super::{DeclarationLocation, Located, PreprocessedFile, Rule, TypeExpr};
use crate::resource::DiagnosticScope;
use crate::resource::InsideSecret;
use crate::resource::{
    BindingAddress, ParserPlaceholder, PathSegment, RelativeBindingName, Value, ValueKind,
};
use indexmap::IndexMap;
use pest::iterators::{Pair, Pairs};
use std::{collections::HashMap, path::Path, sync::Arc};

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static PROJECTION_COPY_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        }
    }
    PROJECTION_COPY_WORK.update(|work| work + size(value));
}

mod cursor;
pub use cursor::{
    AuthoredCursor, AuthoredSlots, AuthoredUse, CompletenessReport, EdgeRole, ExpansionInstanceId,
    JudgedNodeRef, JudgedSlot, NoAuthoredEvidence, NodeId, ProjectedPhase, ProjectionGeneration,
    SlotDeclarationId, UnreachedSlot, UseId,
};
pub(crate) use cursor::{
    AuthoredForm, AuthoredNavigation, CursorKind, SourceReference, SyntheticSource,
};

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
pub(super) struct ConditionSnapshot {
    history: Arc<std::sync::RwLock<ConditionHistory>>,
    version: usize,
    previous: Option<Arc<ConditionSnapshot>>,
}

impl ConditionSnapshot {
    fn get(&self, id: NodeId) -> Option<bool> {
        let selected = {
            let history = self.history.read().expect("retained conditions lock");
            history.values.get(&id).and_then(|values| {
                let index = values.partition_point(|(version, _)| *version <= self.version);
                index.checked_sub(1).map(|index| values[index].1)
            })
        };
        selected.or_else(|| self.previous.as_ref()?.get(id))
    }

    fn over(&self, previous: &Self) -> Self {
        let mut snapshot = self.clone();
        snapshot.previous = Some(Arc::new(previous.clone()));
        snapshot
    }
}

#[derive(Debug, Default)]
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
        ConditionSnapshot {
            history: self.0.clone(),
            version: self.0.read().expect("retained conditions lock").version,
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
    children: Vec<ExprValue<P, B>>,
}

type AuthoredNodes<B> =
    HashMap<(ExpressionInstance, NodeId), Arc<AuthoredExpr<ParserPlaceholder, B>>>;

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

/// The declaration environment is selected at source/directory admission,
/// retained with every require node, and reused by all resolver entry points.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum RequireScope {
    /// An isolated buffer cannot establish that sibling arguments are absent.
    #[default]
    Unknown,
    /// Bootstrap/backend parsing retains complete declarations without
    /// entering guard validation. Only the directory loader mints this proof.
    UnvalidatedDirectory(crate::config_loader::CompleteDirectoryDeclarations),
    Directory(crate::config_loader::CompleteDirectoryDeclarations),
    ModuleTemplate {
        arguments: Arc<[String]>,
    },
    ModuleCall,
}

impl RequireScope {
    pub(crate) fn for_declarations(
        arguments: impl Iterator<Item = String>,
        blocks: super::ModuleDeclarationBlocks,
    ) -> Self {
        let arguments = arguments.collect::<Vec<_>>();
        if blocks == super::ModuleDeclarationBlocks::Absent {
            Self::Unknown
        } else {
            Self::ModuleTemplate {
                arguments: arguments.into(),
            }
        }
    }
}

/// Parser-minted declaration identity and its immutable authored obligation.
/// The exact value that suppresses ordinary value inference evaluates the
/// condition; a public File.requires edit cannot remove that obligation.
#[derive(Debug, Clone)]
struct RetainedRequireCondition {
    id: NodeId,
    scope: RequireScope,
    declaration: super::RequireBlock<ParserPlaceholder, RelativeBindingName>,
}

impl PartialEq for RetainedRequireCondition {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.scope == other.scope
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ExprKind<B: BindingAddress> {
    Scalar(TypeExpr),
    RequireCondition(Arc<RetainedRequireCondition>),
    Parameter(TypeExpr),
    LoopSlot(TypeExpr),
    UntypedParameter,
    Binding(B),
    Reference {
        binding: B,
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
    Sink(super::AttributeKey),
    If,
    For,
    Wait,
    Compose,
    DynamicAccess,
    ModuleCall,
    Boolean,
    Declaration,
    ProviderDeclaration,
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
struct FunctionSignature {
    parameters: Vec<(String, Option<TypeExpr>)>,
    return_type: Option<TypeExpr>,
}

impl<P: ValueKind, B: BindingAddress> ExprValue<P, B> {
    pub fn node_id(&self) -> NodeId {
        self.authored.id
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
        let authored = Arc::unwrap_or_clone(self.authored);
        let kind = match authored.kind {
            ExprKind::Binding(binding) => ExprKind::Binding(map(binding)),
            ExprKind::Reference {
                binding,
                attribute,
                segments,
            } => ExprKind::Reference {
                binding: map(binding),
                attribute,
                segments,
            },
            ExprKind::Scalar(ty) => ExprKind::Scalar(ty),
            ExprKind::RequireCondition(declaration) => ExprKind::RequireCondition(declaration),
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
            ExprKind::Sink(name) => ExprKind::Sink(name),
            ExprKind::If => ExprKind::If,
            ExprKind::For => ExprKind::For,
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
            location: authored.location,
            kind,
            children: authored
                .children
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
    let value = reduce()?;
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
    let payload = reduce()?;
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
    let instance = ExpansionInstanceId::fresh();
    let mut slots = AuthoredSlots::default();
    slots.set_declaration(
        captures
            .resource_declarations
            .get(&root.node_id())
            .ok_or_else(|| MissingEvidence::new("resource storage declaration"))?
            .clone(),
    );
    for child in &root.authored.children {
        let ExprKind::Sink(name) = &child.authored.kind else {
            continue;
        };
        if let Some(sink) = name.lookup_hash(sinks) {
            let conditions = captures.condition_snapshot();
            slots.insert(
                name.clone(),
                sink.expression.clone(),
                kind,
                instance,
                conditions,
            );
        } else {
            // Grouped blocks have explicit compiler-owned container handles,
            // including when a pending child prevents a provisional Value.
            let mut expression = child.authored.children[0].clone();
            if let Some(value) = values.get(name) {
                expression.payload = Projection::Value(value.clone());
            }
            slots.insert(
                name.clone(),
                expression,
                kind,
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
    report: Arc<ExpressionReport<B>>,
    arguments: HashMap<B, TypeExpr>,
    instance: ExpressionInstance,
    require_scope: Option<RequireScope>,
    captures: Arc<CapturedNodes>,
    projections: Arc<HashMap<(ExpressionInstance, NodeId), Value<ParserPlaceholder, B>>>,
}

impl<B: BindingAddress> PartialEq for Expressions<B> {
    fn eq(&self, other: &Self) -> bool {
        self.roots == other.roots
            && self.definitions == other.definitions
            && self.report == other.report
            && self.arguments == other.arguments
            && self.instance == other.instance
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
            report: Arc::new(ExpressionReport::empty()),
            arguments: HashMap::new(),
            instance: ExpressionInstance::Root,
            require_scope: None,
            captures: Arc::default(),
            projections: Arc::default(),
        }
    }
}

fn retain_require_nodes<P: ValueKind, B: BindingAddress>(
    expression: &mut ExprValue<P, B>,
    memo: &mut HashMap<usize, Arc<AuthoredExpr<P, B>>>,
    transform: &mut impl FnMut(
        NodeId,
        Option<RetainedRequireCondition>,
    ) -> Option<RetainedRequireCondition>,
) {
    let address = Arc::as_ptr(&expression.authored) as usize;
    if let Some(node) = memo.get(&address) {
        expression.authored = node.clone();
        return;
    }
    let mut node = (*expression.authored).clone();
    let current = match &node.kind {
        ExprKind::RequireCondition(require) => Some((**require).clone()),
        _ => None,
    };
    if let Some(require) = transform(node.id, current) {
        node.kind = ExprKind::RequireCondition(Arc::new(require));
    }
    for child in &mut node.children {
        retain_require_nodes(child, memo, transform);
    }
    let node = Arc::new(node);
    memo.insert(address, node.clone());
    expression.authored = node;
}

impl<B: BindingAddress> Expressions<B> {
    pub(in crate::parser) fn set_require_scope(&mut self, scope: RequireScope) {
        self.require_scope = Some(scope.clone());
        let mut memo = HashMap::new();
        let mut transform = |_: NodeId, current: Option<RetainedRequireCondition>| {
            current.map(|mut require| {
                require.scope = scope.clone();
                require
            })
        };
        for root in &mut self.roots {
            retain_require_nodes(root, &mut memo, &mut transform);
        }
        for root in self.definitions.values_mut() {
            retain_require_nodes(root, &mut memo, &mut transform);
        }
        self.report = Arc::new(ExpressionReport::empty());
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

    pub(super) fn extend(&mut self, other: Self) {
        self.roots.extend(other.roots);
        self.definitions.extend(other.definitions);
        self.arguments.extend(other.arguments);
        if self.require_scope.is_none() {
            self.require_scope = other.require_scope;
        }
        Arc::make_mut(&mut self.projections).extend(Arc::unwrap_or_clone(other.projections));
        Arc::make_mut(&mut self.report).extend(Arc::unwrap_or_clone(other.report));
    }

    pub(super) fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
    ) -> Expressions<C> {
        let mut memo = HashMap::new();
        Expressions {
            captures: Arc::default(),
            projections: Arc::new(
                self.projections
                    .iter()
                    .map(|(id, value)| (id.clone(), value.clone().map_bindings(map)))
                    .collect(),
            ),
            instance: self.instance,
            require_scope: self.require_scope,
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

    pub(crate) fn bind_arguments(
        &mut self,
        scope: &crate::resource::ModuleInstanceScope,
        arguments: impl IntoIterator<Item = (B, TypeExpr)>,
    ) {
        self.set_require_scope(RequireScope::ModuleCall);
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

/// A schema-judged configuration owns its file and report and borrows the
/// caller's schema registry. Parsing alone cannot construct this type.
/// Consumers borrow this bundle so the file and schemas stay paired.
#[derive(Debug, Clone)]
pub struct JudgedFile<'a, E, D, B: BindingAddress = RelativeBindingName> {
    file: super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &'a crate::schema::SchemaRegistry,
}

impl<E, D, B: BindingAddress> JudgedFile<'_, E, D, B> {
    pub fn file(&self) -> &super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
        &self.file
    }
    pub fn report(&self) -> &ExpressionReport<B> {
        self.file.expression_report()
    }
    pub fn schemas(&self) -> &crate::schema::SchemaRegistry {
        self.schemas
    }
    pub(crate) fn into_file(self) -> super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
        self.file
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
        admit_require_scope(&mut file);
        judge_configuration(&mut file, schemas, config);
        Self { file, schemas }
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

pub(crate) fn set_require_scope<E, D, B: BindingAddress>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    scope: RequireScope,
) {
    file.parser_binding_metadata
        .syntax
        .expressions_mut()
        .set_require_scope(scope);
}

pub(crate) fn require_scope<E, D, B: BindingAddress>(
    file: &super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
) -> RequireScope {
    file.parser_binding_metadata
        .syntax()
        .expressions()
        .require_scope
        .clone()
        .unwrap_or_default()
}

/// Full analysis acquires the directory obligation together with its
/// inference exemption only when the loader proved declaration completeness.
/// An isolated buffer stays unknown and retains ordinary expression checks.
pub(crate) fn admit_require_scope<E, D, B: BindingAddress>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
) {
    let module_blocks = file
        .parser_binding_metadata
        .syntax()
        .module_declaration_blocks();
    let expressions = file.parser_binding_metadata.syntax.expressions_mut();
    let scope = match expressions.require_scope.as_ref() {
        Some(RequireScope::UnvalidatedDirectory(evidence)) => RequireScope::Directory(*evidence),
        Some(_) => return,
        None => RequireScope::for_declarations(
            file.arguments.iter().map(|argument| argument.name.clone()),
            module_blocks,
        ),
    };
    expressions.set_require_scope(scope);
}

pub(crate) fn has_retained_requires<E, D, B: BindingAddress>(
    file: &super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
) -> bool {
    fn contains<P: ValueKind, B: BindingAddress>(root: &ExprValue<P, B>) -> bool {
        matches!(root.authored.kind, ExprKind::RequireCondition(_))
            || root.authored.children.iter().any(contains)
    }
    file.parser_binding_metadata
        .syntax()
        .expressions()
        .roots
        .iter()
        .any(contains)
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
    if file
        .parser_binding_metadata
        .syntax()
        .expressions()
        .require_scope
        .is_none()
    {
        let scope = RequireScope::for_declarations(
            file.arguments.iter().map(|argument| argument.name.clone()),
            file.parser_binding_metadata
                .syntax()
                .module_declaration_blocks(),
        );
        file.parser_binding_metadata
            .syntax
            .expressions_mut()
            .set_require_scope(scope);
    }
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
        let declarations = self
            .captures
            .requires
            .read()
            .expect("retained require declarations")
            .clone();
        let scope = self.require_scope.clone().unwrap_or_default();
        let mut memo = HashMap::new();
        for root in &mut self.roots {
            retain_require_nodes(root, &mut memo, &mut |id, current| {
                declarations
                    .get(&id)
                    .map(|declaration| RetainedRequireCondition {
                        id,
                        scope: scope.clone(),
                        declaration: declaration.clone(),
                    })
                    .or(current)
            });
        }
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
                    }
                    out.roots.push(expression);
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
    ExprValue {
        authored: Arc::new(AuthoredExpr {
            id: NodeId::fresh(),
            definition_use: UseId::fresh(),
            location: ExpressionLocation::new(
                ExpressionInstance::Root,
                &DeclarationLocation::expression_node(pair, file, source),
            ),
            kind,
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
                        kind: ExprKind::Sink(name.clone()),
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
    requires: std::sync::RwLock<
        HashMap<NodeId, super::RequireBlock<ParserPlaceholder, RelativeBindingName>>,
    >,
    resource_declarations: HashMap<NodeId, SlotDeclarationId>,
    values: std::sync::RwLock<HashMap<NodeId, Value<ParserPlaceholder, RelativeBindingName>>>,
    conditions: ConditionRecorder,
}
impl std::fmt::Debug for CapturedNodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CapturedNodes(..)")
    }
}
impl CapturedNodes {
    pub(in crate::parser) fn retain_require(
        &self,
        id: NodeId,
        declaration: super::RequireBlock<ParserPlaceholder, RelativeBindingName>,
    ) {
        self.requires
            .write()
            .expect("retained require declarations")
            .insert(id, declaration);
    }

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
            if matches!(expression.authored.kind, ExprKind::Alias | ExprKind::Parens)
                && let [child] = expression.authored.children.as_slice()
            {
                expression = child;
            } else {
                break;
            }
        }
    }
}

fn compile(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
    captures: &mut CapturedNodes,
) -> ExprValue {
    let key = ParseNodeKey::of(&pair);
    if let Some(expression) = captures.get(&key) {
        return expression.clone();
    }
    let expression = compile_form_entry(pair, file, source, scope, captures);
    captures.insert(key, expression.clone());
    expression
}

fn compile_form_entry(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
    captures: &mut CapturedNodes,
) -> ExprValue {
    // Keep pass-through grammar levels off the large form compiler's stack.
    // Each still receives its authored location and its own checked evidence.
    if matches!(
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
            | Rule::validate_primary
            | Rule::validate_or_expr
            | Rule::validate_and_expr
            | Rule::validate_comparison
            | Rule::validate_additive_expr
            | Rule::validate_multiplicative_expr
    ) || (pair.as_rule() == Rule::validate_not_expr
        && !pair.as_str().trim_start().starts_with('!'))
    {
        let mut parts = pair.clone().into_inner();
        if let Some(child) = parts.next()
            && parts.next().is_none()
        {
            let child = compile(child, file, source, scope, captures);
            let kind =
                if pair.as_rule() == Rule::primary && pair.as_str().trim_start().starts_with('(') {
                    ExprKind::Parens
                } else {
                    ExprKind::Alias
                };
            return node(&pair, kind, vec![child], file, source);
        }
    }
    compile_form(pair, file, source, scope, captures)
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

fn compile_form(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
    captures: &mut CapturedNodes,
) -> ExprValue {
    let mut children = || {
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
    };
    match pair.as_rule() {
        Rule::expression | Rule::primary => {
            let children = children();
            if children.len() == 1 {
                node(&pair, ExprKind::Alias, children, file, source)
            } else {
                node(&pair, ExprKind::Sequence, children, file, source)
            }
        }
        Rule::coalesce_expr => {
            let mut children = children();
            let mut right = children.pop().expect("coalesce operand");
            while let Some(left) = children.pop() {
                right = node(&pair, ExprKind::Coalesce, vec![left, right], file, source);
            }
            right
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
                    ExprKind::Binding(RelativeBindingName::new(name.as_str())),
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
        Rule::list => node(&pair, ExprKind::List, children(), file, source),
        Rule::map => {
            let entries = lower_blocks(
                pair.clone()
                    .into_inner()
                    .map(|entry| {
                        if entry.as_rule() == Rule::map_entry {
                            let mut parts = entry.clone().into_inner();
                            let key = parts.next().expect("map key");
                            // Keys must have the exact spelling used by the
                            // value parser, including its escape handling.
                            let name = super::AttributeKey::literal_in_file(key, file);
                            let value = compile(
                                parts.next().expect("map value"),
                                file,
                                source,
                                scope,
                                captures,
                            );
                            node(&entry, ExprKind::Sink(name), vec![value], file, source)
                        } else {
                            compile(entry, file, source, scope, captures)
                        }
                    })
                    .collect(),
            );
            let keys = entries
                .iter()
                .map(|entry| match &entry.authored.kind {
                    ExprKind::Sink(name) => name.clone(),
                    _ => unreachable!("compiled map entry"),
                })
                .collect();
            node(&pair, ExprKind::Map(keys), entries, file, source)
        }
        Rule::fn_body | Rule::if_body | Rule::else_clause | Rule::for_body => {
            node(&pair, ExprKind::Sequence, children(), file, source)
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
            for parameter in pair
                .clone()
                .into_inner()
                .flatten()
                .filter(|part| part.as_rule() == Rule::fn_param)
            {
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
            }
            let children = pair
                .clone()
                .into_inner()
                .filter(|part| part.as_rule() == Rule::fn_body)
                .map(|body| compile(body, file, source, &scope, captures))
                .collect();
            node(
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
                    }),
                },
                children,
                file,
                source,
            )
        }
        Rule::for_expr => {
            let mut parts = pair.clone().into_inner();
            let binding = parts.next().expect("loop binding");
            let iterable = compile(
                parts.next().expect("loop iterable"),
                file,
                source,
                scope,
                captures,
            );
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
            node(&pair, ExprKind::For, vec![iterable, body], file, source)
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
                children().into_iter().skip(1).collect(),
                file,
                source,
            )
        }
        Rule::attribute => {
            let mut parts = pair.clone().into_inner();
            let name = parts.next().expect("attribute name");
            let name = super::AttributeKey::literal_in_file(name, file);
            let value = compile(
                parts.next().expect("attribute value"),
                file,
                source,
                scope,
                captures,
            );
            node(&pair, ExprKind::Sink(name), vec![value], file, source)
        }
        Rule::if_expr => node(&pair, ExprKind::If, children(), file, source),
        Rule::wait_expr => {
            let mut parts = pair.clone().into_inner();
            let target = parts.next().expect("wait target");
            let target = scope.get(target.as_str()).cloned().unwrap_or_else(|| {
                node(
                    &target,
                    ExprKind::Binding(RelativeBindingName::new(target.as_str())),
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
        | Rule::fn_local_let
        | Rule::attributes_param
        | Rule::exports_param
        | Rule::module_call_arg
        | Rule::arg_default_attr => {
            let values: Vec<_> = pair
                .clone()
                .into_inner()
                .filter(|part| part.as_rule() == Rule::expression)
                .map(|part| compile(part, file, source, scope, captures))
                .collect();
            let kind = if values.is_empty() {
                ExprKind::Declaration
            } else {
                ExprKind::Sequence
            };
            node(&pair, kind, values, file, source)
        }
        Rule::module_call => node(&pair, ExprKind::ModuleCall, children(), file, source),
        Rule::nested_block => {
            let name_pair = pair.clone().into_inner().next().expect("nested block name");
            let name = super::AttributeKey::authored_in_file(name_pair.clone(), file);
            let header = DeclarationLocation::expression_node(&name_pair, file, source);
            let members = children().into_iter().skip(1).collect::<Vec<_>>();
            let (fields, mut locals): (Vec<_>, Vec<_>) = members
                .into_iter()
                .partition(|member| matches!(member.authored.kind, ExprKind::Sink(_)));
            let keys = fields
                .iter()
                .map(|field| match &field.authored.kind {
                    ExprKind::Sink(name) => name.clone(),
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
        | Rule::validation_condition_attr
        | Rule::validation_error_message_attr
        | Rule::attributes_block
        | Rule::exports_block
        | Rule::backend_block
        | Rule::require_statement
        | Rule::wait_attr
        | Rule::wait_until_attr
        | Rule::wait_timeout_attr
        | Rule::wait_depends_on_attr
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
        rule => node(
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

pub(crate) use judgement::judge_expressions;
pub(in crate::parser) use judgement::unjudged as missing_judgement;
pub(crate) use judgement::{
    AdmissionContext, EmptyCoalesceDefault, admit_with_context, coalesce_type,
};
pub use judgement::{
    CollectionKind, ExpressionAdmission, ExpressionCheckOwner, ExpressionReport, JudgedExpr,
    ProviderAuthorityUnavailable, SinkJudgedExpr, admit_expression,
};
#[cfg(any(test, feature = "test-utils"))]
pub(crate) fn value_dependent_judgement_for_test() -> JudgedExpr {
    judgement::constraint_fixture()
}

// Constructors for judgement evidence are confined to this module. In
// particular, the syntax compiler and provisional evaluators cannot mint it.
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
                | Self::Map(knowledge) => knowledge,
                Self::Literal | Self::EnumIdentifier => Classification::Known,
                Self::LoopSlot => Classification::Deferred,
            }
        }
    }

    /// The intrinsic value shape does not depend on nominal identity or on
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
    #[derive(Debug, Clone, PartialEq)]
    pub struct JudgedExpr(Arc<JudgedExprData>);

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
            &self.0
        }
    }

    impl JudgedExpr {
        fn data_mut(&mut self) -> &mut JudgedExprData {
            Arc::make_mut(&mut self.0)
        }
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
        fn concrete(&self) -> bool {
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
        fn failed(&self) -> bool {
            self.failure().is_some()
        }
        fn failure(&self) -> Option<(&InferenceError, FailureOwner)> {
            match &self.judgement {
                ExprJudgement::Failed { error, owner }
                | ExprJudgement::Rejected { error, owner, .. } => Some((error, *owner)),
                _ => None,
            }
        }
        fn reject(&mut self, error: InferenceError, owner: FailureOwner) {
            self.data_mut().judgement = ExprJudgement::Rejected {
                source: Box::new(self.judgement.clone()),
                error,
                owner,
            };
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
        fn mark_failed(&mut self, error: InferenceError, owner: FailureOwner) {
            self.data_mut().judgement = ExprJudgement::Failed { error, owner };
        }
        pub fn static_type(&self) -> Result<&TypeExpr, InferenceIssue> {
            self.judgement.static_type()
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
                    _ => None,
                },
                _ => None,
            }
        }
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
                        _ => false,
                    },
                    _ => actual == &TypeExpr::String && sink.is_string_shaped(),
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
                ordinary.set_kind(InferredKind::Ordinary);
                ordinary.data_mut().member = None;
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
        // Intrinsic reference provenance controls single-reference unwrapping.
        // It does not interpret construction evidence or admit a sink.
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
        /// Borrow the assigned reference for diagnostics when the judgement
        /// preserves it through interpolation. Evaluation keeps the stored value.
        pub fn assignment_value<'a, P: ValueKind, B: BindingAddress>(
            &self,
            value: &'a Value<P, B>,
        ) -> &'a Value<P, B> {
            let reference_assignment = self.preserves_reference_assignment()
                || (!self.alternatives.is_empty()
                    && self
                        .alternatives
                        .iter()
                        .all(Self::preserves_reference_assignment));
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
        fn owned_by_bindings(mut self) -> Self {
            self.report_at(FailureOwner::BindingIndex);
            self
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
                        .map(|shape| format!("{shape:?}"))
                        .unwrap_or_else(|| "value-dependent expression".into())
                })
        }
    }

    /// Expression evidence interpreted at a particular sink. Only this view
    /// exposes construction and assignment categories.
    pub struct SinkJudgedExpr<'a> {
        expression: std::borrow::Cow<'a, JudgedExpr>,
        sink: &'a TypeExpr,
        config: &'a super::super::ProviderContext,
    }

    impl std::ops::Deref for SinkJudgedExpr<'_> {
        type Target = JudgedExpr;
        fn deref(&self) -> &Self::Target {
            &self.expression
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
                _ => None,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum CollectionKind {
        List,
        Map,
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
    ) -> Result<ExpressionAdmission, InferenceError> {
        match &expression.authority {
            Some(authority) => {
                admit_with_context(expression, sink, AdmissionContext::Assignment, authority)
            }
            None => Err(MissingEvidence::new("expression judgement authority").into()),
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
    ) -> Result<ExpressionAdmission, InferenceError> {
        use ExpressionAdmission::{DeferredInterpolation, ProviderPending, ValueCheck};
        let expression = expression.for_sink(sink, config);
        let (ty, kind, concrete) = match &expression.judgement {
            ExprJudgement::Failed { error, .. } | ExprJudgement::Rejected { error, .. } => {
                return Err(error.clone());
            }
            ExprJudgement::AuthorityUnavailable(authority) => {
                return Ok(ProviderPending(authority.clone()));
            }
            ExprJudgement::Declaration => return Err(assignment_error(&expression, sink)),
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
                        admit_with_context(branch, sink, context, config)?,
                    ))
                });
        }
        if expression.is_value_dependent() {
            return Ok(ValueCheck);
        }
        let sink = crate::validation::resolve_type_expr(sink, config).map_err(|error| {
            InferenceError::UnknownType {
                reason: error.to_string(),
            }
        })?;
        let sink = &sink;
        let resolved_actual = ty
            .map(|ty| crate::validation::resolve_type_expr(ty, config))
            .transpose()
            .map_err(|error| InferenceError::UnknownType {
                reason: error.to_string(),
            })?;
        let ty = resolved_actual.as_ref();
        if context == AdmissionContext::Resource
            && matches!(kind, Some(InferredKind::Reference(_)))
            && expression.check_owner() == ExpressionCheckOwner::BindingIndex
        {
            // Retain reference evidence for resource attribute admission. This
            // expression pass has neither the destination descriptor nor the
            // binding authority to decide that assignment a second time.
            return Ok(ValueCheck);
        }
        if context == AdmissionContext::Function
            && (matches!(ty, Some(TypeExpr::Ref(_)))
                || (matches!(kind, Some(InferredKind::Reference(_)))
                    && ty == Some(&TypeExpr::String)
                    && sink.is_string_shaped()))
        {
            // FunctionArgument owns handle identity; schema strings keep concrete refinement checks.
            return Ok(ValueCheck);
        }
        if let TypeExpr::Union(members) = sink {
            if expression.requires_declared_assignment()
                && ty == Some(&TypeExpr::String)
                && !members.is_empty()
                && members
                    .iter()
                    .all(|member| matches!(member, TypeExpr::StringLiteral(_)))
            {
                return Ok(ValueCheck);
            }
            return members
                .iter()
                .find_map(|member| admit_with_context(&expression, member, context, config).ok())
                .ok_or_else(|| assignment_error(&expression, sink));
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
                    .map(|child| (child, inner.as_ref()))
                    .collect(),
                (CollectionKind::Map, TypeExpr::Map(inner)) => expression
                    .fields()
                    .map(|(_, child)| child)
                    .chain(expression.member.iter().map(Arc::as_ref))
                    .map(|child| (child, inner.as_ref()))
                    .collect(),
                (CollectionKind::Map, TypeExpr::Struct { fields }) => fields
                    .iter()
                    .filter(|(name, _)| {
                        context != AdmissionContext::Resource || expression.field(name).is_ok()
                    })
                    .map(|(name, sink)| {
                        expression
                            .field(name)
                            .map(|child| (child, sink))
                            .map_err(|_| InferenceError::UnknownType {
                                reason: format!("expected struct, missing field '{name}'"),
                            })
                    })
                    .collect::<Result<_, _>>()?,
                (CollectionKind::List | CollectionKind::Map, _) => {
                    return Err(assignment_error(&expression, sink));
                }
            };
            return pairs
                .into_iter()
                .try_fold(ValueCheck, |admission, (child, sink)| {
                    Ok(combine_admission(
                        admission,
                        admit_with_context(child, sink, child_context, config)?,
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
                Ok(ValueCheck)
            } else {
                Err(assignment_error(&expression, sink))
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
                Err(assignment_error(&expression, sink))
            };
        }
        if expression.requires_declared_assignment() {
            if *actual == TypeExpr::String && !matches!(sink, TypeExpr::String) {
                return Err(assignment_error(&expression, sink));
            }
            return match crate::validation::typecheck::inferred_type_assignment(
                sink,
                actual,
                &config.custom_types,
            ) {
                crate::schema::TypeAssignability::Assignable => Ok(ValueCheck),
                crate::schema::TypeAssignability::Mismatch => {
                    Err(assignment_error(&expression, sink))
                }
                crate::schema::TypeAssignability::Unchecked(evidence) => evidence
                    .into_types()
                    .find_map(|ty| pending_type(&ty))
                    .map(ProviderPending)
                    .ok_or_else(|| assignment_error(&expression, sink)),
            };
        }
        if context == AdmissionContext::StructValue && concrete {
            return Ok(ValueCheck);
        }
        if let Some(authority) = unavailable_type(sink) {
            return if matches!(
                ValueShape::of_type(actual),
                Some(ValueShape::String | ValueShape::Int | ValueShape::Float)
            ) {
                Ok(ProviderPending(authority))
            } else {
                Err(assignment_error(&expression, sink))
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
                })
            } else {
                Err(InferenceError::UnknownType {
                    reason: format!(
                        "cannot assign deferred interpolation: expected {sink}, got String"
                    ),
                })
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
            Ok(ValueCheck)
        } else {
            Err(assignment_error(&expression, sink))
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

    fn assignment_error(expression: &JudgedExpr, sink: &TypeExpr) -> InferenceError {
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

    #[derive(Clone)]
    pub struct ExpressionReport<B: BindingAddress = RelativeBindingName> {
        bindings: Arc<HashMap<B, JudgedExpr>>,
        expressions: Arc<IndexMap<ExpressionLocation, JudgedExpr>>,
        pub(super) nodes: Arc<HashMap<(ExpressionInstance, NodeId), JudgedExpr>>,
        pub(super) syntax_nodes: Arc<AuthoredNodes<B>>,
        pub(super) use_roots: Arc<Vec<cursor::UseRoot<B>>>,
        pub(super) use_interner: Arc<cursor::UseInterner>,
        pub(super) definitions: Arc<IndexMap<B, ExprValue<ParserPlaceholder, B>>>,
        pub(super) projections:
            Arc<HashMap<(ExpressionInstance, NodeId), Value<ParserPlaceholder, B>>>,
        findings: Arc<Vec<Located<InferenceError>>>,
        function_errors: Arc<Vec<Located<InferenceError>>>,
        operands: Arc<HashMap<ExpressionLocation, [JudgedExpr; 2]>>,
        module_attributes: Arc<IndexMap<(B, String), JudgedExpr>>,
        resource_reports: Arc<HashMap<crate::resource::ResourceId, Arc<ExpressionReport<B>>>>,
        parameters: Arc<HashMap<String, JudgedExpr>>,
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
                syntax_nodes: Arc::default(),
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
                instance: ExpressionInstance::Root,
                authored: false,
            }
        }
        pub(super) fn extend(&mut self, other: Self) {
            Arc::make_mut(&mut self.bindings).extend(Arc::unwrap_or_clone(other.bindings));
            Arc::make_mut(&mut self.expressions).extend(Arc::unwrap_or_clone(other.expressions));
            Arc::make_mut(&mut self.nodes).extend(Arc::unwrap_or_clone(other.nodes));
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
                .collect::<HashMap<_, _>>()
                .into();
            ExpressionReport {
                instance: self.instance.clone(),
                authored: self.authored,
                bindings: self
                    .bindings
                    .iter()
                    .map(|(name, expression)| (map(name.clone()), expression.clone()))
                    .collect::<HashMap<_, _>>()
                    .into(),
                expressions: self.expressions.clone(),
                nodes: self.nodes.clone(),
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
                parameters: self.parameters.clone(),
            }
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
        pub(crate) fn parameter(&self, name: &str) -> Result<&JudgedExpr, MissingEvidence> {
            self.parameters
                .get(name)
                .ok_or_else(|| MissingEvidence::new(format!("parameter {name}")))
        }
        pub(crate) fn record_instance<E, D>(
            &mut self,
            file: &super::super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
            binding: Option<&B>,
        ) where
            D: super::super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
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
        pub(super) fn record_resources<E, D>(
            &mut self,
            file: &super::super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        ) where
            D: super::super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
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
        findings
            .iter()
            .find(|finding| !matches!(finding.detail(), InferenceError::DirectoryConstraint(_)))
            .map(|finding| {
                super::super::ParseError::InvalidExpression {
                    line: finding.location().span().start_line(),
                    message: finding.detail().to_string(),
                }
                .at(finding.location())
            })
    }

    pub fn judge_expressions<E, D, B: BindingAddress>(
        file: &super::super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        schemas: &crate::schema::SchemaRegistry,
        config: &super::super::ProviderContext,
    ) -> ExpressionReport<B>
    where
        D: super::super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let expressions = file.parser_binding_metadata.syntax().expressions();
        let mut bindings = inference::bindings_from_parsed(file);
        for wait in &file.wait_bindings {
            if let Some(target) = bindings.get(wait.target.as_str()).cloned() {
                bindings.insert(wait.binding.to_string(), target);
            }
        }
        fn index<'a, B: BindingAddress>(
            expression: &'a ExprValue<ParserPlaceholder, B>,
            authored: &mut IndexMap<ExpressionLocation, &'a ExprValue<ParserPlaceholder, B>>,
            visited: &mut std::collections::HashSet<usize>,
        ) {
            if !visited.insert(Arc::as_ptr(&expression.authored) as usize) {
                return;
            }
            authored
                .entry(expression.authored.location.clone())
                .or_insert(expression);
            for child in &expression.authored.children {
                index(child, authored, visited);
            }
        }
        let mut authored = IndexMap::new();
        let mut visited = std::collections::HashSet::new();
        for expression in expressions
            .roots
            .iter()
            .chain(expressions.definitions.values())
        {
            index(expression, &mut authored, &mut visited);
        }
        let mut functions = HashMap::new();
        for expression in authored.values() {
            if let ExprKind::Function { name, signature } = &expression.authored.kind {
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
            scope: DiagnosticScope::Public,
            by_location: IndexMap::new(),
            by_node: HashMap::new(),
            syntax_nodes: HashMap::new(),
            visiting: Vec::new(),
            findings: Vec::new(),
            function_errors: Vec::new(),
            operands: HashMap::new(),
            module_attributes: Some(&expressions.report.module_attributes),
        };
        let mut judged_bindings = HashMap::new();
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
            syntax_nodes: Arc::new(judge.syntax_nodes),
            use_roots: Arc::default(),
            use_interner: Arc::default(),
            definitions: Arc::new(expressions.definitions.clone()),
            projections: expressions.projections.clone(),
            findings: Arc::new(judge.findings),
            function_errors: Arc::new(judge.function_errors),
            operands: Arc::new(judge.operands),
            module_attributes: Arc::new(module_attributes),
            resource_reports: Arc::default(),
            parameters: file
                .arguments
                .iter()
                .map(|argument| {
                    (argument.name.clone(), {
                        let mut parameter = declared(Ok(argument.static_type.clone()), false);
                        parameter.data_mut().authority = Some(authority.clone());
                        parameter
                    })
                })
                .collect::<HashMap<_, _>>()
                .into(),
        };
        let mut snapshots = HashMap::new();
        #[allow(clippy::iter_over_hash_type)]
        // keyed rebuild: each instance snapshot depends only on that instance and the common report
        for (id, previous) in expressions.report.resource_reports.iter() {
            let snapshot = snapshots
                .entry(previous.instance.clone())
                .or_insert_with(|| {
                    let mut snapshot = report.clone();
                    snapshot.instance = previous.instance.clone();
                    snapshot.resource_reports = Arc::default();
                    Arc::new(snapshot)
                });
            Arc::make_mut(&mut report.resource_reports).insert(id.clone(), snapshot.clone());
        }
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
            definition: definitions.contains(&expression.node_id()),
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
        report.record_instance(file, None);
        report
    }

    struct Judge<'a, B: BindingAddress> {
        authority: Arc<super::super::ProviderContext>,
        authored: IndexMap<ExpressionLocation, &'a ExprValue<ParserPlaceholder, B>>,
        expressions: &'a Expressions<B>,
        bindings: &'a InferenceBindings<ParserPlaceholder, B>,
        functions: HashMap<(ExpressionInstance, String), Arc<FunctionSignature>>,
        schemas: &'a crate::schema::SchemaRegistry,
        config: &'a super::super::ProviderContext,
        memo: HashMap<(usize, DiagnosticScope), JudgedExpr>,
        scope: DiagnosticScope,
        by_location: IndexMap<ExpressionLocation, JudgedExpr>,
        by_node: HashMap<(ExpressionInstance, NodeId), JudgedExpr>,
        syntax_nodes: AuthoredNodes<B>,
        visiting: Vec<usize>,
        findings: Vec<Located<InferenceError>>,
        function_errors: Vec<Located<InferenceError>>,
        operands: HashMap<ExpressionLocation, [JudgedExpr; 2]>,
        module_attributes: Option<&'a IndexMap<(B, String), JudgedExpr>>,
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

        fn judge(&mut self, expression: &ExprValue<ParserPlaceholder, B>) -> JudgedExpr {
            let root = &expression.authored;
            let id = Arc::as_ptr(root) as usize;
            let scope = if self.scope.is_secret() || matches!(root.kind, ExprKind::Secret) {
                DiagnosticScope::Secret
            } else {
                DiagnosticScope::Public
            };
            if let Some(judged) = self.memo.get(&(id, scope)) {
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
                return failure;
            }
            let outer_scope = self.scope;
            self.scope = scope;
            self.visiting.push(id);
            let directory_require = match &root.kind {
                ExprKind::RequireCondition(require)
                    if matches!(require.scope, RequireScope::Directory(_)) =>
                {
                    Some(require.clone())
                }
                _ => None,
            };
            let mut children = if directory_require.is_some() {
                Vec::new()
            } else {
                root.children
                    .iter()
                    .map(|child| self.judge(child))
                    .collect::<Vec<_>>()
            };
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
                let knowledge = list.elements.iter().all(JudgedExpr::concrete).into();
                list.data_mut().judgement = ExprJudgement::ValueDependent {
                    reason: NotInferableReason::ValueDependent {
                        reason: "join accepts independently rendered scalar elements".into(),
                    },
                    shape: Some((CollectionKind::List, knowledge)),
                    owner: FailureOwner::Boundary,
                };
            }
            let mut result = if let Some(require) = directory_require {
                if let Some(diagnostic) =
                    crate::module_resolver::evaluate_directory_require(&require.declaration)
                {
                    self.push_finding(
                        root.location.authored.clone(),
                        InferenceError::DirectoryConstraint(Box::new(diagnostic)),
                    );
                }
                declaration()
            } else {
                self.infer(expression, &children)
            };
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
            if let Some(failed) = children.iter().find(|child| child.failed()) {
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
            self.memo.insert((id, scope), result.clone());
            self.by_location
                .insert(root.location.clone(), result.clone());
            self.by_node
                .insert((root.location.instance.clone(), root.id), result.clone());
            self.syntax_nodes
                .insert((root.location.instance.clone(), root.id), root.clone());
            result
        }

        fn refresh_module_output(&mut self, evidence: &JudgedExpr) -> JudgedExpr {
            let Some(origin) = evidence.origin.as_ref() else {
                return evidence.clone();
            };
            let mut output = match self.authored.get(origin).copied() {
                Some(authored) => self.judge(authored),
                None => incomplete("authored module output origin"),
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
        ) -> Option<JudgedExpr> {
            let error = crate::validation::typecheck::admit_function_expression(
                expression,
                sink,
                self.config,
            )
            .err()?;
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
                return Some(expression.clone());
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
            base: &JudgedExpr,
            step: Option<&PathSegment>,
            binding: &str,
        ) -> JudgedExpr {
            if base.is_value_dependent()
                || base.failed()
                || matches!(base.judgement, ExprJudgement::AuthorityUnavailable(_))
                || base.is_enum_identifier()
            {
                return base.clone();
            }
            let Some(step) = step else {
                if let Some(member) = &base.member {
                    return (**member).clone();
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
            let selected = match step {
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
            };
            if let Some(selected) = selected {
                return selected.clone();
            }
            let ty = base
                .static_type()
                .cloned()
                .and_then(|ty| self.narrow_declared(&ty, std::slice::from_ref(step), binding));
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
            base: JudgedExpr,
            steps: &[PathSegment],
            binding: &str,
        ) -> JudgedExpr {
            steps
                .iter()
                .fold(base, |base, step| self.access(&base, Some(step), binding))
        }

        fn infer(
            &mut self,
            expression: &ExprValue<ParserPlaceholder, B>,
            children: &[JudgedExpr],
        ) -> JudgedExpr {
            let root = &expression.authored;
            let ordinary = declared;
            let child_type = |index: usize| {
                children.get(index).map_or_else(
                    || incomplete("operand").static_type().cloned(),
                    |child| child.static_type().cloned(),
                )
            };
            match &root.kind {
                ExprKind::Scalar(ty) => {
                    ordinary(Ok(ty.clone()), true).with_kind(|_| InferredKind::Literal)
                }
                ExprKind::Parameter(ty) => ordinary(Ok(ty.clone()), false).owned_by_bindings(),
                ExprKind::LoopSlot(ty) => ordinary(Ok(ty.clone()), false)
                    .with_kind(|_| InferredKind::LoopSlot)
                    .owned_by_bindings(),
                ExprKind::UntypedParameter => value_dependent("unannotated function parameter"),
                ExprKind::Binding(name) => {
                    if let Some(ty) = self.parameter_type(name) {
                        ordinary(Ok(ty.clone()), false).owned_by_bindings()
                    } else if let Some(definition) = self.expressions.definitions.get(name) {
                        self.judge(definition)
                    } else {
                        ordinary(Ok(TypeExpr::String), true).with_kind(|_| InferredKind::Literal)
                    }
                }
                ExprKind::Reference {
                    binding,
                    attribute,
                    segments,
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
                        return self.access_path(attribute, segments, binding.as_str());
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
                        && !attributes.contains_key(attribute)
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
                        self.access_path(base, &steps, binding.as_str())
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
                    children[0].clone()
                }
                ExprKind::Interpolation { .. } => ordinary(
                    Ok(TypeExpr::String),
                    children.iter().all(JudgedExpr::concrete),
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
                        ty => collection(ty, shape, children.iter().all(JudgedExpr::concrete)),
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
                    let body = authored_child(children.last());
                    if let Some(sink) = &signature.return_type
                        && let Some(failure) = self.check_function_sink(
                            &body,
                            sink,
                            root.children.last().unwrap().location(),
                            root.children.last().unwrap().location(),
                            &format!("function '{name}': return value"),
                        )
                    {
                        return failure;
                    }
                    declaration()
                }
                ExprKind::RequireCondition(_) => authored_child(children.first()),
                ExprKind::Sequence => authored_child(children.last()),
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
                    self.operands.insert(
                        ExpressionLocation::new(root.location.instance.clone(), location),
                        operands.clone(),
                    );
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
                            ExprKind::Binding(binding) | ExprKind::Reference { binding, .. } => {
                                Some(binding)
                            }
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
                        .cloned()
                        .unwrap_or_else(|| incomplete("iterable operand"))
                }
                ExprKind::Alias
                | ExprKind::Parens
                | ExprKind::Definition
                | ExprKind::Wait
                | ExprKind::Sink(_) => authored_child(children.first()),
                ExprKind::Element => {
                    let collection = &children[0];
                    if collection.is_value_dependent() {
                        return value_dependent(
                            "external iterable elements require resolved value checks",
                        );
                    }
                    if let Some(member) = &collection.member {
                        return loop_element((**member).clone());
                    }
                    let members = collection
                        .elements
                        .iter()
                        .chain(collection.fields.values())
                        .cloned()
                        .collect::<Vec<_>>();
                    if !members.is_empty() {
                        let mut element = members[0].clone();
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
                    let branches = &children[1..];
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
                    let mut result = alternatives(ty, branches);
                    if !branches.iter().any(JudgedExpr::failed) && result.failed() {
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
                        *knowledge = children.iter().all(JudgedExpr::concrete).into();
                    }
                    result
                }
                ExprKind::For => {
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
                    let body = children
                        .last()
                        .cloned()
                        .unwrap_or_else(|| incomplete("for body"));
                    let mut result = collection(
                        body.static_type()
                            .cloned()
                            .map(|ty| TypeExpr::List(Box::new(ty))),
                        CollectionKind::List,
                        false,
                    );
                    result.data_mut().member = Some(Arc::new(body));
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
                ExprKind::NestedBlock { .. } => children[0].clone(),
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
                    ExprKind::Sink(name) => {
                        if let Some(attribute) = name.effective_attribute(schema.schema()) {
                            if attribute.is_read_only() {
                                continue;
                            }
                            let expected = crate::validation::expression_sink_type(
                                attribute.field_type(),
                                schema.owner(),
                                schema.schema().as_schema().defs(),
                            );
                            let value = &child.authored.children[0];
                            let judged = self.judge(value);
                            let admission = admit_with_context(
                                &judged,
                                &expected,
                                AdmissionContext::Resource,
                                self.config,
                            );
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
                                    (Arc::as_ptr(&value.authored) as usize, self.scope),
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

    fn loop_element(mut expression: JudgedExpr) -> JudgedExpr {
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
                .cloned()
                .map(loop_element)
                .collect(),
        );
        expression
    }

    /// Join source evidence independently of a selected projection. Exact
    /// containers keep evidence for every index/key any branch can produce.
    fn alternatives(ty: Result<TypeExpr, InferenceIssue>, branches: &[JudgedExpr]) -> JudgedExpr {
        if branches.len() == 1 {
            return branches[0].clone();
        }
        let mut result = declared(ty, branches.iter().all(|branch| branch.concrete()));
        result.data_mut().authority = branches.first().and_then(|branch| branch.authority.clone());
        result.data_mut().alternatives = Arc::new(branches.to_vec());
        let member_sources = branches
            .iter()
            .filter_map(|branch| branch.member.as_deref())
            .cloned()
            .collect::<Vec<_>>();
        result.data_mut().member = member_sources
            .first()
            .map(|first| Arc::new(alternatives(first.static_type().cloned(), &member_sources)));
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
                    alternatives(sources[0].static_type().cloned(), &sources)
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
                        alternatives(sources[0].static_type().cloned(), &sources),
                    )
                })
                .collect(),
        );
        result
    }

    /// Interpret the registry declaration once for both calls and intrinsics.
    fn builtin_result(
        result_typing: crate::builtins::BuiltinResultTyping,
        children: &[JudgedExpr],
    ) -> JudgedExpr {
        use crate::builtins::BuiltinResultTyping as R;
        let concrete = children.iter().all(JudgedExpr::concrete);
        match result_typing {
            R::Argument(index) => children
                .get(index)
                .cloned()
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
                result.data_mut().member = Some(Arc::new(
                    declared(Ok(TypeExpr::String), concrete)
                        .with_kind(InferredKind::BuiltinConstruction),
                ));
                result.with_kind(InferredKind::BuiltinStringList)
            }
            R::List => builtin_collection(CollectionKind::List, concrete),
            R::Map => builtin_collection(CollectionKind::Map, concrete),
            R::Any => value_dependent("builtin return type depends on arguments"),
        }
    }

    fn builtin_collection(shape: CollectionKind, concrete: bool) -> JudgedExpr {
        let mut result = collection(
            value_dependent("builtin return type depends on arguments")
                .static_type()
                .cloned(),
            shape,
            concrete,
        );
        result.data_mut().member = Some(Arc::new(value_dependent(
            "builtin elements require resolved constraint checks",
        )));
        result
    }

    fn collection(
        ty: Result<TypeExpr, InferenceIssue>,
        shape: CollectionKind,
        concrete: bool,
    ) -> JudgedExpr {
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
        });
        result.report_at(FailureOwner::Boundary);
        result
    }

    fn declared(ty: Result<TypeExpr, InferenceIssue>, concrete: bool) -> JudgedExpr {
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
                | NotInferableReason::NonInferableBinding { .. }
                | NotInferableReason::PlaceholderTypeUnavailable { .. }),
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
        JudgedExpr(Arc::new(JudgedExprData {
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
        }))
    }
    pub(crate) fn unjudged() -> Result<JudgedExpr, MissingEvidence> {
        Err(MissingEvidence::new("declaration has not been judged"))
    }
    fn authored_child(child: Option<&JudgedExpr>) -> JudgedExpr {
        child
            .cloned()
            .unwrap_or_else(|| incomplete("authored child"))
    }

    fn incomplete(request: &str) -> JudgedExpr {
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
        result
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
            refreshed.inherit_output_materialization(&previous);
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
            admit_expression(&expression, &TypeExpr::String)
                .unwrap_err()
                .to_string()
                .contains("expression judgement authority")
        );
    }

    pub(super) fn value_dependent(reason: &str) -> JudgedExpr {
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
    fn declaration() -> JudgedExpr {
        let mut result = value_dependent("declaration is not a value");
        result.data_mut().judgement = ExprJudgement::Declaration;
        result
    }
}
