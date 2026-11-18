//! Structural secrecy of authored configuration. Values obtained from providers
//! and state never enter this owner. Availability and secrecy are separate facts.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use indexmap::{IndexMap, IndexSet};

use crate::parser::retained::{ExprKind, ExprValue, ExpressionInstance, ExpressionReport, NodeId};
use crate::parser::{DeclarationLocation, File, ResourceRef};
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, FunctionArgument, InterpolationPart,
    ParserPlaceholder, PathSegment, ResourceId, Subscript, Value,
};
use crate::schema::{SchemaKind, SchemaRegistry};

pub const MAX_REFERENCE_DEPTH: usize = 128;
pub const MAX_REFERENCE_VISITS: usize = 4096;

/// Follow one more reference; expression nesting does not add reference depth.
fn depth_after_reference(depth: usize) -> usize {
    depth + 1
}

/// The only leaves admitted into runtime evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Secrecy {
    Secret,
    Public,
}

/// A payload-free description of an authored value's secret paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    Leaf(Secrecy),
    Map(Arc<BTreeMap<String, Evidence>>),
    List(Arc<Vec<Evidence>>),
}

impl Evidence {
    pub fn secrecy(&self) -> Secrecy {
        match self {
            Self::Leaf(secrecy) => *secrecy,
            Self::Map(fields) => aggregate(fields.values()),
            Self::List(elements) => aggregate(elements.iter()),
        }
    }

    fn select(&self, path: &[PathSegment]) -> Option<Self> {
        let mut value = self;
        for step in path {
            value = match (value, step) {
                (Self::Leaf(secrecy), _) => return Some(Self::Leaf(*secrecy)),
                (Self::Map(fields), PathSegment::Field { name }) => fields.get(name)?,
                (
                    Self::Map(fields),
                    PathSegment::Subscript {
                        index: Subscript::Str { key },
                    },
                ) => fields.get(key.expose_plaintext())?,
                (
                    Self::List(elements),
                    PathSegment::Subscript {
                        index: Subscript::Int { index },
                    },
                ) => elements.get(usize::try_from(*index).ok()?)?,
                (
                    Self::Map(_),
                    PathSegment::Subscript {
                        index: Subscript::Int { index: _ },
                    },
                )
                | (Self::List(_), PathSegment::Field { name: _ })
                | (
                    Self::List(_),
                    PathSegment::Subscript {
                        index: Subscript::Str { key: _ },
                    },
                ) => return None,
            };
        }
        Some(value.clone())
    }
}

