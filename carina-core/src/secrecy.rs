//! Structural secrecy of authored configuration. Values obtained from providers
//! and state never enter this owner. Availability and secrecy are separate facts.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use indexmap::IndexMap;

use crate::parser::retained::{ExprKind, ExprValue, ExpressionInstance, ExpressionReport, NodeId};
use crate::parser::{DeclarationLocation, File, ResourceRef};
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, InterpolationPart, ParserPlaceholder,
    PathSegment, ResourceId, Subscript, Value,
};
use crate::schema::{SchemaKind, SchemaRegistry};

pub const MAX_REFERENCE_DEPTH: usize = 128;
pub const MAX_REFERENCE_VISITS: usize = 4096;

/// References and expression steps share this counting policy.
fn depth_after_step(depth: usize) -> usize {
    depth + 1
}

/// The only leaves admitted into runtime evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secrecy {
    Secret,
    Public,
}

/// A payload-free description of an authored value's secret paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    Leaf(Secrecy),
    Map(BTreeMap<String, Evidence>),
    List(Vec<Evidence>),
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

/// The core owns message text and both authored and instantiated locations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Finding {
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
        match &self.cause {
            DerivationError::MixedAlternatives(MixedAlternatives { operator }) => {
                let operator = match operator {
                    Alternatives::Coalesce => "??",
                    Alternatives::Lookup => "lookup()",
                };
                format!(
                    "Attribute '{}': {operator} alternatives mix secret and public values; all alternatives must be secret or all public.",
                    self.attribute
                )
            }
            DerivationError::LimitReached(LimitReached {
                kind: LimitKind::Depth,
            }) => format!(
                "Attribute '{}': the value follows a chain of {MAX_REFERENCE_DEPTH} or more reference or expression steps from this attribute; Carina cannot decide whether it is a secret.",
                self.attribute
            ),
            DerivationError::LimitReached(LimitReached {
                kind: LimitKind::ReferenceVisits,
            }) => format!(
                "Attribute '{}': deciding whether the value is a secret needs more than {MAX_REFERENCE_VISITS} reference lookups from this attribute.",
                self.attribute
            ),
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
    pub limit: Option<LimitKind>,
}

