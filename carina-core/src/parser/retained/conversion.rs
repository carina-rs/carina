//! Checked conversion policy over the walk's authored cursor. No value routing.

pub(crate) mod diagnostic;

use super::*;
use crate::schema::value_validation::{AssignmentFindings, AuthoredOutcome, CheckValue};
use crate::schema::{AttrTypeKind, CompiledAttributeType, LeafPath, RefinedScalarKind, TypeError};
use crate::validation::admission::DestinationKind;
use diagnostic::{ConversionError, ConversionErrorKind as Kind, DeclarationBounds};

/// Parse-stage judgement stored on the authored Convert itself.
#[derive(Clone)]
pub(crate) struct ConversionParsing {
    judgement: Arc<std::sync::OnceLock<ParseJudgement>>,
    secret: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParseJudgement {
    Rejected(ConversionError),
    OperandFailed,
}
impl std::fmt::Debug for ConversionParsing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversionParsing")
            .field("judged", &self.judgement.get().is_some())
            .finish()
    }
}
impl PartialEq for ConversionParsing {
    // Judgement and diagnostic owners are metadata, not authored syntax.
    fn eq(&self, other: &Self) -> bool {
        self.secret == other.secret
    }
}
impl ConversionParsing {
    pub(super) fn new(secret: bool) -> Self {
        Self {
            judgement: Arc::default(),
            secret,
        }
    }
    pub(crate) fn judgement(&self) -> Option<&ParseJudgement> {
        self.judgement.get()
    }
    pub(super) fn operand_failed(&self) {
        self.judgement.get_or_init(|| ParseJudgement::OperandFailed);
    }
    pub(super) fn reject(&self, site: &ConversionSite) -> bool {
        match self.judgement.get_or_init(|| {
            let error = ConversionError::new(
                Kind::UnsupportedPosition,
                &site.location,
                "conversion is not supported inside this expression",
            )
            .owned_by(site.node);
            ParseJudgement::Rejected(if self.secret {
                error.inside_secret()
            } else {
                error
            })
        }) {
            ParseJudgement::Rejected(_) => true,
            ParseJudgement::OperandFailed => false,
        }
    }
}

/// A conversion's accept or reject verdict for one authored use and borrowed
/// destination. Only conversion policy can mint it; the schema walk consumes it.
///
/// ```compile_fail,E0451
/// use carina_core::parser::retained::ConversionJudgement;
/// fn forge() -> ConversionJudgement {
///     ConversionJudgement { stamp: unreachable!(), location: unreachable!(), verdict: unreachable!() }
/// }
/// ```
/// ```compile_fail,E0277
/// use carina_core::parser::retained::ConversionJudgement;
/// let _: ConversionJudgement = Default::default();
/// ```
/// ```compile_fail,E0277
/// use carina_core::parser::retained::ConversionJudgement;
/// fn duplicate(verdict: ConversionJudgement) { let _ = Clone::clone(&verdict); }
/// ```
pub struct ConversionJudgement {
    stamp: SourceStamp,
    location: DeclarationLocation,
    verdict: ConversionVerdict,
}
pub(crate) struct ConversionVerdict(Box<Decision>);
enum Decision {
    Accepted(AssignmentFindings, AcceptedConversion),
    Rejected(AssignmentFindings),
}
#[derive(Debug)]
pub(crate) struct AcceptedConversion {
    node: NodeId,
    location: DeclarationLocation,
    secret: bool,
}
impl AcceptedConversion {
    pub(crate) fn reject_destination(&self, cause: TypeError) -> ConversionError {
        let error = ConversionError::destination(&self.location, self.node, cause);
        if self.secret {
            error.inside_secret()
        } else {
            error
        }
    }
}
impl ConversionVerdict {
    pub(crate) fn decide<T>(
        self,
        accepted: impl FnOnce(AssignmentFindings, AcceptedConversion) -> T,
        rejected: impl FnOnce(AssignmentFindings) -> T,
    ) -> T {
        match *self.0 {
            Decision::Accepted(findings, owner) => accepted(findings, owner),
            Decision::Rejected(findings) => rejected(findings),
        }
    }
    pub(crate) fn seam_rejection(path: LeafPath, error: TypeError) -> Self {
        Self(Box::new(Decision::Rejected(AssignmentFindings {
            errors: vec![(path, error)],
            ..Default::default()
        })))
    }
    pub(crate) fn invalid_witness(error: ConversionError) -> Self {
        Self(Box::new(Decision::Rejected(AssignmentFindings {
            conversions: std::iter::once(error).collect(),
            ..Default::default()
        })))
    }
}
impl std::fmt::Debug for ConversionJudgement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversionJudgement")
            .field("stamp", &self.stamp)
            .finish_non_exhaustive()
    }
}
impl ConversionJudgement {
    pub(crate) fn consume<B: BindingAddress>(
        self,
        declaration: &CompiledAttributeType,
        cursor: &AuthoredCursor<'_, B>,
    ) -> Result<ConversionVerdict, ConversionError> {
        let matches = match &cursor.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
                self.stamp == SourceStamp::new(usage, declaration)
            }
            CursorKind::Projected(_) => false,
        };
        if !matches {
            return Err(ConversionError::new(
                Kind::Destination,
                &self.location,
                "conversion verdict belongs to another authored use or destination",
            )
            .owned_by(self.stamp.node));
        }
        Ok(self.verdict)
    }
}

