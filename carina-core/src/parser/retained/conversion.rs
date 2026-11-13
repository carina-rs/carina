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

/// A template preflight suppresses the ordinary judgement without judging a Convert.
/// Only the conversion policy, supplied a sealed preflight boundary, can mint it.
///
/// ```compile_fail,E0451
/// use carina_core::parser::retained::TemplateDeferral;
/// let _ = TemplateDeferral { _private: () };
/// ```
#[derive(Debug)]
pub struct TemplateDeferral {
    _private: (),
}
impl TemplateDeferral {
    pub(crate) fn consume(self) {}
}

pub(crate) type TemplateOutcome = AuthoredOutcome<
    std::convert::Infallible,
    TemplateDeferral,
    crate::schema::value_validation::NoConversions,
>;

pub(crate) fn check_template<B: BindingAddress>(
    _boundary: crate::validation::admission::TemplatePreflight,
    _declaration: &CompiledAttributeType,
    cursor: &AuthoredCursor<'_, B>,
) -> TemplateOutcome {
    match &cursor.kind {
        CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => match usage.form() {
            AuthoredForm::Convert => AuthoredOutcome::Deferred(TemplateDeferral { _private: () }),
            AuthoredForm::If
            | AuthoredForm::Coalesce
            | AuthoredForm::Forward
            | AuthoredForm::Sink
            | AuthoredForm::Parens
            | AuthoredForm::Definition
            | AuthoredForm::Binding
            | AuthoredForm::Reference
            | AuthoredForm::List
            | AuthoredForm::Map
            | AuthoredForm::Secret
            | AuthoredForm::Literal
            | AuthoredForm::Computed => AuthoredOutcome::Continue(Default::default()),
        },
        CursorKind::Projected(_) => AuthoredOutcome::Continue(Default::default()),
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
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordedVerdict {
    rejected: bool,
}
impl RecordedVerdict {
    pub(crate) fn rejecting(self) -> bool {
        self.rejected
    }
}

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
    pub(crate) fn recorded(&self) -> RecordedVerdict {
        RecordedVerdict {
            rejected: match self.0.as_ref() {
                Decision::Accepted(..) => false,
                Decision::Rejected(_) => true,
            },
        }
    }
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
    target: Box<Located<crate::conversion::ConversionTypePath>>,
    definition: bool,
    boundary: bool,
    secret: bool,
    parsing: ConversionParsing,
    omitted: Option<NotExpanded>,
    intrinsic: Option<ConversionError>,
    roots: std::collections::HashSet<RootRequirement>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RootRequirement {
    pub(crate) node: NodeId,
    pub(crate) occurrence: UseId,
    pub(crate) instance: ExpansionInstanceId,
    pub(crate) generation: ProjectionGeneration,
    pub(crate) omitted: Option<NotExpanded>,
}
impl RootRequirement {
    pub(super) fn new<B: BindingAddress>(root: &cursor::UseRoot<B>) -> Self {
        Self {
            node: cursor::canonical_root_id(&root.node),
            occurrence: root.occurrence,
            instance: root.instance,
            generation: root.generation,
            omitted: root.node.expansion.omission(&root.environment),
        }
    }
}
pub(crate) type ConversionNodes = HashMap<(NodeId, ExpressionEnvironmentId), InventoryEntry>;
#[derive(Default)]
pub(crate) enum ConversionInventory {
    #[default]
    Empty,
    Nodes {
        nodes: ConversionNodes,
        config: Arc<super::super::ProviderContext>,
    },
    Joined(Arc<Self>, Arc<Self>),
    Template {
        inventory: Arc<Self>,
        activation: crate::module_resolver::TemplateActivation,
        subject: crate::validation::FindingSubject,
    },
}
#[derive(Clone, Copy)]
enum InventoryRole<'a> {
    Authored,
    Template(
        &'a crate::validation::FindingSubject,
        &'a crate::module_resolver::TemplateActivation,
    ),
}
impl ConversionInventory {
    // Chunks retain each producer's use scope; ConversionFindings dedups rejections by NodeId.
    fn chunks(
        &self,
    ) -> impl Iterator<
        Item = (
            &ConversionNodes,
            &super::super::ProviderContext,
            InventoryRole<'_>,
        ),
    > {
        let mut stack = vec![(self, InventoryRole::Authored)];
        std::iter::from_fn(move || {
            loop {
                let (inventory, role) = stack.pop()?;
                match inventory {
                    Self::Empty => {}
                    Self::Nodes { nodes, config } => return Some((nodes, config.as_ref(), role)),
                    Self::Joined(left, right) => {
                        stack.push((right, role));
                        stack.push((left, role));
                    }
                    Self::Template {
                        inventory,
                        activation,
                        subject,
                    } => {
                        stack.push((inventory, InventoryRole::Template(subject, activation)));
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
pub(super) struct InventoryUse<'a> {
    pub(super) context: InventoryContext,
    pub(super) secret: bool,
    pub(super) owner: Option<&'a ConversionSite>,
    pub(super) omitted: Option<NotExpanded>,
}
pub(super) fn inventory_entry<B: BindingAddress>(
    node: &AuthoredExpr<ParserPlaceholder, B>,
    target: &Located<crate::conversion::ConversionTypePath>,
    parsing: &ConversionParsing,
    usage: InventoryUse<'_>,
    inventory: &mut InventoryBuilder,
) -> ConversionOrigins {
    let InventoryUse {
        context,
        secret,
        owner,
        omitted,
    } = usage;
    let entry = InventoryEntry {
        node: node.id,
        environment: node.location.instance.id(),
        owner: owner
            .or(node.first_conversion.as_deref())
            .expect("Convert has its authored site")
            .clone(),
        target: Box::new(target.clone()),
        definition: context == InventoryContext::Definition,
        boundary: context == InventoryContext::Boundary,
        secret,
        parsing: parsing.clone(),
        omitted,
        intrinsic: None,
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
        current.omitted = current.omitted.or(omitted);
    }
    ConversionOrigins::Entry(shared.clone())
}
/// Finalize syntax-owned verdicts from the enclosing grammar roles and actual
/// syntactic consumer visits, independently of resource admission/coverage.
pub(super) fn finalize_entry(entry: &std::cell::RefCell<InventoryEntry>) -> InventoryEntry {
    let mut entry = entry.borrow().clone();
    let rejection = if entry.boundary {
        Some((
            Kind::UnsupportedPosition,
            "conversion is not supported at this position yet",
        ))
    } else if entry.definition {
        Some((Kind::Unused, "conversion has no typed destination use"))
    } else {
        None
    };
    if let Some((kind, message)) = rejection {
        let error =
            ConversionError::new(kind, &entry.owner.location, message).owned_by(entry.owner.node);
        entry.intrinsic = Some(if entry.secret {
            error.inside_secret()
        } else {
            error
        });
    }
    entry
}
pub(super) fn require_root<B: BindingAddress>(
    root: &cursor::UseRoot<B>,
    origins: Option<&ConversionOrigins>,
) {
    if let Some(origins) = origins {
        let requirement = RootRequirement::new(root);
        for entry in origins.entries() {
            entry.borrow_mut().roots.insert(requirement);
        }
    }
}

/// A total outcome is supplied by a policy or by the producer that did not
/// produce a live destination. There is deliberately no default/miss verdict.
enum UseOutcome<'a> {
    Verdict(RecordedVerdict),
    NotExpanded(NotExpanded),
    DestinationUnknown(&'a crate::validation::presentation::DestinationUnknown),
    NotAdmitted(crate::validation::presentation::NotAdmitted),
}
#[derive(Clone, Copy)]
pub(crate) enum InventoryCompletion {
    Complete,
    MissingOutcome,
}

fn in_inventory_role(error: ConversionError, role: InventoryRole<'_>) -> ConversionError {
    match role {
        InventoryRole::Authored => error,
        InventoryRole::Template(subject, _) => error.in_subject(subject.clone()),
    }
}

pub(crate) fn complete_inventory(
    inventory: &ConversionInventory,
    coverage: &crate::schema::value_validation::WalkCoverage,
    admission: crate::validation::presentation::InventoryAdmission<'_>,
) -> (crate::ConversionChannel, InventoryCompletion) {
    use crate::validation::presentation::{InventoryAdmission, NotAdmitted};
    let mut errors = crate::ConversionChannel::default();
    let mut completion = InventoryCompletion::Complete;
    for (nodes, config, role) in inventory.chunks() {
        for entry in nodes.values() {
            let intrinsic = match entry.parsing.judgement() {
                Some(ParseJudgement::Rejected(error)) => Some(error),
                Some(ParseJudgement::OperandFailed) => continue,
                None => entry.intrinsic.as_ref(),
            };
            if let Some(error) = intrinsic {
                errors.append(std::iter::once(in_inventory_role(error.clone(), role)).collect());
                continue;
            }
            // An activated template is a reusable owner, not an additional use.
            // The resolver registered each instance's own inventory/roots.
            if let InventoryRole::Template(
                _,
                crate::module_resolver::TemplateActivation::Activated(_),
            ) = role
            {
                continue;
            }
            let non_admitted = match admission {
                InventoryAdmission::Stopped(witness) | InventoryAdmission::ReadOnly(witness) => {
                    Some(witness)
                }
                InventoryAdmission::Admitted(_) | InventoryAdmission::Required => None,
            };
            let template = match role {
                InventoryRole::Authored => None,
                InventoryRole::Template(
                    _,
                    crate::module_resolver::TemplateActivation::NotExpanded(witness),
                ) => Some(NotExpanded::module_template(witness)),
                InventoryRole::Template(
                    _,
                    crate::module_resolver::TemplateActivation::Activated(_),
                ) => None,
            };
            let finish = |outcome: Option<UseOutcome<'_>>| -> Result<(), Option<ConversionError>> {
                match outcome {
                    Some(UseOutcome::Verdict(verdict)) => {
                        let _rejected = verdict.rejecting();
                        Ok(())
                    }
                    Some(UseOutcome::NotExpanded(omission)) => {
                        let _reason = omission.reason;
                        diagnostic::resolve_target(
                            entry.target.detail(),
                            &entry.owner.location,
                            config,
                        )
                        .map(|_| ())
                        .map_err(|error| {
                            Some(in_inventory_role(error.owned_by(entry.owner.node), role))
                        })
                    }
                    Some(UseOutcome::DestinationUnknown(witness)) => {
                        witness.observed();
                        Ok(())
                    }
                    Some(UseOutcome::NotAdmitted(witness)) => {
                        match witness {
                            NotAdmitted::Ordinary(_) | NotAdmitted::ReadOnly(_) => {}
                        }
                        Ok(())
                    }
                    None => Err(None),
                }
            };
            let mut outcomes = Vec::new();
            if let Some(witness) = non_admitted {
                outcomes.push(Some(UseOutcome::NotAdmitted(witness)));
            } else if let Some(omission) = template {
                outcomes.push(Some(UseOutcome::NotExpanded(omission)));
            } else if entry.roots.is_empty() {
                outcomes.push(entry.omitted.map(UseOutcome::NotExpanded));
            } else {
                for root in &entry.roots {
                    let outcome = if let Some(omission) = root.omitted {
                        Some(UseOutcome::NotExpanded(omission))
                    } else if let Some(witness) = coverage.unknown_destination(root) {
                        Some(UseOutcome::DestinationUnknown(witness))
                    } else {
                        match admission {
                            InventoryAdmission::Admitted(witness) => {
                                witness.observed();
                                coverage
                                    .conversion_outcome(entry.node, entry.environment, root)
                                    .map(UseOutcome::Verdict)
                            }
                            InventoryAdmission::Required => None,
                            InventoryAdmission::Stopped(witness)
                            | InventoryAdmission::ReadOnly(witness) => {
                                Some(UseOutcome::NotAdmitted(witness))
                            }
                        }
                    };
                    outcomes.push(outcome);
                }
            }
            for outcome in outcomes {
                match finish(outcome) {
                    Ok(()) => {}
                    Err(Some(error)) => errors.append(std::iter::once(error).collect()),
                    Err(None) => completion = InventoryCompletion::MissingOutcome,
                }
            }
        }
    }
    (errors, completion)
}