/// Diagnostic results are intentionally distinct from admitted runtime evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecrecyReport {
    findings: Vec<Finding>,
    finding_keys: HashSet<(DeclarationLocation, Option<CallSite>, DerivationError)>,
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
        proof: Option<Evidence>,
        boolean: Option<bool>,
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
        if matches!(
            source.secrecy_kind(),
            ExprKind::Scalar(_) | ExprKind::EmptyInterpolation
        ) {
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
    memo: RefCell<HashMap<ReferenceKey<B>, Memo>>,
    empty_environment: Arc<Environment<B>>,
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
        memo: RefCell::default(),
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
                if matches!(
                    value.secrecy_kind(),
                    ExprKind::Scalar(_) | ExprKind::EmptyInterpolation
                ) {
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
            if let ExprKind::Binding(binding)
            | ExprKind::Reference {
                binding,
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
            && !syntax.secrecy_is_structural(location)
            && let Some(source) = syntax.secrecy_at(location)
        {
            report.inspect_source(
                &owner,
                declaration.detail().name().to_string(),
                location.clone(),
                source,
                owner.empty_environment.clone(),
            );
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
    },
    Function {
        identity: (ExpressionInstance, NodeId),
    },
    Call {
        function: ExprValue<ParserPlaceholder, B>,
        count: usize,
        booleans: Vec<Option<bool>>,
        arguments: Vec<Thunk<B>>,
        environment: Arc<Environment<B>>,
        path: Vec<PathSegment>,
        depth: usize,
    },
    ValueCall {
        function: Arc<crate::parser::UserFunction<B>>,
        functions: Arc<IndexMap<String, crate::parser::UserFunction<B>>>,
        count: usize,
        booleans: Vec<Option<bool>>,
        arguments: Vec<Thunk<B>>,
        environment: Arc<Environment<B>>,
        path: Vec<PathSegment>,
        depth: usize,
    },
    ValueFunction {
        location: DeclarationLocation,
    },
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
    findings: Vec<Finding>,
    visits: usize,
    #[cfg(test)]
    memo_hits: usize,
    max_depth: usize,
    active_references: HashSet<ReferenceKey<B>>,
    active_functions: HashSet<(ExpressionInstance, NodeId)>,
    active_value_functions: HashSet<DeclarationLocation>,
    pending_validation: bool,
    limited: bool,
}
impl<'a, B: BindingAddress> Walk<'a, B> {
    fn new(owner: &'a Owner<'a, B>, attribute: String, location: DeclarationLocation) -> Self {
        Self {
            owner,
            attribute,
            attribute_location: location,
            call_site: None,
            findings: Vec::new(),
            visits: 0,
            #[cfg(test)]
            memo_hits: 0,
            max_depth: 0,
            active_references: HashSet::new(),
            active_functions: HashSet::new(),
            active_value_functions: HashSet::new(),
            pending_validation: false,
            limited: false,
        }
    }
    fn in_source(mut self, source: &ExprValue<ParserPlaceholder, B>) -> Self {
        self.call_site = self
            .owner
            .syntax
            .secrecy_call(source.secrecy_instance())
            .cloned();
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
        self.findings.push(Finding {
            attribute: self.attribute.clone(),
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
            && matches!(
                expression.secrecy_kind(),
                ExprKind::Scalar(_) | ExprKind::EmptyInterpolation
            )
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
                Work::Enter { thunk, path, depth } => {
                    self.enter(thunk, path, depth, &mut work, &mut values)
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
                } => {
                    self.active_references.remove(&key);
                    if errors == self.findings.len()
                        && !self.pending_validation
                        && let Some(Some(evidence)) = values.last()
                    {
                        self.owner.memo.borrow_mut().insert(
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
                    self.active_functions.remove(&identity);
                }
                Work::Call {
                    function,
                    count,
                    booleans,
                    arguments: authored_arguments,
                    environment,
                    path,
                    depth,
                } => {
                    let start = values.len() - count;
                    let arguments = values
                        .drain(start..)
                        .enumerate()
                        .map(|(index, value)| Thunk {
                            source: Source::Argument {
                                authored: Box::new(authored_arguments[index].clone()),
                                proof: value,
                                boolean: booleans[index],
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
                    booleans,
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
                        booleans,
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
                    self.active_value_functions.remove(&location);
                }
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
            | Operation::Key => depth_after_step(depth),
            Operation::Secret | Operation::Last | Operation::Pending => depth,
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
        self.visits += 1;
        if self.visits > MAX_REFERENCE_VISITS {
            self.limit(LimitKind::ReferenceVisits);
            values.push(None);
            return;
        }
        let next_depth = depth_after_step(depth);
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
            environment: source_environment,
            binding: binding.clone(),
            path: path.clone(),
        };
        let memo = self.owner.memo.borrow().get(&key).cloned();
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
                proof,
                boolean: _,
            } => {
                if let Some(proof) = proof {
                    values.push(proof.select(&path));
                } else if path.is_empty() {
                    values.push(None);
                } else {
                    work.push(Work::Enter {
                        thunk: *authored,
                        path,
                        depth,
                    });
                }
            }
            Source::Syntax(expression) => {
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
                    ExprKind::Binding(binding) => {
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
                        } else if name == "decrypt" {
                            self.schedule(Operation::Secret, children(), path, depth, work);
                        } else if name == "lookup" {
                            self.schedule(
                                Operation::Alternatives {
                                    operator: Alternatives::Lookup,
                                    origin: Some((
                                        expression.location().clone(),
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
                            );
                        } else {
                            self.schedule(Operation::Propagate, children(), path, depth, work);
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

    fn known_boolean(&self, input: &Thunk<B>) -> Option<bool> {
        let mut thunk = input.clone();
        let mut bindings = HashSet::new();
        loop {
            match &thunk.source {
                Source::Argument {
                    authored,
                    proof: _,
                    boolean,
                } => {
                    if let Some(boolean) = boolean {
                        return Some(*boolean);
                    }
                    thunk = (**authored).clone();
                    continue;
                }
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
        self.push_call(arguments, depth, work, |booleans, arguments| Work::Call {
            function,
            count: arguments.len(),
            booleans,
            arguments,
            environment,
            path,
            depth,
        });
    }

    fn push_call(
        &self,
        arguments: Vec<Thunk<B>>,
        depth: usize,
        work: &mut Vec<Work<B>>,
        call: impl FnOnce(Vec<Option<bool>>, Vec<Thunk<B>>) -> Work<B>,
    ) {
        let booleans = arguments
            .iter()
            .map(|argument| self.known_boolean(argument))
            .collect();
        work.push(call(booleans, arguments.clone()));
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
        if !self.active_functions.insert(identity.clone()) {
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
        let mut parameters = (*environment).clone();
        parameters.id = next_environment();
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
            self.active_functions.remove(&identity);
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
                    let operation = if name.as_str() == "decrypt" {
                        Operation::Secret
                    } else if name.as_str() == "lookup" {
                        Operation::Alternatives {
                            operator: Alternatives::Lookup,
                            origin: None,
                        }
                    } else {
                        Operation::Propagate
                    };
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
                    self.push_call(arguments, depth, work, |booleans, arguments| {
                        Work::ValueCall {
                            function: lexical_function,
                            functions: lexical_functions,
                            count: arguments.len(),
                            booleans,
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
        booleans: Vec<Option<bool>>,
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
        if !self.active_value_functions.insert(location.clone()) {
            self.finding(
                location,
                None,
                DerivationError::RecursiveFunction(function.name.clone()),
            );
            values.push(None);
            return;
        }
        let mut parameters = (*environment).clone();
        parameters.id = next_environment();
        parameters.in_function = true;
        parameters.functions = Some(functions);
        let mut defaults = Vec::new();
        for (index, parameter) in function.params.iter().enumerate() {
            let argument = if let Some(value) = args.get(index) {
                Some(Thunk {
                    source: Source::Argument {
                        authored: Box::new(authored_arguments[index].clone()),
                        proof: value.clone(),
                        boolean: booleans[index],
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
                if index >= args.len() {
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
            Operation::Key => match children.first_mut()?.take()? {
                Evidence::Leaf(secrecy) => Some(Evidence::Leaf(secrecy)),
                Evidence::Map(_) | Evidence::List(_) => Some(Evidence::Leaf(Secrecy::Public)),
            },
            Operation::Selection { path } => {
                let base = children.first_mut()?.take()?;
                let candidates = match base {
                    Evidence::Map(fields) => fields.into_values().collect(),
                    Evidence::List(elements) => elements,
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
                .map(Evidence::List),
            Operation::Map(keys) => keys
                .into_iter()
                .zip(children.iter_mut())
                .map(|(key, value)| value.take().map(|value| (key, value)))
                .collect::<Option<BTreeMap<_, _>>>()
                .map(Evidence::Map),
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
                let alternatives = match operator {
                    Alternatives::Coalesce => children.iter_mut().map(Option::take).collect(),
                    Alternatives::Lookup => {
                        let mut alternatives = Vec::new();
                        match children.first_mut().and_then(Option::take) {
                            Some(Evidence::Map(fields)) => {
                                alternatives.extend(fields.into_values().map(Some))
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
                    Agreement::Known(evidence) if complete => Some(evidence),
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
            return ReferenceSource::Source(
                Thunk {
                    source: Source::Value(Arc::new(value.clone())),
                    environment: Arc::new(Environment {
                        module: self.syntax.secrecy_argument_module(binding).cloned(),
                        ..Environment::default()
                    }),
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
            for (key, left) in left {
                match pair_agreement(left, &right[key]) {
                    Agreement::Known(value) => {
                        common.insert(key.clone(), value);
                    }
                    Agreement::Mixed => return Agreement::Mixed,
                    Agreement::Pending => return Agreement::Pending,
                }
            }
            Agreement::Known(Evidence::Map(common))
        }
        (Evidence::List(left), Evidence::List(right)) if left.len() == right.len() => {
            let mut common = Vec::new();
            for (left, right) in left.iter().zip(right) {
                match pair_agreement(left, right) {
                    Agreement::Known(value) => common.push(value),
                    Agreement::Mixed => return Agreement::Mixed,
                    Agreement::Pending => return Agreement::Pending,
                }
            }
            Agreement::Known(Evidence::List(common))
        }
        (Evidence::Leaf(_), Evidence::Leaf(_) | Evidence::Map(_) | Evidence::List(_))
        | (Evidence::Map(_) | Evidence::List(_), Evidence::Leaf(_))
        | (Evidence::Map(_), Evidence::Map(_) | Evidence::List(_))
        | (Evidence::List(_), Evidence::List(_) | Evidence::Map(_)) => Agreement::Pending,
    }
}

#[cfg(test)]
mod tests;