fn claimed<B: BindingAddress>(
    usage: &AuthoredUse<'_, B>,
    declaration: &CompiledAttributeType,
    verdict: ConversionVerdict,
) -> AuthoredOutcome {
    AuthoredOutcome::Claimed(ConversionJudgement {
        stamp: SourceStamp::new(usage, declaration),
        location: usage.node().location().authored,
        verdict,
    })
}
fn rejected<B: BindingAddress>(
    usage: &AuthoredUse<'_, B>,
    declaration: &CompiledAttributeType,
    failure: ConversionError,
    matched_destination: bool,
) -> AuthoredOutcome {
    claimed(
        usage,
        declaration,
        ConversionVerdict(Box::new(Decision::Rejected(AssignmentFindings {
            conversions: std::iter::once(failure).collect(),
            matched_destination,
            ..Default::default()
        }))),
    )
}

#[derive(Debug, PartialEq, Eq)]
struct SourceStamp {
    node: NodeId,
    usage: UseId,
    instance: ExpansionInstanceId,
    generation: ProjectionGeneration,
    declaration: *const CompiledAttributeType,
}
impl SourceStamp {
    fn new<B: BindingAddress>(
        usage: &AuthoredUse<'_, B>,
        declaration: &CompiledAttributeType,
    ) -> Self {
        Self {
            node: usage.node().node_id(),
            usage: usage.occurrence(),
            instance: usage.instance(),
            generation: usage.node().generation(),
            declaration,
        }
    }
}

fn error<B: BindingAddress>(
    node: &JudgedNodeRef<'_, B>,
    kind: Kind,
    message: &str,
) -> ConversionError {
    let error =
        ConversionError::new(kind, &node.location().authored, message).owned_by(node.node_id());
    if node.judgement().diagnostic_is_secret() {
        error.inside_secret()
    } else {
        error
    }
}
fn rejection<B: BindingAddress>(
    usage: &AuthoredUse<'_, B>,
    declaration: &CompiledAttributeType,
    kind: Kind,
    message: &str,
) -> AuthoredOutcome {
    rejected(
        usage,
        declaration,
        error(usage.conversion_owner(), kind, message),
        false,
    )
}