fn aggregate<'a>(mut values: impl Iterator<Item = &'a Evidence>) -> Secrecy {
    if values.any(|value| value.secrecy() == Secrecy::Secret) {
        Secrecy::Secret
    } else {
        Secrecy::Public
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Alternatives {
    Coalesce,
    Lookup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MixedAlternatives {
    pub operator: Alternatives,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKind {
    Depth,
    ReferenceVisits,
}

impl LimitKind {
    /// Render the core-owned limit message for an authored declaration.
    pub fn message(self, declaration_kind: &str, name: &str) -> String {
        match self {
            Self::Depth => format!(
                "{declaration_kind} '{name}': the value follows a chain of {MAX_REFERENCE_DEPTH} or more references from this attribute; Carina cannot decide whether it is a secret."
            ),
            Self::ReferenceVisits => format!(
                "{declaration_kind} '{name}': deciding whether the value is a secret needs more than {MAX_REFERENCE_VISITS} reference lookups from this attribute."
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LimitReached {
    pub kind: LimitKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DerivationError {
    MixedAlternatives(MixedAlternatives),
    LimitReached(LimitReached),
    /// Uses the parser's existing RecursiveFunction verdict and message.
    RecursiveFunction(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallSite {
    pub name: String,
    pub location: DeclarationLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DeclarationKind {
    Attribute,
    LetBinding,
    ModuleArgument,
    Output,
    Export,
    Function,
}
impl DeclarationKind {
    fn label(self) -> &'static str {
        match self {
            Self::Attribute => "Attribute",
            Self::LetBinding => "Let binding",
            Self::ModuleArgument => "Module argument",
            Self::Output => "Output",
            Self::Export => "Export",
            Self::Function => "Function",
        }
    }
    fn from_syntax(kind: crate::config_loader::DeclarationKind) -> Option<Self> {
        use crate::config_loader::DeclarationKind as Syntax;
        match kind {
            Syntax::Binding => Some(Self::LetBinding),
            Syntax::Argument => Some(Self::ModuleArgument),
            Syntax::ModuleAttribute => Some(Self::Output),
            Syntax::Export => Some(Self::Export),
            Syntax::UserFunction => Some(Self::Function),
            Syntax::ProviderKind => None,
        }
    }
}

type DeclarationIdentity = (ExpressionInstance, NodeId);

fn literal_root<B: BindingAddress>(mut source: &ExprValue<ParserPlaceholder, B>) -> bool {
    loop {
        match source.secrecy_kind() {
            ExprKind::Scalar(_) | ExprKind::EmptyInterpolation => return true,
            ExprKind::Interpolation { .. } if source.secrecy_children().is_empty() => return true,
            ExprKind::Alias | ExprKind::Parens | ExprKind::Definition
                if source.secrecy_children().len() == 1 =>
            {
                source = &source.secrecy_children()[0]
            }
            _ => return false,
        }
    }
}

#[derive(Clone)]
struct Declaration {
    identity: DeclarationIdentity,
    kind: DeclarationKind,
    name: String,
    location: DeclarationLocation,
}

/// The core owns message text and both authored and instantiated locations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Finding {
    declaration: DeclarationIdentity,
    kind: DeclarationKind,
    attribute: String,
    location: DeclarationLocation,
    call_site: Option<CallSite>,
    cause: DerivationError,
}

impl Finding {
    pub fn attribute(&self) -> &str {
        &self.attribute
    }
    pub fn location(&self) -> &DeclarationLocation {
        &self.location
    }
    pub fn call_site(&self) -> Option<&CallSite> {
        self.call_site.as_ref()
    }
    pub fn cause(&self) -> &DerivationError {
        &self.cause
    }
    pub fn message(&self) -> String {
        let kind = self.kind.label();
        match &self.cause {
            DerivationError::MixedAlternatives(MixedAlternatives { operator }) => {
                let operator = match operator {
                    Alternatives::Coalesce => "??",
                    Alternatives::Lookup => "lookup()",
                };
                format!(
                    "{kind} '{}': {operator} alternatives mix secret and public values; all alternatives must be secret or all public.",
                    self.attribute
                )
            }
            DerivationError::LimitReached(LimitReached { kind: limit }) => {
                limit.message(kind, &self.attribute)
            }
            DerivationError::RecursiveFunction(name) => {
                crate::parser::ParseError::RecursiveFunction(name.clone()).to_string()
            }
        }
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let span = self.location.span();
        write!(
            f,
            "{}:{}:{}: {}",
            self.location.file().path().display(),
            span.start_line(),
            span.start_column(),
            self.message()
        )?;
        if let Some(call) = &self.call_site {
            let span = call.location.span();
            write!(
                f,
                " (module instance '{}' at {}:{}:{})",
                call.name,
                call.location.file().path().display(),
                span.start_line(),
                span.start_column()
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for Finding {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("structural secrecy validation is pending")]
pub struct PendingSecrecy;

/// Non-shipping counters used by depth, visit-budget and memoization regressions.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct TraversalStats {
    pub attribute: String,
    pub maximum_depth: usize,
    pub reference_visits: usize,
    pub memo_hits: usize,
    pub syntax_entries: usize,
    pub function_body_evaluations: usize,
    pub function_memo_hits: usize,
    pub limit: Option<LimitKind>,
}

/// Diagnostic results are intentionally distinct from admitted runtime evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecrecyReport {
    findings: Vec<Finding>,
    finding_keys: HashSet<(
        DeclarationIdentity,
        DeclarationLocation,
        Option<CallSite>,
        DerivationError,
    )>,
    pending: bool,
    rows: Arc<IndexMap<ResourceId, Row>>,
    #[cfg(test)]
    traversals: Vec<TraversalStats>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    declaration: crate::parser::retained::SlotDeclarationId,
    instance: Option<crate::parser::retained::ExpansionInstanceId>,
    attributes: IndexMap<String, Evidence>,
}

impl SecrecyReport {
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    #[cfg(test)]
    fn traversals(&self) -> &[TraversalStats] {
        &self.traversals
    }

    #[cfg(test)]
    fn record<B: BindingAddress>(&mut self, walk: &Walk<'_, B>) {
        self.traversals.push(TraversalStats {
            attribute: walk.attribute.clone(),
            maximum_depth: walk.max_depth,
            reference_visits: walk.visits,
            memo_hits: walk.memo_hits,
            syntax_entries: walk.syntax_entries,
            function_body_evaluations: walk.function_body_evaluations,
            function_memo_hits: walk.function_memo_hits,
            limit: walk
                .findings
                .iter()
                .find_map(|finding| match finding.cause() {
                    DerivationError::LimitReached(LimitReached { kind }) => Some(*kind),
                    DerivationError::MixedAlternatives(_)
                    | DerivationError::RecursiveFunction(_) => None,
                }),
        });
    }

    fn add_findings(&mut self, findings: impl IntoIterator<Item = Finding>) {
        for finding in findings {
            if self.finding_keys.insert((
                finding.declaration.clone(),
                finding.location.clone(),
                finding.call_site.clone(),
                finding.cause.clone(),
            )) {
                self.findings.push(finding);
            }
        }
    }

    fn inspect_source<B: BindingAddress>(
        &mut self,
        owner: &Owner<'_, B>,
        attribute: String,
        location: DeclarationLocation,
        source: ExprValue<ParserPlaceholder, B>,
        environment: Arc<Environment<B>>,
    ) -> Option<Evidence> {
        let mut walk = Walk::new(owner, attribute, location).in_source(&source);
        let evidence = walk.run(Thunk {
            source: Source::Syntax(source),
            environment,
        });
        #[cfg(test)]
        self.record(&walk);
        self.pending |= walk.pending_validation;
        self.add_findings(walk.findings);
        evidence
    }
    // Only successful binding admission calls this. Incomplete buffers,
    // pending schemas and uninstantiated templates cannot mint a graph.
    pub(crate) fn admitted(&self) -> Option<ValidatedSecrecyGraph> {
        (!self.pending && self.findings.is_empty()).then(|| ValidatedSecrecyGraph {
            rows: self.rows.clone(),
        })
    }
}

/// Opaque, immutable evidence, minted only at successful configuration admission.
/// Public AST edits and provider values cannot construct this capability.
///
/// ```compile_fail
/// use carina_core::secrecy::ValidatedSecrecyGraph;
/// let evidence = ValidatedSecrecyGraph { rows: Default::default() };
/// ```
#[derive(Debug, Clone)]
pub struct ValidatedSecrecyGraph {
    rows: Arc<IndexMap<ResourceId, Row>>,
}
impl ValidatedSecrecyGraph {
    pub fn attributes(&self, resource: &ResourceId) -> Option<&IndexMap<String, Evidence>> {
        self.rows.get(resource).map(|row| &row.attributes)
    }
}

#[derive(Clone)]
enum Source<B: BindingAddress> {
    Syntax(ExprValue<ParserPlaceholder, B>),
    Value(Arc<Value<ParserPlaceholder, B>>),
    Argument {
        authored: Box<Thunk<B>>,
        facts: ArgumentFacts,
        activation: Arc<FunctionActivation>,
    },
}
#[derive(Clone)]
struct Thunk<B: BindingAddress> {
    source: Source<B>,
    environment: Arc<Environment<B>>,
}
#[derive(Clone)]
struct Environment<B: BindingAddress> {
    id: usize,
    nodes: Arc<HashMap<(ExpressionInstance, NodeId), Thunk<B>>>,
    names: Arc<HashMap<String, Thunk<B>>>,
    functions: Option<Arc<IndexMap<String, crate::parser::UserFunction<B>>>>,
    conditions: Option<crate::parser::retained::ConditionSnapshot>,
    in_function: bool,
    module: Option<B>,
}
impl<B: BindingAddress> Default for Environment<B> {
    fn default() -> Self {
        Self {
            id: 0,
            nodes: Arc::default(),
            names: Arc::default(),
            functions: None,
            conditions: None,
            in_function: false,
            module: None,
        }
    }
}

impl<B: BindingAddress> Environment<B> {
    fn for_slot(
        slot: &crate::parser::retained::JudgedSlot<B>,
        source: &ExprValue<ParserPlaceholder, B>,
        empty: &Arc<Self>,
    ) -> Arc<Self> {
        if literal_root(source) {
            return empty.clone();
        }
        Arc::new(Self {
            id: next_environment(),
            conditions: Some(slot.secrecy_conditions()),
            // Non-literal slots retain their own memo identity and condition
            // version, while sharing the immutable empty substitution tables.
            ..empty.as_ref().clone()
        })
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ReferenceKey<B: BindingAddress> {
    environment: usize,
    binding: B,
    path: Vec<PathSegment>,
}
#[derive(Clone)]
struct Memo {
    evidence: Evidence,
    relative_depth: usize,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum ArgumentOrigin {
    UnboundParameter,
    Reference {
        instance: Option<ExpressionInstance>,
        binding: String,
        path: Vec<PathSegment>,
        environment: usize,
        activation: Arc<FunctionActivation>,
    },
    Syntax(DeclarationIdentity, usize, Arc<FunctionActivation>),
    Value(usize, usize, Arc<FunctionActivation>),
}

/// Payload observations are needed only by the existing condition/shape
/// helpers. Ordinary string arguments share a key by their secrecy evidence.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
struct ArgumentObservations {
    boolean: Option<bool>,
    string: Option<String>,
    non_lists: Option<Vec<bool>>,
}

#[derive(Clone)]
struct ArgumentFacts {
    proof: Option<Evidence>,
    observations: ArgumentObservations,
    // Whole resource handles and pending selections need their authored
    // source, rather than treating all missing evidence as the same argument.
    origin: Option<ArgumentOrigin>,
}

// Memo keys must not introduce Rust-stack recursion for deeply nested
// expression evidence. Both hashing and equality use the same heap traversal
// as the secrecy walk, with no expression-depth limit.
impl PartialEq for ArgumentFacts {
    fn eq(&self, other: &Self) -> bool {
        if self.observations != other.observations || self.origin != other.origin {
            return false;
        }
        let mut pending = match (&self.proof, &other.proof) {
            (None, None) => return true,
            (Some(left), Some(right)) => vec![(left, right)],
            (None, Some(_)) | (Some(_), None) => return false,
        };
        while let Some((left, right)) = pending.pop() {
            match (left, right) {
                (Evidence::Leaf(left), Evidence::Leaf(right)) => {
                    if left != right {
                        return false;
                    }
                }
                (Evidence::List(left), Evidence::List(right)) => {
                    if Arc::ptr_eq(left, right) {
                        continue;
                    }
                    if left.len() != right.len() {
                        return false;
                    }
                    pending.extend(left.iter().zip(right.iter()));
                }
                (Evidence::Map(left), Evidence::Map(right)) => {
                    if Arc::ptr_eq(left, right) {
                        continue;
                    }
                    if left.len() != right.len() {
                        return false;
                    }
                    for ((left_key, left), (right_key, right)) in left.iter().zip(right.iter()) {
                        if left_key != right_key {
                            return false;
                        }
                        pending.push((left, right));
                    }
                }
                (Evidence::Leaf(_), Evidence::Map(_) | Evidence::List(_))
                | (Evidence::Map(_) | Evidence::List(_), Evidence::Leaf(_))
                | (Evidence::Map(_), Evidence::List(_))
                | (Evidence::List(_), Evidence::Map(_)) => return false,
            }
        }
        true
    }
}

impl Eq for ArgumentFacts {}

impl Hash for ArgumentFacts {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.observations.hash(state);
        self.origin.hash(state);
        let Some(proof) = &self.proof else {
            state.write_u8(0);
            return;
        };
        state.write_u8(1);
        let mut pending = vec![proof];
        while let Some(proof) = pending.pop() {
            match proof {
                Evidence::Leaf(secrecy) => {
                    state.write_u8(0);
                    secrecy.hash(state);
                }
                Evidence::Map(fields) => {
                    state.write_u8(1);
                    fields.len().hash(state);
                    for key in fields.keys() {
                        key.hash(state);
                    }
                    pending.extend(fields.values().rev());
                }
                Evidence::List(items) => {
                    state.write_u8(2);
                    items.len().hash(state);
                    pending.extend(items.iter().rev());
                }
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ArgumentDependencies {
    booleans: bool,
    strings: bool,
    list_shapes: bool,
}

impl ArgumentDependencies {
    fn merge(&mut self, other: Self) -> bool {
        let changed = (other.booleans && !self.booleans)
            || (other.strings && !self.strings)
            || (other.list_shapes && !self.list_shapes);
        self.booleans |= other.booleans;
        self.strings |= other.strings;
        self.list_shapes |= other.list_shapes;
        changed
    }
}

impl ArgumentFacts {
    fn for_key(&self, dependencies: ArgumentDependencies) -> Self {
        let mut facts = self.clone();
        if !dependencies.booleans {
            facts.observations.boolean = None;
        }
        if !dependencies.strings {
            facts.observations.string = None;
        }
        if !dependencies.list_shapes {
            facts.observations.non_lists = None;
        }
        facts
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum FunctionIdentity<B: BindingAddress> {
    Syntax(DeclarationIdentity),
    Value(DeclarationLocation, Option<B>),
}

impl<B: BindingAddress> FunctionIdentity<B> {
    fn active_in(&self, activation: &FunctionActivation) -> bool {
        match self {
            Self::Syntax(identity) => activation.syntax.contains(identity),
            Self::Value(location, _) => activation.values.contains(location),
        }
    }
}

#[derive(Clone)]
enum ReferenceActivation {
    Body(FunctionActivation),
    Argument(FunctionActivation),
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FunctionKey<B: BindingAddress> {
    identity: FunctionIdentity<B>,
    arguments: Vec<ArgumentFacts>,
    path: Vec<PathSegment>,
    // Only reference-bearing bodies depend on the caller's recursion context.
    // Reference-free results use None, including declaration-only walks.
    activation: Option<Arc<FunctionActivation>>,
}

struct FunctionDependencies<B: BindingAddress> {
    observations: ArgumentDependencies,
    callees: IndexSet<FunctionIdentity<B>>,
}

impl<B: BindingAddress> Default for FunctionDependencies<B> {
    fn default() -> Self {
        Self {
            observations: ArgumentDependencies::default(),
            callees: IndexSet::new(),
        }
    }
}

#[derive(Clone)]
struct ReferenceEffect<B: BindingAddress> {
    binding: B,
    path: Vec<PathSegment>,
    environment: Arc<Environment<B>>,
    relative_depth: usize,
    environments: Arc<BTreeMap<usize, usize>>,
    activation: ReferenceActivation,
}

struct FunctionMemo<B: BindingAddress> {
    evidence: Option<Evidence>,
    findings: Vec<Finding>,
    relative_depth: usize,
    // Replaying encounters (rather than adding the first run's visit total)
    // preserves reference memo hits, including warm vs cold shared DAGs.
    references: Vec<ReferenceEffect<B>>,
    local_environments: BTreeSet<usize>,
    // Pending Value origins use allocation identity; retain those allocations
    // for as long as their memo keys can be reused.
    _arguments: Vec<Thunk<B>>,
}

struct FunctionRecording<B: BindingAddress> {
    key: FunctionKey<B>,
    depth: usize,
    max_before: usize,
    errors: usize,
    reference_nesting: usize,
    references: Vec<ReferenceEffect<B>>,
    local_environments: BTreeSet<usize>,
    arguments: Vec<Thunk<B>>,
    activation: FunctionActivation,
}

struct ResourceSource<B: BindingAddress> {
    provider: String,
    resource_type: String,
    kind: SchemaKind,
    attributes: IndexMap<String, Thunk<B>>,
}

struct Owner<'a, B: BindingAddress> {
    syntax: &'a ExpressionReport<B>,
    schemas: &'a SchemaRegistry,
    resources: IndexMap<B, ResourceSource<B>>,
    upstream: HashSet<B>,
    declarations: Vec<Declaration>,
    declaration_index: OnceLock<DeclarationIndex>,
    reference_free: RefCell<HashMap<DeclarationIdentity, bool>>,
    literal_evidence: RefCell<HashMap<DeclarationIdentity, Evidence>>,
    function_evidence: RefCell<HashMap<FunctionKey<B>, Arc<FunctionMemo<B>>>>,
    function_observations: RefCell<HashMap<FunctionIdentity<B>, ArgumentDependencies>>,
    argument_environments: RefCell<HashMap<B, Arc<Environment<B>>>>,
    empty_environment: Arc<Environment<B>>,
}

type DeclarationIndex = IndexMap<(crate::parser::SourceFileId, Option<CallSite>), DeclarationSpans>;

#[derive(Default)]
struct DeclarationSpans {
    declarations: Vec<usize>,
    /// A balanced interval index prunes disjoint siblings even when a large
    /// enclosing let declaration covers all their spans.
    max_ends: Vec<usize>,
}

#[cfg(test)]
thread_local! {
    static DECLARATION_INDEX_OPERATIONS: std::cell::Cell<(usize, usize)> = const {
        std::cell::Cell::new((0, 0))
    };
    static DECLARATION_SPAN_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl<B: BindingAddress> Owner<'_, B> {
    fn cache_function_observations(
        &self,
        graph: IndexMap<FunctionIdentity<B>, FunctionDependencies<B>>,
    ) {
        let mut known = self.function_observations.borrow_mut();
        let mut callers: HashMap<FunctionIdentity<B>, Vec<FunctionIdentity<B>>> = HashMap::new();
        let mut pending = Vec::new();
        for (identity, dependencies) in graph {
            known
                .entry(identity.clone())
                .or_default()
                .merge(dependencies.observations);
            pending.push(identity.clone());
            for callee in dependencies.callees {
                callers
                    .entry(callee.clone())
                    .or_default()
                    .push(identity.clone());
                pending.push(callee);
            }
        }
        // Each observation bit propagates once along each call edge. Indexing
        // a fan-out call graph must not itself re-expand that graph.
        while let Some(identity) = pending.pop() {
            let observations = known.get(&identity).copied().unwrap_or_default();
            for caller in callers.get(&identity).into_iter().flatten() {
                if known.entry(caller.clone()).or_default().merge(observations) {
                    pending.push(caller.clone());
                }
            }
        }
    }

    fn function_observations(
        &self,
        function: &ExprValue<ParserPlaceholder, B>,
    ) -> ArgumentDependencies {
        let identity =
            FunctionIdentity::Syntax((function.secrecy_instance().clone(), function.node_id()));
        if let Some(uses) = self.function_observations.borrow().get(&identity) {
            return *uses;
        }
        let mut pending = vec![(function.clone(), identity.clone())];
        let mut seen = HashSet::new();
        let mut graph: IndexMap<FunctionIdentity<B>, FunctionDependencies<B>> = IndexMap::new();
        while let Some((source, owner)) = pending.pop() {
            if !seen.insert((
                (source.secrecy_instance().clone(), source.node_id()),
                owner.clone(),
            )) {
                continue;
            }
            let dependencies = graph.entry(owner.clone()).or_default();
            dependencies.observations.booleans |= matches!(source.secrecy_kind(), ExprKind::If);
            if let ExprKind::Call(name) = source.secrecy_kind() {
                if let Some(callee) = self
                    .syntax
                    .secrecy_function(source.secrecy_instance(), name)
                {
                    let callee_identity = FunctionIdentity::Syntax((
                        callee.secrecy_instance().clone(),
                        callee.node_id(),
                    ));
                    dependencies.callees.insert(callee_identity.clone());
                    if !self
                        .function_observations
                        .borrow()
                        .contains_key(&callee_identity)
                    {
                        pending.push((callee, callee_identity));
                    }
                } else {
                    let closure = self
                        .syntax
                        .secrecy_builtin_closure(source.secrecy_instance(), name);
                    let builtin = crate::builtins::Builtin::from_name(name)
                        .or_else(|| closure.as_ref().map(|(builtin, _)| *builtin));
                    if let Some((_, captures)) = closure {
                        pending
                            .extend(captures.into_iter().map(|capture| (capture, owner.clone())));
                    }
                    dependencies.observations.strings |= builtin
                        == Some(crate::builtins::Builtin::Function(
                            crate::builtins::BuiltinFunction::map,
                        ));
                    dependencies.observations.list_shapes |= builtin
                        == Some(crate::builtins::Builtin::Function(
                            crate::builtins::BuiltinFunction::flatten,
                        ));
                }
            }
            if let ExprKind::Binding {
                binding,
                authored_binding: _,
            }
            | ExprKind::Reference {
                binding,
                authored_binding: _,
                attribute: _,
                segments: _,
            } = source.secrecy_kind()
                && let Some(definition) = self.syntax.secrecy_definition(binding)
            {
                pending.push((definition.clone(), owner.clone()));
            }
            pending.extend(
                source
                    .secrecy_children()
                    .iter()
                    .cloned()
                    .map(|child| (child, owner.clone())),
            );
        }
        self.cache_function_observations(graph);
        self.function_observations.borrow()[&identity]
    }

    fn value_function_observations(
        &self,
        identity: &FunctionIdentity<B>,
        function: &Arc<crate::parser::UserFunction<B>>,
        functions: &IndexMap<String, crate::parser::UserFunction<B>>,
    ) -> ArgumentDependencies {
        if let Some(uses) = self.function_observations.borrow().get(identity) {
            return *uses;
        }
        let FunctionIdentity::Value(_, module) = identity else {
            unreachable!("value function")
        };
        let mut pending = vec![function.clone()];
        let mut seen = HashSet::new();
        let mut graph = IndexMap::new();
        while let Some(function) = pending.pop() {
            let owner = FunctionIdentity::Value(function.body.location.clone(), module.clone());
            if !seen.insert(owner.clone())
                || self.function_observations.borrow().contains_key(&owner)
            {
                continue;
            }
            let mut dependencies = FunctionDependencies::default();
            let mut values = function
                .params
                .iter()
                .filter_map(|parameter| parameter.default.as_ref())
                .chain(function.local_lets.iter().map(|(_, value, _)| value))
                .chain(std::iter::once(&function.body.value))
                .collect::<Vec<_>>();
            while let Some(value) = values.pop() {
                match value {
                    Value::Concrete(ConcreteValue::List(items)) => values.extend(items),
                    Value::Concrete(ConcreteValue::Map(fields)) => values.extend(fields.values()),
                    Value::Deferred(
                        DeferredValue::FunctionCall { name, args, .. }
                        | DeferredValue::Unavailable { name, args, .. },
                    ) => {
                        dependencies.observations.strings |=
                            *name == crate::builtins::BuiltinFunction::map;
                        dependencies.observations.list_shapes |=
                            *name == crate::builtins::BuiltinFunction::flatten;
                        values.extend(args.iter().filter_map(FunctionArgument::value));
                    }
                    Value::Deferred(DeferredValue::ResolvedFunctionCall {
                        function, args, ..
                    }) => {
                        let callee = functions
                            .get(&function.name)
                            .cloned()
                            .map(Arc::new)
                            .unwrap_or_else(|| function.clone());
                        dependencies.callees.insert(FunctionIdentity::Value(
                            callee.body.location.clone(),
                            module.clone(),
                        ));
                        pending.push(callee);
                        values.extend(args.iter().map(FunctionArgument::walker_value));
                    }
                    Value::Deferred(DeferredValue::Secret(secret)) => {
                        values.push(secret.expose_plaintext())
                    }
                    Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                        values.push(value);
                        values.push(default);
                    }
                    Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
                        values.push(expression.lhs());
                        values.push(expression.rhs());
                    }
                    Value::Deferred(DeferredValue::Interpolation(parts)) => {
                        values.extend(parts.iter().filter_map(|part| match part {
                            InterpolationPart::Expr(value) => Some(value),
                            InterpolationPart::Literal(_) => None,
                        }))
                    }
                    Value::Deferred(DeferredValue::Access(access)) => values.push(access.base()),
                    Value::Deferred(DeferredValue::ModuleIf(conditional)) => {
                        dependencies.observations.booleans = true;
                        values.extend(conditional.values());
                    }
                    Value::Concrete(
                        ConcreteValue::String(_)
                        | ConcreteValue::StringList(_)
                        | ConcreteValue::Int(_)
                        | ConcreteValue::Float(_)
                        | ConcreteValue::Bool(_)
                        | ConcreteValue::Duration(_)
                        | ConcreteValue::EnumIdentifier(_)
                        | ConcreteValue::CanonicalEnum(_),
                    )
                    | Value::Deferred(
                        DeferredValue::ResourceRef { .. }
                        | DeferredValue::BindingRef { .. }
                        | DeferredValue::Placeholder(_)
                        | DeferredValue::Unknown(_),
                    ) => {}
                }
            }
            graph.insert(owner, dependencies);
        }
        self.cache_function_observations(graph);
        self.function_observations.borrow()[identity]
    }

    fn declaration_index(&self) -> &DeclarationIndex {
        self.declaration_index.get_or_init(|| {
            #[cfg(test)]
            DECLARATION_INDEX_OPERATIONS.with(|counts| {
                let (builds, lookups) = counts.get();
                counts.set((builds + 1, lookups));
            });
            let mut index = DeclarationIndex::new();
            for (position, declaration) in self.declarations.iter().enumerate() {
                index
                    .entry((
                        declaration.location.file().clone(),
                        self.syntax.secrecy_call(&declaration.identity.0).cloned(),
                    ))
                    .or_default()
                    .declarations
                    .push(position);
            }
            for entries in index.values_mut() {
                entries.declarations.sort_by_key(|position| {
                    let span = self.declarations[*position].location.span();
                    (span.start_byte(), std::cmp::Reverse(span.end_byte()))
                });
                let base = entries.declarations.len().next_power_of_two();
                entries.max_ends.resize(base * 2, 0);
                for (index, position) in entries.declarations.iter().enumerate() {
                    entries.max_ends[base + index] =
                        self.declarations[*position].location.span().end_byte();
                }
                for index in (1..base).rev() {
                    entries.max_ends[index] =
                        entries.max_ends[index * 2].max(entries.max_ends[index * 2 + 1]);
                }
            }
            index
        })
    }

    fn declaration(
        &self,
        location: &DeclarationLocation,
        call: Option<&CallSite>,
    ) -> Option<&Declaration> {
        #[cfg(test)]
        DECLARATION_INDEX_OPERATIONS.with(|counts| {
            let (builds, lookups) = counts.get();
            counts.set((builds, lookups + 1));
        });
        let entries = self
            .declaration_index()
            .get(&(location.file().clone(), call.cloned()))?;
        let span = location.span();
        let end = entries.declarations.partition_point(|position| {
            self.declarations[*position].location.span().start_byte() <= span.start_byte()
        });
        let mut best: Option<usize> = None;
        let mut pending = vec![(1, 0, entries.max_ends.len() / 2)];
        while let Some((node, left, right)) = pending.pop() {
            if left >= end || entries.max_ends[node] < span.end_byte() {
                continue;
            }
            if right - left == 1 {
                #[cfg(test)]
                DECLARATION_SPAN_CHECKS.with(|checks| checks.set(checks.get() + 1));
                let position = entries.declarations[left];
                let rank = |position: usize| {
                    let span = self.declarations[position].location.span();
                    (span.end_byte() - span.start_byte(), position)
                };
                if best.is_none_or(|previous| rank(position) < rank(previous)) {
                    best = Some(position);
                }
            } else {
                let middle = (left + right) / 2;
                pending.push((node * 2, left, middle));
                pending.push((node * 2 + 1, middle, right));
            }
        }
        best.map(|position| &self.declarations[position])
    }

    /// Only context-independent syntax may share evidence between attributes.
    /// This classification is itself memoized, so a large literal is indexed
    /// once rather than scanned again at each use. Reference memos stay local.
    fn is_reference_free(&self, expression: &ExprValue<ParserPlaceholder, B>) -> bool {
        let key = |source: &ExprValue<ParserPlaceholder, B>| {
            (source.secrecy_instance().clone(), source.node_id())
        };
        let mut pending = vec![(expression, false)];
        let mut known = self.reference_free.borrow_mut();
        while let Some((source, children_done)) = pending.pop() {
            if known.contains_key(&key(source)) {
                continue;
            }
            let independent = match source.secrecy_kind() {
                ExprKind::Binding { binding, authored_binding: _ } => {
                    !self.resources.contains_key(binding)
                        && self.syntax.secrecy_definition(binding).is_none()
                        && self.syntax.secrecy_argument(binding).is_none()
                        && !self.upstream.contains(binding)
                        && self.syntax.secrecy_literal(source)
                }
                ExprKind::Reference { binding, attribute, authored_binding: _, segments: _ } => {
                    !self.resources.contains_key(binding)
                        && self.syntax.secrecy_definition(binding).is_none()
                        && self.syntax.secrecy_argument(binding).is_none()
                        && !self.upstream.contains(binding)
                        && self.syntax.secrecy_literal(source)
                        && self.syntax.secrecy_output(binding, attribute).is_none()
                }
                ExprKind::Parameter(_)
                | ExprKind::UntypedParameter
                | ExprKind::LoopSlot(_)
                // Loop instances share authored nodes but retain distinct
                // condition snapshots. Cache their pure branches, never the
                // instance-dependent branch selection itself.
                | ExprKind::If
                | ExprKind::Function { .. }
                | ExprKind::Resource { .. }
                | ExprKind::For
                | ExprKind::ModuleCall
                | ExprKind::ProviderDeclaration
                | ExprKind::RequireCondition(_)
                | ExprKind::Compose
                | ExprKind::Unsupported(_) => false,
                ExprKind::Call(name) => {
                    self.syntax
                        .secrecy_function(source.secrecy_instance(), name)
                        .is_none()
                        && crate::builtins::Builtin::from_name(name).is_some()
                }
                ExprKind::Scalar(_)
                | ExprKind::EmptyInterpolation
                | ExprKind::Secret
                | ExprKind::List
                | ExprKind::Map(_)
                | ExprKind::Coalesce
                | ExprKind::Sequence
                | ExprKind::Sink { .. }
                | ExprKind::Access(_)
                | ExprKind::Alias
                | ExprKind::Parens
                | ExprKind::Definition
                | ExprKind::Convert { .. }
                | ExprKind::NestedBlock { .. }
                | ExprKind::Wait
                | ExprKind::Iterable
                | ExprKind::Interpolation { .. }
                | ExprKind::Integer { .. }
                | ExprKind::Boolean
                | ExprKind::DynamicAccess
                | ExprKind::Declaration
                | ExprKind::Element => true,
            };
            if !independent {
                known.insert(key(source), false);
            } else if children_done {
                let independent = source
                    .secrecy_children()
                    .iter()
                    .all(|child| known[&key(child)]);
                known.insert(key(source), independent);
            } else {
                pending.push((source, true));
                pending.extend(source.secrecy_children().iter().map(|child| (child, false)));
            }
        }
        known[&key(expression)]
    }
}

pub(crate) fn derive<E, P, T, D, B>(
    file: &File<E, P, T, D, B>,
    syntax: &ExpressionReport<B>,
    schemas: &SchemaRegistry,
) -> SecrecyReport
where
    P: crate::resource::PlaceholderKind,
    T: crate::resource::PlaceholderKind,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
{
    let mut owner = Owner {
        syntax,
        schemas,
        resources: IndexMap::new(),
        declarations: Vec::new(),
        declaration_index: OnceLock::new(),
        reference_free: RefCell::default(),
        literal_evidence: RefCell::default(),
        function_evidence: RefCell::default(),
        function_observations: RefCell::default(),
        argument_environments: RefCell::default(),
        empty_environment: Arc::default(),
        upstream: file
            .upstream_states
            .iter()
            .map(|upstream| upstream.binding.clone())
            .collect(),
    };
    let mut report = SecrecyReport::default();
    // Standalone module templates have no call-site substitutions.
    if crate::validation::is_module_definition(file)
        && matches!(syntax.secrecy_instance(), ExpressionInstance::Root)
    {
        report.pending = true;
        return report;
    }
    // Register authored owners before any dependency walk. A finding reached
    // through another declaration retains its own kind, name and parser ID.
    for resource in syntax.secrecy_resources() {
        for sink in resource.secrecy_children() {
            if let ExprKind::Sink { name } = sink.secrecy_kind() {
                owner.declarations.push(Declaration {
                    identity: (sink.secrecy_instance().clone(), sink.node_id()),
                    kind: DeclarationKind::Attribute,
                    name: name.to_string(),
                    location: sink.location().clone(),
                });
            }
        }
    }
    for (name, source) in syntax.secrecy_arguments_authored() {
        owner.declarations.push(Declaration {
            identity: (source.secrecy_instance().clone(), source.node_id()),
            kind: DeclarationKind::ModuleArgument,
            name,
            location: source.location().clone(),
        });
    }
    for call in &file.module_calls {
        for (name, _) in call.arguments() {
            if let Some(location) = call.id().argument_value_location(name)
                && let Some(source) = syntax.secrecy_at(location)
            {
                owner.declarations.push(Declaration {
                    identity: (source.secrecy_instance().clone(), source.node_id()),
                    kind: DeclarationKind::ModuleArgument,
                    name: name.to_string(),
                    location: location.clone(),
                });
            }
        }
    }
    for declaration in file.parser_binding_metadata.syntax().declarations() {
        if let Some(kind) = DeclarationKind::from_syntax(declaration.detail().kind())
            && let Some(location) = declaration.detail().value_location()
        {
            for source in syntax.secrecy_at_instances(location) {
                owner.declarations.push(Declaration {
                    identity: (source.secrecy_instance().clone(), source.node_id()),
                    kind,
                    name: declaration.detail().name().to_string(),
                    location: location.clone(),
                });
            }
        }
    }
    for function in syntax.secrecy_functions() {
        if let ExprKind::Function { name, signature: _ } = function.secrecy_kind() {
            owner.declarations.push(Declaration {
                identity: (function.secrecy_instance().clone(), function.node_id()),
                kind: DeclarationKind::Function,
                name: name.clone(),
                location: function.location().clone(),
            });
        }
    }
    owner.declaration_index();
    for resource in file
        .iter_all_resources()
        .chain(file.deferred_for_phase.analysis_resource_refs())
    {
        let (slots, kind) = match resource {
            ResourceRef::Resource(resource) => (&resource.authored_slots, SchemaKind::Resource),
            ResourceRef::DataSource(source) => (&source.authored_slots, SchemaKind::DataSource),
            ResourceRef::Deferred {
                resource,
                deferred: _,
            } => (&resource.as_resource().authored_slots, SchemaKind::Resource),
            ResourceRef::LiveComposition(_) => continue,
        };
        if schemas
            .get(&resource.id().provider, &resource.id().resource_type, kind)
            .is_none()
        {
            report.pending = true;
        }
        if let Some(binding) = resource.binding_address() {
            let attributes = slots
                .iter()
                .filter_map(|(name, slot)| {
                    syntax
                        .secrecy_root(resource.id(), slot.secrecy_node())
                        .map(|source| {
                            (
                                name.transport_identity().to_string(),
                                Thunk {
                                    environment: Environment::for_slot(
                                        slot,
                                        &source,
                                        &owner.empty_environment,
                                    ),
                                    source: Source::Syntax(source),
                                },
                            )
                        })
                })
                .collect();
            owner.resources.insert(
                binding.clone(),
                ResourceSource {
                    provider: resource.id().provider.clone(),
                    resource_type: resource.id().resource_type.clone(),
                    kind,
                    attributes,
                },
            );
        }
    }
    let mut seen = HashSet::new();
    let mut declarations = HashMap::new();
    for resource in file
        .iter_all_resources()
        .chain(file.deferred_for_phase.analysis_resource_refs())
    {
        let (slots, attribute_count, template) = match resource {
            ResourceRef::Resource(resource) => {
                (&resource.authored_slots, resource.attributes.len(), false)
            }
            ResourceRef::Deferred {
                resource,
                deferred: _,
            } => (
                &resource.as_resource().authored_slots,
                resource.attributes().len(),
                true,
            ),
            ResourceRef::DataSource(_) | ResourceRef::LiveComposition(_) => continue,
        };
        // A repeated traversal of a template is harmless, but two distinct
        // authored declarations must never be paired by a first-wins map.
        let Some(declaration) = slots.declaration_id().cloned() else {
            report.pending = true;
            continue;
        };
        if !seen.insert((resource.id().clone(), declaration.clone())) {
            continue;
        }
        let instance = slots.instance();
        if let Some(previous) =
            declarations.insert((declaration.clone(), instance), resource.id().clone())
            && previous != *resource.id()
        {
            report.pending = true;
            Arc::make_mut(&mut report.rows).shift_remove(&previous);
            continue;
        }
        if report.rows.contains_key(resource.id()) {
            report.pending = true;
            Arc::make_mut(&mut report.rows).shift_remove(resource.id());
            continue;
        }
        let mut attributes = IndexMap::new();
        for (name, slot) in slots.iter() {
            let Some(source) = syntax.secrecy_root(resource.id(), slot.secrecy_node()) else {
                report.pending = true;
                continue;
            };
            let location = syntax
                .secrecy_attribute_location(resource.id(), slot.secrecy_node())
                .unwrap_or_else(|| source.location().clone());
            let environment = resource
                .binding_address()
                .and_then(|binding| owner.resources.get(binding))
                .and_then(|resource| resource.attributes.get(name.transport_identity()))
                .map(|source| source.environment.clone())
                .unwrap_or_else(|| Environment::for_slot(slot, &source, &owner.empty_environment));
            let evidence =
                report.inspect_source(&owner, name.to_string(), location, source, environment);
            match evidence {
                Some(evidence) => {
                    attributes.insert(name.transport_identity().to_string(), evidence);
                }
                None => report.pending = true,
            }
        }
        if attributes.len() != attribute_count {
            report.pending = true;
        }
        if template {
            report.pending = true;
            continue;
        }
        Arc::make_mut(&mut report.rows).insert(
            resource.id().clone(),
            Row {
                declaration,
                instance,
                attributes,
            },
        );
    }
    // Expanded branches still share the original resource syntax. Inspect
    // every authored attribute after the complete caller scope is available,
    // including branches removed by ModuleIf selection.
    for resource in syntax.secrecy_resources() {
        for sink in resource.secrecy_children() {
            if let ExprKind::Sink { name } = sink.secrecy_kind()
                && let Some(value) = sink.secrecy_sink()
            {
                // This pass only adds diagnostics from discarded syntax.
                // Literal leaves cannot add findings or pending evidence.
                if literal_root(value) {
                    continue;
                }
                report.inspect_source(
                    &owner,
                    name.to_string(),
                    sink.location().clone(),
                    value.clone(),
                    owner.empty_environment.clone(),
                );
            }
        }
    }
    // An observation-resolved module condition cannot grant configuration
    // evidence. Configuration-known arguments remain usable; unknown caller
    // arguments keep their instance pending even if expansion selected a row.
    for condition in syntax.secrecy_conditions() {
        let mut nodes = vec![&condition];
        while let Some(node) = nodes.pop() {
            if let ExprKind::Binding {
                binding,
                authored_binding: _,
            }
            | ExprKind::Reference {
                binding,
                authored_binding: _,
                attribute: _,
                segments: _,
            } = node.secrecy_kind()
                && let Some(argument) = syntax.secrecy_argument(binding)
            {
                let walk = Walk::new(&owner, String::new(), condition.location().clone());
                let configured = argument
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .ok()
                    .and_then(|proof| proof.into_classification().into_known().ok())
                    .is_some();
                if !configured
                    && walk
                        .known_boolean(&Thunk {
                            source: Source::Value(Arc::new(argument.clone())),
                            environment: owner.empty_environment.clone(),
                        })
                        .is_none()
                {
                    report.pending = true;
                }
            }
            nodes.extend(node.secrecy_children());
        }
    }

    // Call arguments survive expansion as authored uses even when no module
    // call remains in the runtime file. Recheck them in the complete scope.
    for (name, source) in syntax.secrecy_arguments_authored() {
        report.inspect_source(
            &owner,
            name,
            source.location().clone(),
            source,
            owner.empty_environment.clone(),
        );
    }

    // Call arguments, value lets and outputs also retain their pre-folding
    // syntax. Checking these roots prevents a Public-left coalesce from being
    // hidden by call-site substitution or an unused declaration.
    for call in &file.module_calls {
        for (name, _) in call.arguments() {
            if let Some(location) = call.id().argument_value_location(name)
                && let Some(source) = syntax.secrecy_at(location)
            {
                report.inspect_source(
                    &owner,
                    name.to_string(),
                    location.clone(),
                    source,
                    owner.empty_environment.clone(),
                );
            }
        }
    }
    for declaration in file.parser_binding_metadata.syntax().declarations() {
        if matches!(
            declaration.detail().kind(),
            crate::config_loader::DeclarationKind::Binding
                | crate::config_loader::DeclarationKind::Argument
                | crate::config_loader::DeclarationKind::ModuleAttribute
                | crate::config_loader::DeclarationKind::Export
        ) && let Some(location) = declaration.detail().value_location()
        {
            for source in syntax.secrecy_at_instances(location) {
                if syntax.secrecy_is_structural(&source) {
                    continue;
                }
                report.inspect_source(
                    &owner,
                    declaration.detail().name().to_string(),
                    location.clone(),
                    source,
                    owner.empty_environment.clone(),
                );
            }
        }
    }
    if syntax.has_failed_expressions() {
        report.pending = true;
    }
    report
}

// All dependency following, including function calls, runs on this heap stack.
// The reference limit is a semantic error, never a guard on the Rust stack.
enum Work<B: BindingAddress> {
    Enter {
        thunk: Thunk<B>,
        path: Vec<PathSegment>,
        depth: usize,
    },
    Finish {
        operation: Operation,
        count: usize,
        path: Vec<PathSegment>,
    },
    Reference {
        key: ReferenceKey<B>,
        depth: usize,
        errors: usize,
        max_before: usize,
        pending_before: usize,
    },
    Literal {
        identity: DeclarationIdentity,
        errors: usize,
        path: Vec<PathSegment>,
    },
    Function {
        identity: (ExpressionInstance, NodeId),
    },
    Call {
        function: ExprValue<ParserPlaceholder, B>,
        count: usize,
        observations: Vec<ArgumentObservations>,
        arguments: Vec<Thunk<B>>,
        environment: Arc<Environment<B>>,
        path: Vec<PathSegment>,
        depth: usize,
    },
    ValueCall {
        function: Arc<crate::parser::UserFunction<B>>,
        functions: Arc<IndexMap<String, crate::parser::UserFunction<B>>>,
        count: usize,
        observations: Vec<ArgumentObservations>,
        arguments: Vec<Thunk<B>>,
        environment: Arc<Environment<B>>,
        path: Vec<PathSegment>,
        depth: usize,
    },
    ValueFunction {
        location: DeclarationLocation,
    },
    ReplayReference {
        effect: ReferenceEffect<B>,
        depth: usize,
    },
    ReferenceReplayFinished {
        environments: Arc<BTreeMap<usize, usize>>,
        activation: FunctionActivation,
    },
    FunctionResult(Option<Evidence>),
    MemoFunction(FunctionIdentity<B>),
    Activation(FunctionActivation),
}
#[derive(Clone, Default, PartialEq, Eq)]
struct FunctionActivation {
    syntax: IndexSet<DeclarationIdentity>,
    values: BTreeSet<DeclarationLocation>,
}

impl Hash for FunctionActivation {
    fn hash<H: Hasher>(&self, state: &mut H) {
        fn unordered<'a, T: Hash + 'a, H: Hasher>(
            values: impl ExactSizeIterator<Item = &'a T>,
            state: &mut H,
        ) {
            values.len().hash(state);
            let hash = values.fold(0u64, |sum, value| {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                value.hash(&mut hash);
                sum.wrapping_add(hash.finish())
            });
            hash.hash(state);
        }
        unordered(self.syntax.iter(), state);
        unordered(self.values.iter(), state);
    }
}

impl FunctionActivation {
    fn added_since(&self, base: &Self) -> Self {
        Self {
            syntax: self.syntax.difference(&base.syntax).cloned().collect(),
            values: self.values.difference(&base.values).cloned().collect(),
        }
    }

    fn with(&self, additions: Self) -> Self {
        let mut activation = self.clone();
        activation.syntax.extend(additions.syntax);
        activation.values.extend(additions.values);
        activation
    }
}
enum Operation {
    Propagate,
    Secret,
    List,
    Map(Vec<String>),
    Last,
    Alternatives {
        operator: Alternatives,
        origin: Option<(DeclarationLocation, Option<CallSite>)>,
    },
    If {
        selected: Option<usize>,
    },
    Selection {
        path: Vec<PathSegment>,
    },
    Key,
    Pending,
    Concat,
    Flatten(Option<Vec<bool>>),
    Keys,
    Values,
    Length,
    Project(Option<String>),
}

fn builtin_operation(
    name: &str,
    origin: Option<(DeclarationLocation, Option<CallSite>)>,
    accessor: Option<String>,
    flatten_non_lists: Option<Vec<bool>>,
) -> Operation {
    use crate::builtins::{Builtin, BuiltinFunction};
    let Some(builtin) = Builtin::from_name(name) else {
        return Operation::Pending;
    };
    match builtin {
        Builtin::Secret => Operation::Secret,
        Builtin::Function(function) => match function {
            BuiltinFunction::decrypt => Operation::Secret,
            BuiltinFunction::lookup => Operation::Alternatives {
                operator: Alternatives::Lookup,
                origin,
            },
            BuiltinFunction::concat => Operation::Concat,
            BuiltinFunction::flatten => Operation::Flatten(flatten_non_lists),
            BuiltinFunction::keys => Operation::Keys,
            BuiltinFunction::values => Operation::Values,
            BuiltinFunction::length => Operation::Length,
            BuiltinFunction::map => Operation::Project(accessor),
            // Scalar builtins unwrap direct Secret inputs and protect the
            // entire result (builtins::evaluate_secret_arguments). join also
            // protects the result when any list element is Secret.
            BuiltinFunction::cidr_subnet
            | BuiltinFunction::env
            | BuiltinFunction::join
            | BuiltinFunction::lower
            | BuiltinFunction::max
            | BuiltinFunction::min
            | BuiltinFunction::replace
            | BuiltinFunction::split
            | BuiltinFunction::trim
            | BuiltinFunction::upper => Operation::Propagate,
        },
    }
}

struct FunctionContext<'a, B: BindingAddress> {
    environment: Arc<Environment<B>>,
    path: Vec<PathSegment>,
    depth: usize,
    work: &'a mut Vec<Work<B>>,
    values: &'a mut Vec<Option<Evidence>>,
}

struct Walk<'a, B: BindingAddress> {
    owner: &'a Owner<'a, B>,
    attribute: String,
    attribute_location: DeclarationLocation,
    call_site: Option<CallSite>,
    declaration: Option<Declaration>,
    findings: Vec<Finding>,
    visits: usize,
    #[cfg(test)]
    memo_hits: usize,
    #[cfg(test)]
    syntax_entries: usize,
    #[cfg(test)]
    function_body_evaluations: usize,
    #[cfg(test)]
    function_memo_hits: usize,
    max_depth: usize,
    // Visit/depth accounting is private to this declaration walk.
    memo: HashMap<ReferenceKey<B>, Memo>,
    active_references: HashSet<ReferenceKey<B>>,
    function_memo: HashMap<FunctionKey<B>, Arc<FunctionMemo<B>>>,
    function_recordings: Vec<FunctionRecording<B>>,
    reference_environments: Arc<BTreeMap<usize, usize>>,
    activation: FunctionActivation,
    pending_validation: bool,
    pending_events: usize,
    limited: bool,
}
impl<'a, B: BindingAddress> Walk<'a, B> {
    fn new(owner: &'a Owner<'a, B>, attribute: String, location: DeclarationLocation) -> Self {
        Self {
            owner,
            attribute,
            attribute_location: location,
            call_site: None,
            declaration: None,
            findings: Vec::new(),
            visits: 0,
            #[cfg(test)]
            memo_hits: 0,
            #[cfg(test)]
            syntax_entries: 0,
            #[cfg(test)]
            function_body_evaluations: 0,
            #[cfg(test)]
            function_memo_hits: 0,
            max_depth: 0,
            memo: HashMap::new(),
            active_references: HashSet::new(),
            function_memo: HashMap::new(),
            function_recordings: Vec::new(),
            reference_environments: Arc::default(),
            activation: FunctionActivation::default(),
            pending_validation: false,
            pending_events: 0,
            limited: false,
        }
    }
    fn in_source(mut self, source: &ExprValue<ParserPlaceholder, B>) -> Self {
        if literal_root(source) {
            return self;
        }
        self.call_site = self
            .owner
            .syntax
            .secrecy_call(source.secrecy_instance())
            .cloned();
        self.declaration = Some(
            self.owner
                .declaration(&self.attribute_location, self.call_site.as_ref())
                .cloned()
                .unwrap_or_else(|| Declaration {
                    identity: (source.secrecy_instance().clone(), source.node_id()),
                    kind: DeclarationKind::Attribute,
                    name: self.attribute.clone(),
                    location: self.attribute_location.clone(),
                }),
        );
        self
    }
    fn finding(
        &mut self,
        location: DeclarationLocation,
        call: Option<CallSite>,
        cause: DerivationError,
    ) {
        // Count every occurrence so the reference memo gate also sees errors
        // already reported elsewhere in this walk. The report deduplicates.
        let declaration = self
            .owner
            .declaration(&location, call.as_ref())
            .or(self.declaration.as_ref())
            .expect("finding has an authored declaration");
        self.findings.push(Finding {
            declaration: declaration.identity.clone(),
            kind: declaration.kind,
            attribute: declaration.name.clone(),
            location,
            call_site: call,
            cause,
        });
    }
    fn limit(&mut self, kind: LimitKind) {
        if !self.limited {
            self.limited = true;
            self.finding(
                self.attribute_location.clone(),
                self.call_site.clone(),
                DerivationError::LimitReached(LimitReached { kind }),
            );
        }
    }
    fn run(&mut self, thunk: Thunk<B>) -> Option<Evidence> {
        if let Source::Syntax(expression) = &thunk.source
            && thunk.environment.nodes.is_empty()
            && literal_root(expression)
        {
            return Some(Evidence::Leaf(Secrecy::Public));
        }
        let mut work = vec![Work::Enter {
            thunk,
            path: Vec::new(),
            depth: 0,
        }];
        let mut values = Vec::new();
        while let Some(item) = work.pop() {
            if self.limited {
                return None;
            }
            match item {
                Work::Enter {
                    mut thunk,
                    path,
                    depth,
                } => {
                    // Tail-enter transparent syntax instead of scheduling a
                    // finish frame for every parser wrapper. Substituted
                    // parameter nodes retain their own dispatch below.
                    while let Source::Syntax(expression) = &thunk.source {
                        if !thunk.environment.nodes.contains_key(&(
                            expression.secrecy_instance().clone(),
                            expression.node_id(),
                        )) && matches!(
                            expression.secrecy_kind(),
                            ExprKind::Alias
                                | ExprKind::Parens
                                | ExprKind::Definition
                                | ExprKind::Declaration
                        ) && expression.secrecy_children().len() == 1
                        {
                            thunk.source = Source::Syntax(expression.secrecy_children()[0].clone());
                        } else {
                            break;
                        }
                    }
                    if let Source::Syntax(expression) = &thunk.source
                        && thunk.environment.nodes.is_empty()
                        && literal_root(expression)
                    {
                        values.push(Some(Evidence::Leaf(Secrecy::Public)));
                        continue;
                    }
                    if let Source::Syntax(expression) = &thunk.source
                        && self.owner.is_reference_free(expression)
                    {
                        let identity =
                            (expression.secrecy_instance().clone(), expression.node_id());
                        if let Some(evidence) = self.owner.literal_evidence.borrow().get(&identity)
                        {
                            values.push(evidence.select(&path));
                            continue;
                        }
                        work.push(Work::Literal {
                            identity,
                            errors: self.findings.len(),
                            path,
                        });
                        self.enter(thunk, Vec::new(), depth, &mut work, &mut values);
                    } else {
                        self.enter(thunk, path, depth, &mut work, &mut values);
                    }
                }
                Work::Literal {
                    identity,
                    errors,
                    path,
                } => {
                    if let Some(value) = values.last_mut() {
                        if errors == self.findings.len()
                            && let Some(evidence) = value.as_ref()
                        {
                            self.owner
                                .literal_evidence
                                .borrow_mut()
                                .insert(identity, evidence.clone());
                        }
                        *value = value.as_ref().and_then(|evidence| evidence.select(&path));
                    }
                }
                Work::Finish {
                    operation,
                    count,
                    path,
                } => {
                    let start = values.len() - count;
                    let result = self.finish(operation, &mut values[start..]);
                    values.truncate(start);
                    values.push(result.and_then(|value| value.select(&path)));
                }
                Work::Reference {
                    key,
                    depth,
                    errors,
                    max_before,
                    pending_before,
                } => {
                    self.active_references.remove(&key);
                    if errors == self.findings.len()
                        && pending_before == self.pending_events
                        && let Some(Some(evidence)) = values.last()
                    {
                        self.memo.insert(
                            key,
                            Memo {
                                evidence: evidence.clone(),
                                relative_depth: self.max_depth.saturating_sub(depth),
                            },
                        );
                    }
                    self.max_depth = self.max_depth.max(max_before);
                }
                Work::Function { identity } => {
                    self.finish_function(&values);
                    self.activation.syntax.shift_remove(&identity);
                }
                Work::Call {
                    function,
                    count,
                    observations,
                    arguments: authored_arguments,
                    environment,
                    path,
                    depth,
                } => {
                    let start = values.len() - count;
                    let activation = Arc::new(self.activation.clone());
                    let arguments = values
                        .drain(start..)
                        .enumerate()
                        .map(|(index, value)| Thunk {
                            source: Source::Argument {
                                authored: Box::new(authored_arguments[index].clone()),
                                facts: self.argument_facts(
                                    &authored_arguments[index],
                                    value,
                                    observations[index].clone(),
                                ),
                                activation: activation.clone(),
                            },
                            environment: environment.clone(),
                        })
                        .collect();
                    self.begin_function(
                        function,
                        arguments,
                        FunctionContext {
                            environment,
                            path,
                            depth,
                            work: &mut work,
                            values: &mut values,
                        },
                    );
                }
                Work::ValueCall {
                    function,
                    functions,
                    count,
                    observations,
                    arguments: authored_arguments,
                    environment,
                    path,
                    depth,
                } => {
                    let arguments = values.split_off(values.len() - count);
                    self.begin_value_function(
                        function,
                        functions,
                        arguments,
                        observations,
                        authored_arguments,
                        FunctionContext {
                            environment,
                            path,
                            depth,
                            work: &mut work,
                            values: &mut values,
                        },
                    );
                }
                Work::ValueFunction { location } => {
                    self.finish_function(&values);
                    self.activation.values.remove(&location);
                }
                Work::ReplayReference { effect, depth } => {
                    let activation = match effect.activation {
                        ReferenceActivation::Body(additions) => self.activation.with(additions),
                        ReferenceActivation::Argument(activation) => activation,
                    };
                    work.push(Work::ReferenceReplayFinished {
                        environments: std::mem::replace(
                            &mut self.reference_environments,
                            effect.environments,
                        ),
                        activation: std::mem::replace(&mut self.activation, activation),
                    });
                    self.reference(
                        effect.binding,
                        effect.path,
                        effect.environment,
                        depth + effect.relative_depth,
                        &mut work,
                        &mut values,
                    );
                }
                Work::ReferenceReplayFinished {
                    environments,
                    activation,
                } => {
                    values.pop();
                    self.reference_environments = environments;
                    self.activation = activation;
                }
                Work::FunctionResult(evidence) => values.push(evidence),
                Work::MemoFunction(identity) => match identity {
                    FunctionIdentity::Syntax(identity) => {
                        self.activation.syntax.shift_remove(&identity);
                    }
                    FunctionIdentity::Value(location, _) => {
                        self.activation.values.remove(&location);
                    }
                },
                Work::Activation(activation) => self.activation = activation,
            }
        }
        values.pop().flatten()
    }
    fn schedule<C>(
        &mut self,
        operation: Operation,
        children: C,
        mut path: Vec<PathSegment>,
        depth: usize,
        work: &mut Vec<Work<B>>,
    ) where
        C: IntoIterator<Item = Thunk<B>>,
        C::IntoIter: ExactSizeIterator + DoubleEndedIterator,
    {
        let children = children.into_iter();
        // Expression operations preserve the number of references followed.
        let child_depth = match &operation {
            Operation::Propagate
            | Operation::List
            | Operation::Map(_)
            | Operation::Alternatives {
                operator: _,
                origin: _,
            }
            | Operation::If { selected: _ }
            | Operation::Selection { path: _ }
            | Operation::Key
            | Operation::Secret
            | Operation::Last
            | Operation::Pending => depth,
            Operation::Concat
            | Operation::Flatten(_)
            | Operation::Keys
            | Operation::Values
            | Operation::Length
            | Operation::Project(_) => depth,
        };
        // Transparent wrappers must pass selection to their result child.
        // A resource argument has no whole-value evidence; its authored path
        // can only be followed once that child receives the requested fields.
        let mut result_path = if matches!(&operation, Operation::Last) {
            std::mem::take(&mut path)
        } else {
            Vec::new()
        };
        work.push(Work::Finish {
            operation,
            count: children.len(),
            path,
        });
        work.extend(children.rev().map(|thunk| Work::Enter {
            thunk,
            // The reversed iterator visits the result child first.
            path: std::mem::take(&mut result_path),
            depth: child_depth,
        }));
    }
    fn reference(
        &mut self,
        binding: B,
        path: Vec<PathSegment>,
        environment: Arc<Environment<B>>,
        depth: usize,
        work: &mut Vec<Work<B>>,
        values: &mut Vec<Option<Evidence>>,
    ) {
        for recording in &mut self.function_recordings {
            if recording.reference_nesting == self.active_references.len() {
                recording.references.push(ReferenceEffect {
                    binding: binding.clone(),
                    path: path.clone(),
                    environment: environment.clone(),
                    relative_depth: depth - recording.depth,
                    environments: self.reference_environments.clone(),
                    // Parameter path re-entry runs in the argument's authored
                    // activation. Body references retain the current caller
                    // and any nested callees instead of capturing old callers.
                    activation: if recording.key.identity.active_in(&self.activation) {
                        ReferenceActivation::Body(
                            self.activation.added_since(&recording.activation),
                        )
                    } else {
                        ReferenceActivation::Argument(self.activation.clone())
                    },
                });
            }
        }
        self.visits += 1;
        if self.visits > MAX_REFERENCE_VISITS {
            self.limit(LimitKind::ReferenceVisits);
            values.push(None);
            return;
        }
        let next_depth = depth_after_reference(depth);
        if next_depth >= MAX_REFERENCE_DEPTH {
            self.max_depth = self.max_depth.max(next_depth);
            self.limit(LimitKind::Depth);
            values.push(None);
            return;
        }
        let source = self
            .owner
            .reference_source(&binding, &path, environment.clone());
        let source_environment = match &source {
            ReferenceSource::Source(thunk, _) => thunk.environment.id,
            ReferenceSource::Public | ReferenceSource::Handle | ReferenceSource::Pending => {
                environment.id
            }
        };
        let key = ReferenceKey {
            environment: self.reference_environment(source_environment),
            binding: binding.clone(),
            path: path.clone(),
        };
        let memo = self.memo.get(&key).cloned();
        let reached_depth = next_depth + memo.as_ref().map_or(0, |memo| memo.relative_depth);
        self.max_depth = self.max_depth.max(reached_depth.min(MAX_REFERENCE_DEPTH));
        if let Some(memo) = memo {
            #[cfg(test)]
            {
                self.memo_hits += 1;
            }
            // This encounter was charged before consulting the memo. Reusing
            // a complete subtree does not re-expand its internal DAG edges.
            if reached_depth >= MAX_REFERENCE_DEPTH {
                self.limit(LimitKind::Depth);
                values.push(None);
            } else {
                values.push(Some(memo.evidence));
            }
            return;
        }
        if !self.active_references.insert(key.clone()) {
            // Existing binding admission owns reference-cycle diagnostics.
            self.pending_validation = true;
            self.pending_events += 1;
            values.push(None);
            return;
        }
        match source {
            ReferenceSource::Public => {
                self.active_references.remove(&key);
                values.push(Some(Evidence::Leaf(Secrecy::Public)));
            }
            ReferenceSource::Handle => {
                self.active_references.remove(&key);
                values.push(None);
            }
            ReferenceSource::Pending => {
                self.pending_validation = true;
                self.pending_events += 1;
                self.active_references.remove(&key);
                values.push(None);
            }
            ReferenceSource::Source(source, remaining) => {
                let max_before = self.max_depth;
                self.max_depth = next_depth;
                work.push(Work::Reference {
                    key,
                    depth: next_depth,
                    errors: self.findings.len(),
                    max_before,
                    pending_before: self.pending_events,
                });
                work.push(Work::Enter {
                    thunk: source,
                    path: remaining,
                    depth: next_depth,
                });
            }
        }
    }
    fn enter(
        &mut self,
        thunk: Thunk<B>,
        path: Vec<PathSegment>,
        depth: usize,
        work: &mut Vec<Work<B>>,
        values: &mut Vec<Option<Evidence>>,
    ) {
        self.max_depth = self.max_depth.max(depth);
        if depth >= MAX_REFERENCE_DEPTH {
            self.limit(LimitKind::Depth);
            values.push(None);
            return;
        }
        let Thunk {
            source,
            environment,
        } = thunk;
        match source {
            Source::Argument {
                authored,
                facts,
                activation,
            } => {
                if let Some(proof) = facts.proof {
                    values.push(proof.select(&path));
                } else if path.is_empty() {
                    values.push(None);
                } else {
                    // The argument was authored in the caller. Re-enter it
                    // under that activation, then restore the callee before
                    // resuming its remaining work.
                    work.push(Work::Activation(std::mem::replace(
                        &mut self.activation,
                        (*activation).clone(),
                    )));
                    work.push(Work::Enter {
                        thunk: *authored,
                        path,
                        depth,
                    });
                }
            }
            Source::Syntax(expression) => {
                #[cfg(test)]
                {
                    self.syntax_entries += 1;
                }
                if let Some(parameter) = environment
                    .nodes
                    .get(&(expression.secrecy_instance().clone(), expression.node_id()))
                {
                    work.push(Work::Enter {
                        thunk: parameter.clone(),
                        path,
                        depth,
                    });
                    return;
                }
                let children = || {
                    let environment = environment.clone();
                    expression
                        .secrecy_children()
                        .iter()
                        .map(move |child| Thunk {
                            source: Source::Syntax(child.clone()),
                            environment: environment.clone(),
                        })
                };
                match expression.secrecy_kind() {
                    ExprKind::Binding {
                        binding,
                        authored_binding: _,
                    } => {
                        if self.local_literal(binding, &expression) {
                            values.push(Some(Evidence::Leaf(Secrecy::Public)));
                        } else {
                            self.reference(binding.clone(), path, environment, depth, work, values);
                        }
                    }
                    ExprKind::Reference {
                        binding,
                        attribute,
                        segments,
                        authored_binding: _,
                    } => {
                        if self.local_literal(binding, &expression)
                            && self
                                .owner
                                .syntax
                                .secrecy_output(binding, attribute)
                                .is_none()
                        {
                            values.push(Some(Evidence::Leaf(Secrecy::Public)));
                            return;
                        }
                        let path = std::iter::once(PathSegment::Field {
                            name: attribute.clone(),
                        })
                        .chain(segments.iter().cloned())
                        .chain(path)
                        .collect();
                        self.reference(binding.clone(), path, environment, depth, work, values);
                    }
                    ExprKind::Scalar(_) | ExprKind::EmptyInterpolation => {
                        values.push(Some(Evidence::Leaf(Secrecy::Public)))
                    }
                    ExprKind::Parameter(_) | ExprKind::UntypedParameter => values.push(None),
                    ExprKind::LoopSlot(_) => {
                        self.schedule(Operation::Key, children(), path, depth, work)
                    }
                    ExprKind::Element => self.schedule(
                        Operation::Selection { path },
                        children(),
                        Vec::new(),
                        depth,
                        work,
                    ),
                    ExprKind::Secret => {
                        self.schedule(Operation::Secret, children(), path, depth, work)
                    }
                    ExprKind::List => self.schedule(Operation::List, children(), path, depth, work),
                    ExprKind::Map(keys) => self.schedule(
                        Operation::Map(
                            keys.iter()
                                .map(|key| key.transport_identity().to_string())
                                .collect(),
                        ),
                        children(),
                        path,
                        depth,
                        work,
                    ),
                    ExprKind::Coalesce => self.schedule(
                        Operation::Alternatives {
                            operator: Alternatives::Coalesce,
                            origin: Some((
                                expression.secrecy_operator().clone(),
                                self.owner
                                    .syntax
                                    .secrecy_call(expression.secrecy_instance())
                                    .cloned(),
                            )),
                        },
                        children(),
                        path,
                        depth,
                        work,
                    ),
                    ExprKind::Call(name) => {
                        if let Some(function) = self
                            .owner
                            .syntax
                            .secrecy_function(expression.secrecy_instance(), name)
                        {
                            self.function(
                                function,
                                children().collect(),
                                FunctionContext {
                                    environment,
                                    path,
                                    depth,
                                    work,
                                    values,
                                },
                            );
                        } else {
                            let (builtin, arguments) = self.builtin_arguments(
                                &expression,
                                name,
                                children().collect(),
                                &environment,
                            );
                            let operation = builtin_operation(
                                &builtin,
                                Some((
                                    expression.location().clone(),
                                    self.owner
                                        .syntax
                                        .secrecy_call(expression.secrecy_instance())
                                        .cloned(),
                                )),
                                (builtin == "map")
                                    .then(|| {
                                        arguments
                                            .first()
                                            .and_then(|argument| self.known_string(argument))
                                    })
                                    .flatten(),
                                (builtin == "flatten")
                                    .then(|| {
                                        arguments
                                            .first()
                                            .and_then(|argument| self.non_list_members(argument))
                                    })
                                    .flatten(),
                            );
                            self.schedule(operation, arguments, path, depth, work);
                        }
                    }
                    ExprKind::Sequence => {
                        // Local lets are inlined in retained body references. The
                        // last authored expression supplies the function result.
                        self.schedule(Operation::Last, children(), path, depth, work);
                    }
                    ExprKind::Sink { name: _ } => {
                        if let Some(value) = expression.secrecy_sink() {
                            let mut operands = children().collect::<Vec<_>>();
                            let index = expression
                                .secrecy_children()
                                .iter()
                                .position(|child| child.node_id() == value.node_id())
                                .expect("retained sink owns its value");
                            // Dynamic authored keys also contain expressions.
                            // Check them before returning the value's evidence.
                            let value = operands.remove(index);
                            operands.push(value);
                            self.schedule(Operation::Last, operands, path, depth, work);
                        } else {
                            values.push(None);
                        }
                    }
                    ExprKind::Access(step) => {
                        if let Some(base) = expression.secrecy_children().first() {
                            let path = std::iter::once(step.clone()).chain(path).collect();
                            work.push(Work::Enter {
                                thunk: Thunk {
                                    source: Source::Syntax(base.clone()),
                                    environment,
                                },
                                path,
                                depth,
                            });
                        } else {
                            values.push(None);
                        }
                    }
                    ExprKind::If => {
                        let condition = expression.secrecy_condition();
                        let branches = children()
                            .enumerate()
                            .filter(|(index, _)| Some(*index) != condition)
                            .map(|(_, child)| child)
                            .collect::<Vec<_>>();
                        let selected = condition
                            .and_then(|index| expression.secrecy_children().get(index))
                            .and_then(|condition| {
                                if environment.in_function {
                                    self.known_boolean(&Thunk {
                                        source: Source::Syntax(condition.clone()),
                                        environment: environment.clone(),
                                    })
                                } else {
                                    environment
                                        .conditions
                                        .as_ref()
                                        .and_then(|conditions| conditions.get(condition.node_id()))
                                        .or_else(|| {
                                            self.owner.syntax.secrecy_condition_value(&expression)
                                        })
                                }
                            })
                            .map(|selected| if selected { 0 } else { 1 });
                        self.schedule(Operation::If { selected }, branches, path, depth, work);
                    }
                    ExprKind::Alias
                    | ExprKind::Parens
                    | ExprKind::Definition
                    | ExprKind::Convert {
                        target: _,
                        parsing: _,
                    }
                    | ExprKind::NestedBlock { name: _, header: _ }
                    | ExprKind::Wait
                    | ExprKind::Iterable => {
                        self.schedule(Operation::Last, children(), path, depth, work)
                    }
                    ExprKind::Interpolation {
                        single_reference: _,
                    }
                    | ExprKind::Integer { op: _, location: _ }
                    | ExprKind::Boolean => {
                        self.schedule(Operation::Propagate, children(), path, depth, work)
                    }
                    ExprKind::DynamicAccess => {
                        self.schedule(
                            Operation::Selection { path },
                            children(),
                            Vec::new(),
                            depth,
                            work,
                        );
                    }
                    ExprKind::Declaration => {
                        self.schedule(Operation::Last, children(), path, depth, work)
                    }
                    ExprKind::Function {
                        name: _,
                        signature: _,
                    }
                    | ExprKind::Resource {
                        provider: _,
                        resource_type: _,
                        read: _,
                    }
                    | ExprKind::For
                    | ExprKind::ModuleCall
                    | ExprKind::ProviderDeclaration
                    | ExprKind::RequireCondition(_)
                    | ExprKind::Compose
                    | ExprKind::Unsupported(_) => {
                        self.schedule(Operation::Pending, children(), path, depth, work)
                    }
                }
            }
            Source::Value(value) => {
                self.enter_value(&value, environment, path, depth, work, values)
            }
        }
    }
    fn local_literal(&self, binding: &B, expression: &ExprValue<ParserPlaceholder, B>) -> bool {
        !self.owner.resources.contains_key(binding)
            && self.owner.syntax.secrecy_definition(binding).is_none()
            && self.owner.syntax.secrecy_argument(binding).is_none()
            && !self.owner.upstream.contains(binding)
            && self.owner.syntax.secrecy_literal(expression)
    }

    /// Retained call and pipe syntax names the closure's let binding. Follow
    /// its authored definition and prepend captures before applying builtin
    /// secrecy rules. This also preserves intrinsic secret/decrypt behavior.
    fn builtin_arguments(
        &self,
        expression: &ExprValue<ParserPlaceholder, B>,
        name: &str,
        arguments: Vec<Thunk<B>>,
        environment: &Arc<Environment<B>>,
    ) -> (String, Vec<Thunk<B>>) {
        if crate::builtins::Builtin::from_name(name).is_some() {
            return (name.to_string(), arguments);
        }
        let Some((builtin, captures)) = self
            .owner
            .syntax
            .secrecy_builtin_closure(expression.secrecy_instance(), name)
        else {
            return (name.to_string(), arguments);
        };
        let name = match builtin {
            crate::builtins::Builtin::Secret => "secret",
            crate::builtins::Builtin::Function(function) => function.as_str(),
        };
        let arguments = captures
            .into_iter()
            .map(|source| Thunk {
                source: Source::Syntax(source),
                environment: environment.clone(),
            })
            .chain(arguments)
            .collect();
        (name.to_string(), arguments)
    }

    fn known_boolean(&self, input: &Thunk<B>) -> Option<bool> {
        let mut thunk = input.clone();
        let mut bindings = HashSet::new();
        loop {
            match &thunk.source {
                Source::Argument { facts, .. } => return facts.observations.boolean,
                Source::Value(value) => {
                    let substitution = match value.as_ref() {
                        Value::Deferred(DeferredValue::Placeholder(
                            ParserPlaceholder::ModuleArgument {
                                name,
                                diagnostic: _,
                            },
                        )) => {
                            let binding =
                                crate::resource::binding_address::binding_selected_attribute(
                                    thunk.environment.module.as_ref()?,
                                    name,
                                );
                            let argument = self.owner.syntax.secrecy_argument(&binding)?;
                            Some((binding, argument))
                        }
                        Value::Deferred(DeferredValue::BindingRef { binding }) => self
                            .owner
                            .syntax
                            .secrecy_argument(binding)
                            .map(|argument| (binding.clone(), argument)),
                        _ => None,
                    };
                    if let Some((binding, argument)) = substitution {
                        if !bindings.insert(binding.clone()) {
                            return None;
                        }
                        thunk.source = Source::Value(Arc::new(argument.clone()));
                        thunk.environment = Arc::new(Environment {
                            module: self.owner.syntax.secrecy_argument_module(&binding).cloned(),
                            ..(*thunk.environment).clone()
                        });
                        continue;
                    }
                    if let Value::Deferred(DeferredValue::Placeholder(
                        ParserPlaceholder::FnParam { name }
                        | ParserPlaceholder::FnLocal { name }
                        | ParserPlaceholder::ModuleArgument {
                            name,
                            diagnostic: _,
                        },
                    )) = value.as_ref()
                        && let Some(parameter) = thunk.environment.names.get(name)
                    {
                        thunk = parameter.clone();
                        continue;
                    }
                    let proof = value
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                        .ok()?
                        .into_classification()
                        .into_known()
                        .ok()?;
                    return crate::resource::knowledge::known_bool(&proof);
                }
                Source::Syntax(expression) => {
                    if let Some(parameter) = thunk
                        .environment
                        .nodes
                        .get(&(expression.secrecy_instance().clone(), expression.node_id()))
                    {
                        thunk = parameter.clone();
                        continue;
                    }
                    if matches!(
                        expression.secrecy_kind(),
                        ExprKind::Alias
                            | ExprKind::Parens
                            | ExprKind::Definition
                            | ExprKind::Sequence
                            | ExprKind::Declaration
                    ) && let Some(child) = expression.secrecy_children().last()
                    {
                        thunk.source = Source::Syntax(child.clone());
                        continue;
                    }
                    if thunk.environment.in_function {
                        let mut nodes = vec![expression];
                        while let Some(node) = nodes.pop() {
                            if thunk
                                .environment
                                .nodes
                                .contains_key(&(node.secrecy_instance().clone(), node.node_id()))
                            {
                                return None;
                            }
                            nodes.extend(node.secrecy_children());
                        }
                    }
                    return self.owner.syntax.secrecy_boolean(expression);
                }
            }
        }
    }

    fn non_list_members(&self, input: &Thunk<B>) -> Option<Vec<bool>> {
        let mut thunk = input.clone();
        let mut seen = HashSet::new();
        loop {
            match &thunk.source {
                Source::Argument { facts, .. } => return facts.observations.non_lists.clone(),
                Source::Syntax(expression) => {
                    if !seen.insert((
                        thunk.environment.id,
                        expression.secrecy_instance().clone(),
                        expression.node_id(),
                    )) {
                        return None;
                    }
                    if let Some(parameter) = thunk
                        .environment
                        .nodes
                        .get(&(expression.secrecy_instance().clone(), expression.node_id()))
                    {
                        thunk = parameter.clone();
                    } else if matches!(
                        expression.secrecy_kind(),
                        ExprKind::Alias
                            | ExprKind::Parens
                            | ExprKind::Definition
                            | ExprKind::Declaration
                    ) && expression.secrecy_children().len() == 1
                    {
                        thunk.source = Source::Syntax(expression.secrecy_children()[0].clone());
                    } else if let ExprKind::Binding {
                        binding,
                        authored_binding: _,
                    } = expression.secrecy_kind()
                    {
                        thunk.source =
                            Source::Syntax(self.owner.syntax.secrecy_definition(binding)?.clone());
                    } else if matches!(expression.secrecy_kind(), ExprKind::List) {
                        return Some(
                            expression
                                .secrecy_children()
                                .iter()
                                .map(|item| self.owner.syntax.secrecy_non_list(item))
                                .collect(),
                        );
                    } else {
                        return None;
                    }
                }
                Source::Value(value) => {
                    let Value::Concrete(ConcreteValue::List(items)) = value.as_ref() else {
                        return None;
                    };
                    return Some(
                        items
                            .iter()
                            .map(|item| {
                                matches!(
                                    item,
                                    Value::Concrete(
                                        ConcreteValue::String(_)
                                            | ConcreteValue::Int(_)
                                            | ConcreteValue::Float(_)
                                            | ConcreteValue::Bool(_)
                                            | ConcreteValue::Duration(_)
                                            | ConcreteValue::Map(_)
                                            | ConcreteValue::EnumIdentifier(_)
                                            | ConcreteValue::CanonicalEnum(_)
                                    )
                                )
                            })
                            .collect(),
                    );
                }
            }
        }
    }

    fn known_string(&self, input: &Thunk<B>) -> Option<String> {
        let mut thunk = input.clone();
        let mut seen = HashSet::new();
        loop {
            match &thunk.source {
                Source::Argument { facts, .. } => return facts.observations.string.clone(),
                Source::Value(value) => match value.as_ref() {
                    Value::Concrete(ConcreteValue::String(value)) => {
                        return Some(value.expose_plaintext().to_owned());
                    }
                    Value::Deferred(DeferredValue::Placeholder(
                        ParserPlaceholder::FnParam { name } | ParserPlaceholder::FnLocal { name },
                    )) => {
                        thunk = thunk.environment.names.get(name)?.clone();
                    }
                    _ => return None,
                },
                Source::Syntax(expression) => {
                    let identity = (
                        thunk.environment.id,
                        expression.secrecy_instance().clone(),
                        expression.node_id(),
                    );
                    if !seen.insert(identity) {
                        return None;
                    }
                    if let Some(parameter) = thunk
                        .environment
                        .nodes
                        .get(&(expression.secrecy_instance().clone(), expression.node_id()))
                    {
                        thunk = parameter.clone();
                    } else if matches!(
                        expression.secrecy_kind(),
                        ExprKind::Alias
                            | ExprKind::Parens
                            | ExprKind::Definition
                            | ExprKind::Sequence
                            | ExprKind::Declaration
                    ) {
                        thunk.source =
                            Source::Syntax(expression.secrecy_children().last()?.clone());
                    } else {
                        return self.owner.syntax.secrecy_string(expression);
                    }
                }
            }
        }
    }

    fn reference_environment(&self, id: usize) -> usize {
        self.reference_environments.get(&id).copied().unwrap_or(id)
    }

    fn function_environment(&mut self) -> usize {
        let id = next_environment();
        for recording in &mut self.function_recordings {
            recording.local_environments.insert(id);
        }
        id
    }

    fn argument_facts(
        &self,
        argument: &Thunk<B>,
        proof: Option<Evidence>,
        observations: ArgumentObservations,
    ) -> ArgumentFacts {
        let origin = proof.is_none().then(|| {
            let mut thunk = argument;
            loop {
                match &thunk.source {
                    Source::Argument { facts, .. } => {
                        return facts
                            .origin
                            .clone()
                            .expect("pending argument has an origin");
                    }
                    Source::Syntax(source) => {
                        let mut source = source;
                        loop {
                            if let Some(parameter) = thunk
                                .environment
                                .nodes
                                .get(&(source.secrecy_instance().clone(), source.node_id()))
                            {
                                thunk = parameter;
                                break;
                            }
                            if matches!(
                                source.secrecy_kind(),
                                ExprKind::Alias
                                    | ExprKind::Parens
                                    | ExprKind::Definition
                                    | ExprKind::Declaration
                            ) && source.secrecy_children().len() == 1
                            {
                                source = &source.secrecy_children()[0];
                                continue;
                            }
                            return match source.secrecy_kind() {
                                ExprKind::Parameter(_) | ExprKind::UntypedParameter => {
                                    ArgumentOrigin::UnboundParameter
                                }
                                ExprKind::Binding {
                                    binding,
                                    authored_binding: _,
                                } => self.argument_reference_origin(
                                    binding,
                                    Vec::new(),
                                    &thunk.environment,
                                    Some(source.secrecy_instance().clone()),
                                ),
                                ExprKind::Reference {
                                    binding,
                                    attribute,
                                    segments,
                                    authored_binding: _,
                                } => self.argument_reference_origin(
                                    binding,
                                    std::iter::once(PathSegment::Field {
                                        name: attribute.clone(),
                                    })
                                    .chain(segments.iter().cloned())
                                    .collect(),
                                    &thunk.environment,
                                    Some(source.secrecy_instance().clone()),
                                ),
                                _ => ArgumentOrigin::Syntax(
                                    (source.secrecy_instance().clone(), source.node_id()),
                                    self.reference_environment(thunk.environment.id),
                                    Arc::new(self.activation.clone()),
                                ),
                            };
                        }
                    }
                    Source::Value(value) => {
                        if let Value::Deferred(DeferredValue::Placeholder(
                            ParserPlaceholder::FnParam { name }
                            | ParserPlaceholder::FnLocal { name },
                        )) = value.as_ref()
                            && let Some(parameter) = thunk.environment.names.get(name)
                        {
                            thunk = parameter;
                            continue;
                        }
                        if matches!(
                            value.as_ref(),
                            Value::Deferred(DeferredValue::Placeholder(
                                ParserPlaceholder::FnParam { .. }
                                    | ParserPlaceholder::FnLocal { .. }
                            ))
                        ) {
                            return ArgumentOrigin::UnboundParameter;
                        }
                        return match value.as_ref() {
                            Value::Deferred(DeferredValue::BindingRef { binding }) => self
                                .argument_reference_origin(
                                    binding,
                                    Vec::new(),
                                    &thunk.environment,
                                    None,
                                ),
                            Value::Deferred(DeferredValue::ResourceRef { path }) => self
                                .argument_reference_origin(
                                    path.binding_address(),
                                    std::iter::once(PathSegment::Field {
                                        name: path.attribute().to_string(),
                                    })
                                    .chain(path.segments().iter().cloned())
                                    .collect(),
                                    &thunk.environment,
                                    None,
                                ),
                            _ => ArgumentOrigin::Value(
                                Arc::as_ptr(value) as usize,
                                self.reference_environment(thunk.environment.id),
                                Arc::new(self.activation.clone()),
                            ),
                        };
                    }
                }
            }
        });
        ArgumentFacts {
            proof,
            observations,
            origin,
        }
    }

    fn argument_reference_origin(
        &self,
        binding: &B,
        path: Vec<PathSegment>,
        environment: &Environment<B>,
        instance: Option<ExpressionInstance>,
    ) -> ArgumentOrigin {
        ArgumentOrigin::Reference {
            instance,
            binding: binding.as_str().to_owned(),
            path,
            environment: self.reference_environment(environment.id),
            activation: Arc::new(self.activation.clone()),
        }
    }

    fn record_function(&mut self, mut key: FunctionKey<B>, depth: usize, arguments: Vec<Thunk<B>>) {
        #[cfg(test)]
        {
            self.function_body_evaluations += 1;
        }
        key.activation = Some(Arc::new(self.activation.clone()));
        self.function_recordings.push(FunctionRecording {
            key,
            depth,
            max_before: self.max_depth,
            errors: self.findings.len(),
            reference_nesting: self.active_references.len(),
            references: Vec::new(),
            local_environments: BTreeSet::new(),
            arguments,
            activation: self.activation.clone(),
        });
        self.max_depth = depth;
    }

    fn finish_function(&mut self, values: &[Option<Evidence>]) {
        let mut recording = self
            .function_recordings
            .pop()
            .expect("active function recording");
        let relative_depth = self.max_depth.saturating_sub(recording.depth);
        self.max_depth = self.max_depth.max(recording.max_before);
        let mut keys = HashSet::new();
        let findings = self.findings[recording.errors..]
            .iter()
            .filter(|finding| keys.insert((**finding).clone()))
            .cloned()
            .collect::<Vec<_>>();
        // Recursion depends on the active call stack and is never reusable.
        if findings
            .iter()
            .any(|finding| matches!(finding.cause(), DerivationError::RecursiveFunction(_)))
        {
            return;
        }
        let memo = Arc::new(FunctionMemo {
            evidence: values.last().cloned().flatten(),
            findings,
            relative_depth,
            references: recording.references,
            local_environments: recording.local_environments,
            _arguments: recording.arguments,
        });
        if memo.references.is_empty() {
            recording.key.activation = None;
        }
        if memo.references.is_empty() && memo.findings.is_empty() {
            // Function bodies use argument observations for their conditions,
            // not the caller's slot snapshot. A body with no reference effects
            // is therefore independent of unrelated loop/attribute snapshots.
            self.owner
                .function_evidence
                .borrow_mut()
                .insert(recording.key, memo);
        } else {
            self.function_memo.insert(recording.key, memo);
        }
    }

    fn reuse_function(
        &mut self,
        key: &FunctionKey<B>,
        depth: usize,
        work: &mut Vec<Work<B>>,
    ) -> bool {
        let memo = self
            .owner
            .function_evidence
            .borrow()
            .get(key)
            .cloned()
            .or_else(|| self.function_memo.get(key).cloned())
            .or_else(|| {
                let mut key = key.clone();
                key.activation = Some(Arc::new(self.activation.clone()));
                self.function_memo.get(&key).cloned()
            });
        let Some(memo) = memo else {
            return false;
        };
        #[cfg(test)]
        {
            self.function_memo_hits += 1;
        }
        self.findings.extend(memo.findings.iter().cloned());
        work.push(Work::MemoFunction(key.identity.clone()));
        work.push(Work::FunctionResult(memo.evidence.clone()));
        if memo.references.is_empty() {
            self.max_depth = self.max_depth.max(depth + memo.relative_depth);
            return true;
        }
        // Every invocation used to allocate fresh local environments. Rename
        // those identities when replaying, while retaining the identities of
        // module arguments, resource slots and authored caller arguments.
        let renamed = memo
            .local_environments
            .iter()
            .map(|id| (*id, self.function_environment()))
            .collect::<BTreeMap<_, _>>();
        for effect in memo.references.iter().rev() {
            let mut environments = (*self.reference_environments).clone();
            for (raw, effective) in effect.environments.iter() {
                let target = renamed
                    .get(effective)
                    .or_else(|| self.reference_environments.get(effective))
                    .copied()
                    .unwrap_or(*effective);
                environments.insert(*raw, target);
            }
            for (old, new) in &renamed {
                if !effect.environments.contains_key(old) {
                    environments.insert(*old, *new);
                }
            }
            let mut effect = effect.clone();
            effect.environments = Arc::new(environments);
            work.push(Work::ReplayReference { effect, depth });
        }
        true
    }

    fn function(
        &mut self,
        function: ExprValue<ParserPlaceholder, B>,
        arguments: Vec<Thunk<B>>,
        context: FunctionContext<'_, B>,
    ) {
        let FunctionContext {
            environment,
            path,
            depth,
            work,
            values: _,
        } = context;
        self.push_call(arguments, depth, work, |observations, arguments| {
            Work::Call {
                function,
                count: arguments.len(),
                observations,
                arguments,
                environment,
                path,
                depth,
            }
        });
    }

    fn push_call(
        &self,
        arguments: Vec<Thunk<B>>,
        depth: usize,
        work: &mut Vec<Work<B>>,
        call: impl FnOnce(Vec<ArgumentObservations>, Vec<Thunk<B>>) -> Work<B>,
    ) {
        let observations = arguments
            .iter()
            .map(|argument| ArgumentObservations {
                boolean: self.known_boolean(argument),
                string: self.known_string(argument),
                non_lists: self.non_list_members(argument),
            })
            .collect();
        work.push(call(observations, arguments.clone()));
        work.extend(arguments.into_iter().rev().map(|thunk| Work::Enter {
            thunk,
            path: Vec::new(),
            depth,
        }));
    }
    fn begin_function(
        &mut self,
        function: ExprValue<ParserPlaceholder, B>,
        arguments: Vec<Thunk<B>>,
        context: FunctionContext<'_, B>,
    ) {
        let FunctionContext {
            environment,
            path,
            depth,
            work,
            values,
        } = context;
        let identity = (function.secrecy_instance().clone(), function.node_id());
        let ExprKind::Function { name, signature } = function.secrecy_kind() else {
            unreachable!("function syntax")
        };
        if !self.activation.syntax.insert(identity.clone()) {
            self.finding(
                function.location().clone(),
                self.owner
                    .syntax
                    .secrecy_call(function.secrecy_instance())
                    .cloned(),
                DerivationError::RecursiveFunction(name.clone()),
            );
            values.push(None);
            return;
        }
        let observations = self.owner.function_observations(&function);
        let key = FunctionKey {
            identity: FunctionIdentity::Syntax(identity.clone()),
            arguments: arguments
                .iter()
                .map(|argument| match &argument.source {
                    Source::Argument { facts, .. } => facts.for_key(observations),
                    _ => unreachable!("call arguments have been evaluated"),
                })
                .collect(),
            path: path.clone(),
            activation: None,
        };
        if self.reuse_function(&key, depth, work) {
            return;
        }
        self.record_function(key, depth, arguments.clone());
        let mut parameters = (*environment).clone();
        parameters.id = self.function_environment();
        parameters.in_function = true;
        let mut defaults = Vec::new();
        for (index, node) in signature.parameter_nodes.iter().enumerate() {
            let argument = arguments.get(index).cloned().or_else(|| {
                let id = signature.default_nodes.get(index).copied().flatten()?;
                let source = self
                    .owner
                    .syntax
                    .secrecy_node(function.secrecy_instance(), id)?;
                Some(Thunk {
                    source: Source::Syntax(source),
                    environment: Arc::new(parameters.clone()),
                })
            });
            if let Some(argument) = argument {
                if index >= arguments.len() {
                    defaults.push(argument.clone());
                }
                Arc::make_mut(&mut parameters.nodes)
                    .insert((function.secrecy_instance().clone(), *node), argument);
            }
        }
        let Some(body) = function.secrecy_function_body() else {
            values.push(None);
            self.finish_function(values);
            self.activation.syntax.shift_remove(&identity);
            return;
        };
        let mut children = defaults;
        children.push(Thunk {
            source: Source::Syntax(body.clone()),
            environment: Arc::new(parameters),
        });
        work.push(Work::Function { identity });
        self.schedule(Operation::Last, children, path, depth, work);
    }
    fn enter_value(
        &mut self,
        value: &Value<ParserPlaceholder, B>,
        environment: Arc<Environment<B>>,
        path: Vec<PathSegment>,
        depth: usize,
        work: &mut Vec<Work<B>>,
        values: &mut Vec<Option<Evidence>>,
    ) {
        let thunk = |value: &Value<ParserPlaceholder, B>| Thunk {
            source: Source::Value(Arc::new(value.clone())),
            environment: environment.clone(),
        };
        match value {
            Value::Concrete(concrete) => match concrete {
                ConcreteValue::String(_)
                | ConcreteValue::EnumIdentifier(_)
                | ConcreteValue::CanonicalEnum(_)
                | ConcreteValue::Int(_)
                | ConcreteValue::Float(_)
                | ConcreteValue::Bool(_)
                | ConcreteValue::Duration(_)
                | ConcreteValue::StringList(_) => {
                    values.push(Some(Evidence::Leaf(Secrecy::Public)))
                }
                ConcreteValue::List(elements) => self.schedule(
                    Operation::List,
                    elements.iter().map(thunk),
                    path,
                    depth,
                    work,
                ),
                ConcreteValue::Map(fields) => self.schedule(
                    Operation::Map(fields.keys().cloned().collect()),
                    fields.values().map(thunk),
                    path,
                    depth,
                    work,
                ),
            },
            Value::Deferred(deferred) => match deferred {
                DeferredValue::Secret(secret) => self.schedule(
                    Operation::Secret,
                    [thunk(secret.expose_plaintext())],
                    path,
                    depth,
                    work,
                ),
                DeferredValue::ResourceRef { path: reference } => {
                    let path = std::iter::once(PathSegment::Field {
                        name: reference.attribute().to_string(),
                    })
                    .chain(reference.segments().iter().cloned())
                    .chain(path)
                    .collect();
                    self.reference(
                        reference.binding_address().clone(),
                        path,
                        environment,
                        depth,
                        work,
                        values,
                    );
                }
                DeferredValue::BindingRef { binding } => {
                    self.reference(binding.clone(), path, environment, depth, work, values)
                }
                DeferredValue::Coalesce { value, default } => self.schedule(
                    Operation::Alternatives {
                        operator: Alternatives::Coalesce,
                        origin: None,
                    },
                    [thunk(value), thunk(default)],
                    path,
                    depth,
                    work,
                ),
                DeferredValue::IntegerArithmetic(expression) => self.schedule(
                    Operation::Propagate,
                    [thunk(expression.lhs()), thunk(expression.rhs())],
                    path,
                    depth,
                    work,
                ),
                DeferredValue::Interpolation(parts) => {
                    let children = parts
                        .iter()
                        .filter_map(|part| match part {
                            InterpolationPart::Expr(value) => Some(thunk(value)),
                            InterpolationPart::Literal(_) => None,
                        })
                        .collect::<Vec<_>>();
                    self.schedule(Operation::Propagate, children, path, depth, work);
                }
                DeferredValue::FunctionCall {
                    name,
                    args,
                    source_line: _,
                }
                | DeferredValue::Unavailable {
                    name,
                    args,
                    reason: _,
                    source_line: _,
                } => {
                    let children = args
                        .iter()
                        .filter_map(|argument| argument.value())
                        .map(thunk)
                        .collect::<Vec<_>>();
                    let operation = builtin_operation(
                        name.as_str(),
                        None,
                        (*name == crate::builtins::BuiltinFunction::map)
                            .then(|| {
                                children
                                    .first()
                                    .and_then(|argument| self.known_string(argument))
                            })
                            .flatten(),
                        (*name == crate::builtins::BuiltinFunction::flatten)
                            .then(|| {
                                children
                                    .first()
                                    .and_then(|argument| self.non_list_members(argument))
                            })
                            .flatten(),
                    );
                    self.schedule(operation, children, path, depth, work);
                }
                DeferredValue::ResolvedFunctionCall {
                    function,
                    functions,
                    args,
                    source_line: _,
                } => {
                    if let Some(syntax) = self
                        .owner
                        .syntax
                        .secrecy_lexical_function(environment.module.as_ref(), &function.name)
                    {
                        self.function(
                            syntax,
                            args.iter()
                                .map(|argument| thunk(argument.walker_value()))
                                .collect(),
                            FunctionContext {
                                environment,
                                path,
                                depth,
                                work,
                                values,
                            },
                        );
                        return;
                    }
                    let lexical_functions = environment
                        .functions
                        .clone()
                        .unwrap_or_else(|| functions.clone());
                    let lexical_function = lexical_functions
                        .get(&function.name)
                        .map(|function| Arc::new(function.clone()))
                        .unwrap_or_else(|| function.clone());
                    let arguments = args
                        .iter()
                        .map(|argument| thunk(argument.walker_value()))
                        .collect::<Vec<_>>();
                    self.push_call(arguments, depth, work, |observations, arguments| {
                        Work::ValueCall {
                            function: lexical_function,
                            functions: lexical_functions,
                            count: arguments.len(),
                            observations,
                            arguments,
                            environment,
                            path,
                            depth,
                        }
                    });
                }
                DeferredValue::Access(access) => {
                    let path = std::iter::once(access.step().clone()).chain(path).collect();
                    work.push(Work::Enter {
                        thunk: thunk(access.base()),
                        path,
                        depth,
                    });
                }
                DeferredValue::Placeholder(placeholder) => match placeholder {
                    ParserPlaceholder::ModuleArgument {
                        name,
                        diagnostic: _,
                    } => {
                        if let Some(module) = &environment.module {
                            let binding =
                                crate::resource::binding_address::binding_selected_attribute(
                                    module, name,
                                );
                            self.reference(binding, path, environment, depth, work, values);
                        } else {
                            values.push(None);
                        }
                    }
                    ParserPlaceholder::FnParam { name } | ParserPlaceholder::FnLocal { name } => {
                        if let Some(parameter) = environment.names.get(name) {
                            work.push(Work::Enter {
                                thunk: parameter.clone(),
                                path,
                                depth,
                            });
                        } else {
                            values.push(None);
                        }
                    }
                    ParserPlaceholder::Loop(_)
                    | ParserPlaceholder::EmptyInterpolation { source: _ } => values.push(None),
                },
                DeferredValue::ModuleIf(conditional) => {
                    let selected = self
                        .known_boolean(&thunk(conditional.condition().value()))
                        .map(|selected| if selected { 0 } else { 1 });
                    self.schedule(
                        Operation::If { selected },
                        [
                            thunk(conditional.then_value()),
                            thunk(conditional.else_value()),
                        ],
                        path,
                        depth,
                        work,
                    );
                }
                DeferredValue::Unknown(_) => values.push(None),
            },
        }
    }
    fn begin_value_function(
        &mut self,
        function: Arc<crate::parser::UserFunction<B>>,
        functions: Arc<IndexMap<String, crate::parser::UserFunction<B>>>,
        args: Vec<Option<Evidence>>,
        observations: Vec<ArgumentObservations>,
        authored_arguments: Vec<Thunk<B>>,
        context: FunctionContext<'_, B>,
    ) {
        let FunctionContext {
            environment,
            path,
            depth,
            work,
            values,
        } = context;
        let location = function.body.location.clone();
        let activation = Arc::new(self.activation.clone());
        if !self.activation.values.insert(location.clone()) {
            self.finding(
                location,
                None,
                DerivationError::RecursiveFunction(function.name.clone()),
            );
            values.push(None);
            return;
        }
        let argument_count = args.len();
        let arguments = args
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                self.argument_facts(
                    &authored_arguments[index],
                    value,
                    observations[index].clone(),
                )
            })
            .collect::<Vec<_>>();
        let identity = FunctionIdentity::Value(location.clone(), environment.module.clone());
        let dependencies = self
            .owner
            .value_function_observations(&identity, &function, &functions);
        let key = FunctionKey {
            identity,
            arguments: arguments
                .iter()
                .map(|facts| facts.for_key(dependencies))
                .collect(),
            path: path.clone(),
            activation: None,
        };
        if self.reuse_function(&key, depth, work) {
            return;
        }
        self.record_function(key, depth, authored_arguments.clone());
        let mut parameters = (*environment).clone();
        parameters.id = self.function_environment();
        parameters.in_function = true;
        parameters.functions = Some(functions);
        let mut defaults = Vec::new();
        for (index, parameter) in function.params.iter().enumerate() {
            let argument = if let Some(facts) = arguments.get(index) {
                Some(Thunk {
                    source: Source::Argument {
                        authored: Box::new(authored_arguments[index].clone()),
                        facts: facts.clone(),
                        activation: activation.clone(),
                    },
                    environment: environment.clone(),
                })
            } else {
                parameter.default.as_ref().map(|value| Thunk {
                    source: Source::Value(Arc::new(value.clone())),
                    environment: Arc::new(parameters.clone()),
                })
            };
            if let Some(argument) = argument {
                if index >= argument_count {
                    defaults.push(argument.clone());
                }
                Arc::make_mut(&mut parameters.names).insert(parameter.name.clone(), argument);
            }
        }
        let mut children = defaults;
        for (name, value, _) in &function.local_lets {
            let local = Thunk {
                source: Source::Value(Arc::new(value.clone())),
                environment: Arc::new(parameters.clone()),
            };
            children.push(local.clone());
            Arc::make_mut(&mut parameters.names).insert(name.clone(), local);
        }
        children.push(Thunk {
            source: Source::Value(Arc::new(function.body.value.clone())),
            environment: Arc::new(parameters),
        });
        work.push(Work::ValueFunction { location });
        self.schedule(Operation::Last, children, path, depth, work);
    }
    fn finish(
        &mut self,
        operation: Operation,
        children: &mut [Option<Evidence>],
    ) -> Option<Evidence> {
        match operation {
            Operation::Secret => Some(Evidence::Leaf(Secrecy::Secret)),
            Operation::Pending => None,
            Operation::Length => match children.first()?.as_ref()? {
                Evidence::Map(_) | Evidence::List(_) => Some(Evidence::Leaf(Secrecy::Public)),
                Evidence::Leaf(secrecy) => Some(Evidence::Leaf(*secrecy)),
            },
            Operation::Keys => match children.first()?.as_ref()? {
                Evidence::Map(fields) => Some(Evidence::List(Arc::new(vec![
                    Evidence::Leaf(
                        Secrecy::Public
                    );
                    fields.len()
                ]))),
                Evidence::Leaf(secrecy) => Some(Evidence::Leaf(*secrecy)),
                Evidence::List(_) => None,
            },
            Operation::Values => match children.first()?.as_ref()? {
                Evidence::Map(fields) => {
                    Some(Evidence::List(Arc::new(fields.values().cloned().collect())))
                }
                Evidence::Leaf(secrecy) => Some(Evidence::Leaf(*secrecy)),
                Evidence::List(_) => None,
            },
            Operation::Concat => {
                // with_secret_values protects the whole result only for a
                // direct wrapper. Secret members retain their own positions.
                if children
                    .iter()
                    .any(|child| matches!(child, Some(Evidence::Leaf(Secrecy::Secret))))
                {
                    return Some(Evidence::Leaf(Secrecy::Secret));
                }
                let [Some(items), Some(base)] = children else {
                    return None;
                };
                match (items, base) {
                    (Evidence::List(items), Evidence::List(base)) => Some(Evidence::List(
                        Arc::new(base.iter().chain(items.iter()).cloned().collect()),
                    )),
                    (items, base)
                        if items.secrecy() == Secrecy::Public
                            && base.secrecy() == Secrecy::Public =>
                    {
                        Some(Evidence::Leaf(Secrecy::Public))
                    }
                    _ => None,
                }
            }
            Operation::Flatten(non_lists) => match children.first()?.as_ref()? {
                Evidence::List(items)
                    if items.iter().enumerate().any(|(index, item)| {
                        matches!(item, Evidence::Leaf(Secrecy::Public))
                            && !non_lists
                                .as_ref()
                                .and_then(|items| items.get(index))
                                .copied()
                                .unwrap_or(false)
                    }) =>
                {
                    // A uniform Public reference may itself be a list of
                    // unknown length. It cannot occupy an invented single
                    // position before a Secret member.
                    (aggregate(items.iter()) == Secrecy::Public)
                        .then_some(Evidence::Leaf(Secrecy::Public))
                }
                Evidence::List(items) => Some(Evidence::List(Arc::new(
                    items
                        .iter()
                        .flat_map(|item| match item {
                            Evidence::List(items) => items.as_ref().clone(),
                            item => vec![item.clone()],
                        })
                        .collect(),
                ))),
                Evidence::Leaf(secrecy) => Some(Evidence::Leaf(*secrecy)),
                Evidence::Map(_) => None,
            },
            Operation::Project(accessor) => {
                if children
                    .iter()
                    .any(|child| matches!(child, Some(Evidence::Leaf(Secrecy::Secret))))
                {
                    return Some(Evidence::Leaf(Secrecy::Secret));
                }
                let field = accessor?.strip_prefix('.')?.to_owned();
                let select = |value: &Evidence| {
                    value.select(&[PathSegment::Field {
                        name: field.clone(),
                    }])
                };
                match children.get(1)?.as_ref()? {
                    Evidence::List(items) => items
                        .iter()
                        .map(select)
                        .collect::<Option<Vec<_>>>()
                        .map(|items| Evidence::List(Arc::new(items))),
                    Evidence::Map(fields) => fields
                        .iter()
                        .map(|(key, value)| select(value).map(|value| (key.clone(), value)))
                        .collect::<Option<BTreeMap<_, _>>>()
                        .map(|fields| Evidence::Map(Arc::new(fields))),
                    Evidence::Leaf(secrecy) => Some(Evidence::Leaf(*secrecy)),
                }
            }
            Operation::Key => match children.first_mut()?.take()? {
                Evidence::Leaf(secrecy) => Some(Evidence::Leaf(secrecy)),
                Evidence::Map(_) | Evidence::List(_) => Some(Evidence::Leaf(Secrecy::Public)),
            },
            Operation::Selection { path } => {
                let base = children.first_mut()?.take()?;
                let candidates = match base {
                    Evidence::Map(fields) => fields.values().cloned().collect(),
                    Evidence::List(elements) => Arc::unwrap_or_clone(elements),
                    Evidence::Leaf(secrecy) => return Some(Evidence::Leaf(secrecy)),
                };
                let candidates = candidates
                    .iter()
                    .map(|candidate| candidate.select(&path))
                    .collect::<Option<Vec<_>>>()?;
                match agreement(candidates) {
                    Agreement::Known(evidence) => Some(evidence),
                    Agreement::Mixed | Agreement::Pending => None,
                }
            }
            Operation::Propagate => {
                if children
                    .iter()
                    .flatten()
                    .any(|value| value.secrecy() == Secrecy::Secret)
                {
                    Some(Evidence::Leaf(Secrecy::Secret))
                } else if children.iter().all(Option::is_some) {
                    Some(Evidence::Leaf(Secrecy::Public))
                } else {
                    None
                }
            }
            Operation::List => children
                .iter_mut()
                .map(Option::take)
                .collect::<Option<Vec<_>>>()
                .map(|values| Evidence::List(Arc::new(values))),
            Operation::Map(keys) => keys
                .into_iter()
                .zip(children.iter_mut())
                .map(|(key, value)| value.take().map(|value| (key, value)))
                .collect::<Option<BTreeMap<_, _>>>()
                .map(|values| Evidence::Map(Arc::new(values))),
            Operation::Last => children.last_mut()?.take(),
            Operation::If { selected } => match selected {
                Some(index) => children.get_mut(index)?.take(),
                None => {
                    let first = children.first()?.as_ref()?;
                    children
                        .iter()
                        .all(|child| child.as_ref() == Some(first))
                        .then(|| first.clone())
                }
            },
            Operation::Alternatives { operator, origin } => {
                // lookup's key participates in the runtime wrapper, while D2
                // still checks only map members and the authored default.
                let protects_result = operator == Alternatives::Lookup
                    && children
                        .iter()
                        .any(|child| matches!(child, Some(Evidence::Leaf(Secrecy::Secret))));
                let alternatives = match operator {
                    Alternatives::Coalesce => children.iter_mut().map(Option::take).collect(),
                    Alternatives::Lookup => {
                        let mut alternatives = Vec::new();
                        match children.first_mut().and_then(Option::take) {
                            Some(Evidence::Map(fields)) => {
                                alternatives.extend(fields.values().cloned().map(Some))
                            }
                            Some(Evidence::Leaf(secrecy)) => {
                                alternatives.push(Some(Evidence::Leaf(secrecy)))
                            }
                            Some(Evidence::List(_)) | None => alternatives.push(None),
                        }
                        alternatives.push(children.get_mut(2).and_then(Option::take));
                        alternatives
                    }
                };
                let complete = alternatives.iter().all(Option::is_some);
                match agreement(alternatives.into_iter().flatten()) {
                    Agreement::Mixed => {
                        // The retained authored occurrence owns this error.
                        // Replayed argument Values have no operator identity
                        // and cannot invent another location or grant proof.
                        if let Some((location, call)) = origin {
                            self.finding(
                                location,
                                call,
                                DerivationError::MixedAlternatives(MixedAlternatives { operator }),
                            );
                        }
                        None
                    }
                    Agreement::Known(evidence) if complete => Some(if protects_result {
                        Evidence::Leaf(Secrecy::Secret)
                    } else {
                        evidence
                    }),
                    Agreement::Known(_) | Agreement::Pending => None,
                }
            }
        }
    }
}

enum ReferenceSource<B: BindingAddress> {
    Source(Thunk<B>, Vec<PathSegment>),
    Public,
    Handle,
    Pending,
}
impl<B: BindingAddress> Owner<'_, B> {
    fn reference_source(
        &self,
        binding: &B,
        path: &[PathSegment],
        environment: Arc<Environment<B>>,
    ) -> ReferenceSource<B> {
        if self.upstream.contains(binding) {
            return ReferenceSource::Public;
        }
        if let Some(value) = self.syntax.secrecy_argument(binding) {
            // One fresh identity per authored argument source. Repeated
            // encounters still share that source's per-attribute reference
            // memo, without aliasing the empty environment (id zero).
            let environment = self
                .argument_environments
                .borrow_mut()
                .entry(binding.clone())
                .or_insert_with(|| {
                    Arc::new(Environment {
                        id: next_environment(),
                        module: self.syntax.secrecy_argument_module(binding).cloned(),
                        ..Environment::default()
                    })
                })
                .clone();
            return ReferenceSource::Source(
                Thunk {
                    source: Source::Value(Arc::new(value.clone())),
                    environment,
                },
                path.to_vec(),
            );
        }
        if let Some(resource) = self.resources.get(binding) {
            let schema =
                self.schemas
                    .get(&resource.provider, &resource.resource_type, resource.kind);
            let Some(step) = path.first() else {
                return if schema.is_some() {
                    ReferenceSource::Handle
                } else {
                    ReferenceSource::Pending
                };
            };
            let name = match step {
                PathSegment::Field { name } => Some(name.as_str()),
                PathSegment::Subscript {
                    index: Subscript::Str { key },
                } => Some(key.expose_plaintext()),
                PathSegment::Subscript {
                    index: Subscript::Int { index: _ },
                } => None,
            };
            if resource.kind == SchemaKind::Resource
                && let Some(source) = name.and_then(|name| resource.attributes.get(name))
            {
                return ReferenceSource::Source(source.clone(), path[1..].to_vec());
            }
            if schema.is_none() {
                return ReferenceSource::Pending;
            }
            let ty = crate::parser::TypeExpr::Ref(crate::parser::ResourceTypePath::new(
                &resource.provider,
                &resource.resource_type,
            ));
            return if crate::validation::narrow_type_expr(&ty, path, Some(self.schemas)).is_some() {
                ReferenceSource::Public
            } else {
                ReferenceSource::Pending
            };
        }
        if let Some(PathSegment::Field { name }) = path.first()
            && let Some(output) = self.syntax.secrecy_output(binding, name)
        {
            return ReferenceSource::Source(
                Thunk {
                    source: Source::Syntax(output),
                    environment: self.empty_environment.clone(),
                },
                path[1..].to_vec(),
            );
        }
        if let Some(definition) = self.syntax.secrecy_definition(binding) {
            return ReferenceSource::Source(
                Thunk {
                    source: Source::Syntax(definition.clone()),
                    environment,
                },
                path.to_vec(),
            );
        }
        ReferenceSource::Pending
    }
}

fn next_environment() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

enum Agreement {
    Known(Evidence),
    Mixed,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Uniformity {
    Empty,
    Known(Secrecy),
    Mixed,
}

fn uniform(value: &Evidence) -> Uniformity {
    match value {
        Evidence::Leaf(secrecy) => Uniformity::Known(*secrecy),
        Evidence::Map(fields) => uniform_members(fields.values()),
        Evidence::List(elements) => uniform_members(elements.iter()),
    }
}
fn uniform_members<'a>(values: impl Iterator<Item = &'a Evidence>) -> Uniformity {
    values.fold(Uniformity::Empty, |common, value| {
        match (common, uniform(value)) {
            (Uniformity::Empty, other) | (other, Uniformity::Empty) => other,
            (Uniformity::Known(left), Uniformity::Known(right)) if left == right => {
                Uniformity::Known(left)
            }
            (Uniformity::Known(_), Uniformity::Known(_))
            | (Uniformity::Mixed, _)
            | (_, Uniformity::Mixed) => Uniformity::Mixed,
        }
    })
}
fn agreement(values: impl IntoIterator<Item = Evidence>) -> Agreement {
    let mut values = values.into_iter();
    let Some(mut common) = values.next() else {
        return Agreement::Pending;
    };
    for value in values {
        match pair_agreement(&common, &value) {
            Agreement::Known(evidence) => common = evidence,
            Agreement::Mixed => return Agreement::Mixed,
            Agreement::Pending => return Agreement::Pending,
        }
    }
    Agreement::Known(common)
}
fn pair_agreement(left: &Evidence, right: &Evidence) -> Agreement {
    if left == right {
        return Agreement::Known(left.clone());
    }
    match (uniform(left), uniform(right)) {
        (Uniformity::Empty, _) => return Agreement::Known(right.clone()),
        (_, Uniformity::Empty) => return Agreement::Known(left.clone()),
        (Uniformity::Known(left), Uniformity::Known(right)) if left == right => {
            return Agreement::Known(Evidence::Leaf(left));
        }
        (Uniformity::Known(_), Uniformity::Known(_) | Uniformity::Mixed)
        | (Uniformity::Mixed, Uniformity::Known(_)) => return Agreement::Mixed,
        (Uniformity::Mixed, Uniformity::Mixed) => {}
    }
    match (left, right) {
        (Evidence::Map(left), Evidence::Map(right)) if left.keys().eq(right.keys()) => {
            let mut common = BTreeMap::new();
            for (key, left) in left.iter() {
                match pair_agreement(left, &right[key]) {
                    Agreement::Known(value) => {
                        common.insert(key.clone(), value);
                    }
                    Agreement::Mixed => return Agreement::Mixed,
                    Agreement::Pending => return Agreement::Pending,
                }
            }
            Agreement::Known(Evidence::Map(Arc::new(common)))
        }
        (Evidence::List(left), Evidence::List(right)) if left.len() == right.len() => {
            let mut common = Vec::new();
            for (left, right) in left.iter().zip(right.iter()) {
                match pair_agreement(left, right) {
                    Agreement::Known(value) => common.push(value),
                    Agreement::Mixed => return Agreement::Mixed,
                    Agreement::Pending => return Agreement::Pending,
                }
            }
            Agreement::Known(Evidence::List(Arc::new(common)))
        }
        (Evidence::Leaf(_), Evidence::Leaf(_) | Evidence::Map(_) | Evidence::List(_))
        | (Evidence::Map(_) | Evidence::List(_), Evidence::Leaf(_))
        | (Evidence::Map(_), Evidence::Map(_) | Evidence::List(_))
        | (Evidence::List(_), Evidence::List(_) | Evidence::Map(_)) => Agreement::Pending,
    }
}

#[cfg(test)]
mod tests;