/// Shared with the future ordinary discarded-node rule (#4370). This reads
/// static judgement only: no Convert, value, token, or pending obligation.
pub(crate) fn check_scalar_kind<'a>(
    declaration: &CompiledAttributeType,
    judgement: &'a JudgedExpr,
) -> Result<&'a TypeExpr, Kind> {
    let expected = diagnostic::scalar_kind(declaration).ok_or(Kind::Source)?;
    let source = judgement.assignment_source_type().ok_or(Kind::Source)?;
    let actual = match source {
        TypeExpr::String | TypeExpr::StringLiteral(_) => RefinedScalarKind::String,
        TypeExpr::Int => RefinedScalarKind::Int,
        TypeExpr::Float => RefinedScalarKind::Float,
        TypeExpr::SchemaType { owned } => owned.scalar_kind(),
        TypeExpr::Bool
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::Ref(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::UnloadedSchemaType(_)
        | TypeExpr::List(_)
        | TypeExpr::Map(_)
        | TypeExpr::Union(_)
        | TypeExpr::Struct { .. } => return Err(Kind::Source),
    };
    if actual == expected {
        Ok(source)
    } else {
        Err(Kind::ScalarKind)
    }
}

fn destination<'d>(
    declaration: &'d CompiledAttributeType,
    target: &crate::parser::ResolvedSchemaType,
    defs: &'d std::collections::BTreeMap<String, CompiledAttributeType>,
) -> Result<&'d CompiledAttributeType, Kind> {
    match declaration.kind() {
        AttrTypeKind::Union(_) => {
            let members = crate::schema::union_members_with_defs(declaration, defs)
                .ok_or(Kind::Destination)?;
            let mut selected = None;
            for member in members.iter() {
                let member = member.ok_or(Kind::Destination)?.as_attr();
                if crate::parser::IdentifiedCustomTypeDeclaration::new(member)
                    .is_some_and(|d| d.identity() == target.identity())
                {
                    if selected.is_some() {
                        return Err(Kind::AmbiguousDestination);
                    }
                    selected = Some(member);
                }
            }
            selected.ok_or(Kind::Destination)
        }
        AttrTypeKind::String { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Enum { .. }
        | AttrTypeKind::Reference { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::List { .. }
        | AttrTypeKind::Map { .. }
        | AttrTypeKind::Struct { .. }
        | AttrTypeKind::Ref(_)
        | AttrTypeKind::UnloadedSchemaType(_) => {
            if crate::parser::IdentifiedCustomTypeDeclaration::new(declaration)
                .is_some_and(|d| d.identity() == target.identity())
            {
                Ok(declaration)
            } else {
                Err(Kind::Destination)
            }
        }
    }
}

/// The sole authored B1 policy. Syntax navigation and value descent remain in
/// the existing schema walk; this function only reads direct child evidence.
pub(crate) fn check<K: DestinationKind, P: crate::resource::PlaceholderKind, B: BindingAddress>(
    declaration: &CompiledAttributeType,
    checked: CheckValue<'_, P, B>,
    path: &LeafPath,
    cursor: AuthoredCursor<'_, B>,
    defs: &std::collections::BTreeMap<String, CompiledAttributeType>,
    config: &crate::parser::ProviderContext,
) -> Result<AuthoredOutcome, TypeError> {
    let (usage, covered) = match cursor.kind {
        CursorKind::Authored(usage) => (usage, false),
        CursorKind::CoveredBy(usage) => (usage, true),
        CursorKind::Projected(_) => return Ok(AuthoredOutcome::Continue(Default::default())),
    };
    let node = usage.node();
    if node.parse_rejected() {
        return Ok(claimed(
            &usage,
            declaration,
            ConversionVerdict(Box::new(Decision::Rejected(Default::default()))),
        ));
    }
    match node.kind() {
        ExprKind::Convert { target, .. } => {
            let discarded = match checked {
                CheckValue::Current(_) => false,
                CheckValue::Discarded => true,
                CheckValue::Covered => {
                    return Ok(rejection(
                        &usage,
                        declaration,
                        Kind::UnsupportedPosition,
                        "conversion is not supported inside this expression",
                    ));
                }
            };
            if usage.placement_edge() == EdgeRole::ConvertOperand {
                return Ok(rejection(
                    &usage,
                    declaration,
                    Kind::UnsupportedPosition,
                    "a conversion cannot be the operand of another conversion",
                ));
            }
            if covered || usage.placement_edge().restricts_conversion() {
                return Ok(rejection(
                    &usage,
                    declaration,
                    Kind::UnsupportedPosition,
                    "conversion requires an independently typed destination",
                ));
            }
            let operand = match node
                .children()
                .next()
                .unwrap_or_else(|| Err(MissingEvidence::new("conversion operand")))
            {
                Ok(operand) => operand,
                Err(missing) => {
                    let owner = usage.conversion_owner();
                    let failure = error(
                        owner,
                        Kind::Source,
                        "conversion operand has no source judgement",
                    )
                    .with_evidence(missing);
                    let failure = if owner.judgement().diagnostic_is_secret() {
                        failure.inside_secret()
                    } else {
                        failure
                    };
                    return Ok(rejected(&usage, declaration, failure, false));
                }
            };
            let source = operand.judgement();
            if source.is_conversion_output() {
                return Ok(rejection(
                    &usage,
                    declaration,
                    Kind::UnsupportedPosition,
                    "a conversion cannot be the operand of another conversion",
                ));
            }
            let mut matched_destination = false;
            let result = (|| {
                let target =
                    diagnostic::resolve_target(target.detail(), &node.location().authored, config)
                        .map_err(|failure| failure.owned_by(node.node_id()))?;
                let resolved = crate::schema::value_validation::resolve_assignment_declaration(
                    declaration,
                    defs,
                )
                .map_err(|cause| {
                    ConversionError::destination(&node.location().authored, node.node_id(), cause)
                })?;
                let selected = destination(resolved, &target, defs).map_err(|kind| {
                    error(
                        node,
                        kind,
                        "conversion destination must have exactly one matching type identity",
                    )
                })?;
                matched_destination = true;
                let message = if discarded {
                    "discarded conversion operand has an incompatible scalar kind"
                } else {
                    "conversion operand has no compatible scalar source judgement"
                };
                let source_type = check_scalar_kind(selected, source)
                    .map_err(|kind| error(node, kind, message))?;
                if discarded {
                    return Ok(AssignmentFindings::default());
                }
                let known_value =
                    matches!(checked, CheckValue::Current(Value::Concrete(_))) || source.concrete();
                if matches!(resolved.kind(), AttrTypeKind::Union(_)) && !known_value {
                    return Err(error(
                        node,
                        Kind::UnsupportedPosition,
                        "conversion of an unknown value under a union destination is not supported yet",
                    ));
                }
                if let TypeExpr::SchemaType { owned } = source_type {
                    let source_declaration =
                        diagnostic::source_declaration(owned, &node.location().authored, config)
                            .map_err(|failure| failure.owned_by(node.node_id()))?;
                    if let Some(bounds) = DeclarationBounds::from_declaration(selected)
                        && DeclarationBounds::from_declaration(&source_declaration)
                            .is_some_and(|source| source.disjoint(&bounds))
                    {
                        return Err(error(
                            node,
                            Kind::Disjoint,
                            "conversion source and destination declaration bounds are disjoint",
                        ));
                    }
                }
                Ok(crate::validation::admission::conversion_local::<K>(path))
            })();
            Ok(match result {
                Ok(mut findings) => {
                    findings.matched_destination = true;
                    claimed(
                        &usage,
                        declaration,
                        ConversionVerdict(Box::new(Decision::Accepted(
                            findings,
                            AcceptedConversion {
                                node: node.node_id(),
                                location: node.location().authored,
                                secret: node.judgement().diagnostic_is_secret(),
                            },
                        ))),
                    )
                }
                Err(failure) => rejected(
                    &usage,
                    declaration,
                    if node.judgement().diagnostic_is_secret() {
                        failure.inside_secret()
                    } else {
                        failure
                    },
                    matched_destination,
                ),
            })
        }
        ExprKind::If | ExprKind::Coalesce => {
            if !node.judgement().contains_any_conversion() {
                return Ok(AuthoredOutcome::Continue(Default::default()));
            }
            match checked {
                CheckValue::Covered => return Ok(AuthoredOutcome::Continue(Default::default())),
                CheckValue::Current(_) | CheckValue::Discarded => {
                    if covered {
                        return Ok(AuthoredOutcome::Continue(Default::default()));
                    }
                }
            }
            let first = if matches!(node.kind(), ExprKind::If) {
                1
            } else {
                0
            };
            let mut alternatives = node.children().skip(first);
            let initial = alternatives
                .next()
                .transpose()
                .map_err(crate::validation::admission::assignment_seam_error)?;
            if let Some(initial) = initial {
                let converted = initial.judgement().contains_conversion();
                for child in alternatives {
                    if child
                        .map_err(crate::validation::admission::assignment_seam_error)?
                        .judgement()
                        .contains_conversion()
                        != converted
                    {
                        return Ok(rejection(
                            &usage,
                            declaration,
                            Kind::UnsupportedPosition,
                            "conversion and ordinary alternatives cannot share a destination",
                        ));
                    }
                }
            }
            Ok(AuthoredOutcome::Continue(Default::default()))
        }
        ExprKind::Scalar(_)
        | ExprKind::Parameter(_)
        | ExprKind::LoopSlot(_)
        | ExprKind::UntypedParameter
        | ExprKind::Binding(_)
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
        | ExprKind::Alias
        | ExprKind::Parens
        | ExprKind::Definition
        | ExprKind::Element
        | ExprKind::Iterable
        | ExprKind::Resource { .. }
        | ExprKind::Sink { .. }
        | ExprKind::For
        | ExprKind::Wait
        | ExprKind::Compose
        | ExprKind::DynamicAccess
        | ExprKind::ModuleCall
        | ExprKind::Boolean
        | ExprKind::Declaration
        | ExprKind::ProviderDeclaration
        | ExprKind::Unsupported(_) => Ok(AuthoredOutcome::Continue(Default::default())),
    }
}

/// Parsed-node inventory. It is populated by the existing authored index,
/// before any node judgement is requested; unavailable walk evidence cannot
/// remove a Convert from this set.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum InventoryContext {
    Definition,
    Value,
    Boundary,
}
#[derive(Clone)]
pub(crate) struct InventoryEntry {
    node: NodeId,
    environment: ExpressionEnvironmentId,
    owner: ConversionSite,
    definition: bool,
    boundary: bool,
    secret: bool,
    parsing: ConversionParsing,
    roots: std::collections::HashSet<RootRequirement>,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RootRequirement {
    node: NodeId,
    occurrence: UseId,
    instance: ExpansionInstanceId,
    generation: ProjectionGeneration,
}
pub(crate) type ConversionNodes = HashMap<(NodeId, ExpressionEnvironmentId), InventoryEntry>;
pub(crate) enum ConversionInventory {
    Nodes(ConversionNodes, Vec<RootRequirement>),
    Joined(Arc<Self>, Arc<Self>),
}
impl Default for ConversionInventory {
    fn default() -> Self {
        Self::Nodes(Default::default(), Vec::new())
    }
}
impl ConversionInventory {
    // Reports share inventory chunks; joining module reports never copies
    // the inventory accumulated by earlier modules.
    fn entries<'a>(
        &'a self,
        coverage: &'a crate::schema::value_validation::WalkCoverage,
    ) -> impl Iterator<Item = (&'a InventoryEntry, bool)> {
        let mut stack = vec![self];
        let mut current = None;
        let mut unresolved = false;
        std::iter::from_fn(move || {
            loop {
                if let Some(entries) = &mut current
                    && let Some(entry) = std::iter::Iterator::next(entries)
                {
                    return Some((entry, unresolved));
                }
                match stack.pop()? {
                    Self::Nodes(nodes, roots) => {
                        unresolved = roots.iter().any(|root| {
                            !coverage.reached_root(
                                root.node,
                                root.occurrence,
                                root.instance,
                                root.generation,
                            )
                        });
                        current = Some(nodes.values());
                    }
                    Self::Joined(left, right) => {
                        stack.push(right);
                        stack.push(left);
                    }
                }
            }
        })
    }
}

pub(super) type InventoryBuilder =
    HashMap<(NodeId, ExpressionEnvironmentId), std::rc::Rc<std::cell::RefCell<InventoryEntry>>>;
#[derive(Clone, Default)]
pub(super) enum ConversionOrigins {
    #[default]
    Empty,
    Entry(std::rc::Rc<std::cell::RefCell<InventoryEntry>>),
    Joined(std::rc::Rc<Self>, std::rc::Rc<Self>),
}
impl ConversionOrigins {
    pub(super) fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, right) => right,
            (left, Self::Empty) => left,
            (
                left @ (Self::Entry(_) | Self::Joined(..)),
                right @ (Self::Entry(_) | Self::Joined(..)),
            ) => Self::Joined(std::rc::Rc::new(left), std::rc::Rc::new(right)),
        }
    }
    fn entries(&self) -> impl Iterator<Item = &std::rc::Rc<std::cell::RefCell<InventoryEntry>>> {
        let mut stack = vec![self];
        std::iter::from_fn(move || {
            loop {
                match stack.pop()? {
                    Self::Empty => {}
                    Self::Entry(entry) => return Some(entry),
                    Self::Joined(left, right) => {
                        stack.push(right);
                        stack.push(left);
                    }
                }
            }
        })
    }
}
pub(super) fn inventory_entry<B: BindingAddress>(
    node: &AuthoredExpr<ParserPlaceholder, B>,
    parsing: &ConversionParsing,
    context: InventoryContext,
    secret: bool,
    owner: Option<&ConversionSite>,
    inventory: &mut InventoryBuilder,
) -> ConversionOrigins {
    let entry = InventoryEntry {
        node: node.id,
        environment: node.location.instance.id(),
        owner: owner
            .or(node.first_conversion.as_deref())
            .expect("Convert has its authored site")
            .clone(),
        definition: context == InventoryContext::Definition,
        boundary: context == InventoryContext::Boundary,
        secret,
        parsing: parsing.clone(),
        roots: Default::default(),
    };
    let shared = inventory
        .entry((entry.node, entry.environment))
        .or_insert_with(|| std::rc::Rc::new(std::cell::RefCell::new(entry.clone())));
    {
        let mut current = shared.borrow_mut();
        current.definition &= entry.definition;
        current.boundary |= entry.boundary;
        current.secret |= entry.secret;
    }
    ConversionOrigins::Entry(shared.clone())
}
pub(super) fn require_root<B: BindingAddress>(
    root: &cursor::UseRoot<B>,
    origins: Option<&ConversionOrigins>,
    unresolved_roots: &mut Vec<RootRequirement>,
) {
    let requirement = RootRequirement {
        node: cursor::canonical_root_id(&root.node),
        occurrence: root.occurrence,
        instance: root.instance,
        generation: root.generation,
    };
    match origins {
        Some(origins) => {
            for entry in origins.entries() {
                entry.borrow_mut().roots.insert(requirement);
            }
        }
        // An unknown authored root cannot prove any conversion in this parsed
        // inventory. The empty inventory still has no obligations.
        None => unresolved_roots.push(requirement),
    }
}
pub(crate) fn complete_inventory(
    inventory: &ConversionInventory,
    coverage: &crate::schema::value_validation::WalkCoverage,
) -> crate::ConversionChannel {
    inventory
        .entries(coverage)
        .filter_map(|(entry, unresolved)| {
            match entry.parsing.judgement() {
                Some(ParseJudgement::OperandFailed) => return None,
                Some(ParseJudgement::Rejected(error)) => return Some(error.clone()),
                None => {}
            }
            if !unresolved
                && !entry.boundary
                && coverage.conversion_judged(entry.node, entry.environment)
                && entry.roots.iter().all(|root| {
                    coverage.reached_root(
                        root.node,
                        root.occurrence,
                        root.instance,
                        root.generation,
                    )
                })
            {
                return None;
            }
            let (kind, message) = if entry.definition && !entry.boundary && entry.roots.is_empty() {
                (Kind::Unused, "conversion has no typed destination use")
            } else {
                (
                    Kind::UnsupportedPosition,
                    "conversion is not supported at this position",
                )
            };
            let error = ConversionError::new(kind, &entry.owner.location, message)
                .owned_by(entry.owner.node);
            Some(if entry.secret {
                error.inside_secret()
            } else {
                error
            })
        })
        .collect()
}
