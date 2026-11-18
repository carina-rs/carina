//! Checked conversion policy over the walk's authored cursor. No value routing.

pub(crate) mod diagnostic;
pub(crate) mod flow;
mod static_source;
pub(in crate::parser::retained) use static_source::{
    ConversionTypeJudgement, SourceKnowledge, StaticVerdict,
};

use super::*;
use crate::schema::value_validation::{
    AssignmentFindings, AuthoredOutcome, CheckValue, ScalarKindFit, scalar_kind_fit,
};
use crate::schema::{AttrTypeKind, CompiledAttributeType, LeafPath, RefinedScalarKind, TypeError};
use crate::validation::admission::AssignmentKind;
use diagnostic::{
    ConversionError, ConversionErrorKind as Kind, ConversionTypePath, DeclarationBounds,
};

/// Reasons a syntax child has no assignable declaration. This decision does
/// not inspect its value, in either pending or discarded visits.
pub(crate) enum DeclineReason {
    UndeclaredField { field: String },
    FieldAssignment { field: String, error: TypeError },
}
impl DeclineReason {
    fn message(&self) -> String {
        use crate::provider_text::DiagnosticText;
        match self {
            Self::UndeclaredField { field } => format!(
                "conversion cannot be checked: field '{}' is not declared by the destination",
                DiagnosticText::new(field),
            ),
            Self::FieldAssignment { field, error } => format!(
                "conversion cannot be checked: destination field '{}' cannot be assigned: {error}",
                DiagnosticText::new(field),
            ),
        }
    }
}

/// A syntax probe expands each contributing judgement DAG only once. This
/// memo has no syntax/value correspondence or completion responsibility.
pub(crate) struct DeclinedConversions {
    findings: flow::PlacementFindings,
}
impl Default for DeclinedConversions {
    fn default() -> Self {
        Self {
            findings: flow::PlacementFindings::new(Default::default()),
        }
    }
}
impl DeclinedConversions {
    pub(crate) fn reject(
        &mut self,
        child: &JudgedExpr,
        reason: &DeclineReason,
        secret: bool,
    ) -> crate::ConversionChannel {
        match child.conversion_evidence() {
            ConversionEvidence::Free(_proof) => Default::default(),
            ConversionEvidence::Carried(_) => {
                child.conversion_sources().reject(
                    Kind::Destination,
                    &reason.message(),
                    secret || child.diagnostic_is_secret(),
                    &mut self.findings,
                );
                self.findings.take_findings()
            }
        }
    }
}

/// Parse-stage judgement stored on the authored Convert itself.
#[derive(Clone)]
pub(crate) struct ConversionParsing {
    judgement: Arc<std::sync::OnceLock<ParseJudgement>>,
    secret: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParseJudgement {
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
    pub(super) fn operand_failed(&self) {
        self.judgement.get_or_init(|| ParseJudgement::OperandFailed);
    }
    pub(super) fn recoverable(&self) -> bool {
        self.judgement.get().is_none()
    }
}

/// A template preflight suppresses the ordinary judgement without judging a Convert.
/// Only the conversion policy, supplied a sealed preflight boundary, can mint it.
/// The instance walk admits values; an uninstantiated module produces none.
/// Live policy here would double-report two parses and reject unknown unions.
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
    if let Some(proof) = cursor.no_conversion() {
        return AuthoredOutcome::Continue(proof, Default::default());
    }
    match &cursor.kind {
        CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
            match TransportJudgement::new(usage) {
                Ok(transport) => AuthoredOutcome::Transport(transport),
                Err(_) => AuthoredOutcome::Deferred(TemplateDeferral { _private: () }),
            }
        }
        CursorKind::Projected(
            ProjectionAuthority::Validation(_) | ProjectionAuthority::Operational(_),
        ) => AuthoredOutcome::Deferred(TemplateDeferral { _private: () }),
    }
}

/// Sealed permission to descend a conversion-carrying transport. A leaf can
/// never mint this permission instead of judging its Convert.
#[derive(Debug)]
pub(crate) struct TransportJudgement {
    identity: super::cursor::AuthoredUseIdentity,
    owner: Arc<ConversionSite>,
}
pub(crate) struct TransportDescent {
    _private: (),
}
impl TransportDescent {
    pub(crate) fn consume(self) {}
}
impl TransportJudgement {
    fn new<B: BindingAddress>(usage: &AuthoredUse<'_, B>) -> Result<Self, MissingEvidence> {
        let flow::PlacementRole::Transport(_) = usage.node().conversion_role() else {
            return Err(MissingEvidence::new("conversion transport role"));
        };
        match usage.navigation()? {
            AuthoredNavigation::Forward { .. }
            | AuthoredNavigation::Alternatives { .. }
            | AuthoredNavigation::List
            | AuthoredNavigation::Map => {}
            AuthoredNavigation::Secret(_)
            | AuthoredNavigation::Parts
            | AuthoredNavigation::Leaf
            | AuthoredNavigation::Covered => {
                return Err(MissingEvidence::new("conversion transport child"));
            }
        }
        let site = usage
            .node()
            .judgement()
            .conversion_sources()
            .first()
            .ok_or_else(|| MissingEvidence::new("conversion transport source"))?;
        Ok(Self {
            identity: usage.identity(),
            owner: Arc::new(site.clone()),
        })
    }
    pub(crate) fn consume<B: BindingAddress>(
        self,
        cursor: &AuthoredCursor<'_, B>,
    ) -> Result<TransportDescent, ConversionError> {
        if matches!(&cursor.kind, CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) if usage.identity() == self.identity)
        {
            Ok(TransportDescent { _private: () })
        } else {
            Err(ConversionError::new(
                Kind::Source,
                &self.owner.location,
                "conversion transport belongs to another authored use",
            )
            .owned_by(self.owner.node))
        }
    }
}
fn pass<B: BindingAddress>(
    usage: &AuthoredUse<'_, B>,
    declaration: &CompiledAttributeType,
) -> AuthoredOutcome {
    if let Some(proof) = usage.node().judgement().no_conversion() {
        return AuthoredOutcome::Continue(proof, Default::default());
    }
    match TransportJudgement::new(usage) {
        Ok(transport) => AuthoredOutcome::Transport(transport),
        Err(_) => rejection(
            usage,
            declaration,
            Kind::UnsupportedPosition,
            "conversion cannot be checked here",
        ),
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
    fn unchecked(source: &JudgedExpr, carried: flow::CarriedConversion<'_>) -> Self {
        Self(Box::new(Decision::Rejected(AssignmentFindings {
            conversions: Self::unavailable_conversion(source, carried),
            ..Default::default()
        })))
    }
    fn unavailable_conversion(
        source: &JudgedExpr,
        carried: flow::CarriedConversion<'_>,
    ) -> crate::ConversionChannel {
        let site = carried.first();
        let error = ConversionError::new(
            Kind::UnsupportedPosition,
            &site.location,
            "conversion cannot be checked here",
        )
        .owned_by(site.node);
        std::iter::once(if source.diagnostic_is_secret() {
            error.inside_secret()
        } else {
            error
        })
        .collect()
    }
    /// The same proof failure as a validation projection without navigation.
    pub(crate) fn unavailable_source(source: &JudgedExpr) -> crate::ConversionChannel {
        if source.statically_checked() {
            return Default::default();
        }
        match source.conversion_evidence() {
            ConversionEvidence::Free(_proof) => Default::default(),
            ConversionEvidence::Carried(carried) => Self::unavailable_conversion(source, carried),
        }
    }
    pub(crate) fn proof_mismatch<B: BindingAddress>(
        cursor: &AuthoredCursor<'_, B>,
    ) -> Result<Self, TypeError> {
        match &cursor.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
                match usage.node().judgement().conversion_evidence() {
                    ConversionEvidence::Carried(carried) => {
                        Ok(Self::unchecked(usage.node().judgement(), carried))
                    }
                    ConversionEvidence::Free(_) => {
                        Err(crate::validation::admission::assignment_seam_error(
                            MissingEvidence::new("ordinary judgement proof/cursor pairing"),
                        ))
                    }
                }
            }
            CursorKind::Projected(ProjectionAuthority::Validation(projection)) => {
                match projection.evidence() {
                    super::cursor::ProjectionEvidence::Carried(source, carried) => {
                        Ok(Self::unchecked(source, carried))
                    }
                    super::cursor::ProjectionEvidence::Free(_) => {
                        Err(crate::validation::admission::assignment_seam_error(
                            MissingEvidence::new("ordinary judgement proof/cursor pairing"),
                        ))
                    }
                }
            }
            CursorKind::Projected(ProjectionAuthority::Operational(_)) => {
                Err(crate::validation::admission::assignment_seam_error(
                    MissingEvidence::new("ordinary judgement proof/cursor pairing"),
                ))
            }
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
        let matches = SourceStamp::of(cursor, declaration).as_ref() == Some(&self.stamp);
        if !matches {
            return Err(ConversionError::new(
                Kind::Destination,
                &self.location,
                "conversion verdict belongs to another authored use or destination",
            )
            .owned_by(self.stamp.node()));
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
enum SourceStamp {
    Authored {
        node: NodeId,
        usage: UseId,
        instance: ExpansionInstanceId,
        generation: ProjectionGeneration,
        declaration: *const CompiledAttributeType,
    },
    Validation {
        node: NodeId,
        declaration: *const CompiledAttributeType,
        source: usize,
    },
}
impl SourceStamp {
    fn new<B: BindingAddress>(
        usage: &AuthoredUse<'_, B>,
        declaration: &CompiledAttributeType,
    ) -> Self {
        Self::Authored {
            node: usage.node().node_id(),
            usage: usage.occurrence(),
            instance: usage.instance(),
            generation: usage.node().generation(),
            declaration,
        }
    }
    fn of<B: BindingAddress>(
        cursor: &AuthoredCursor<'_, B>,
        declaration: &CompiledAttributeType,
    ) -> Option<Self> {
        match &cursor.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
                Some(Self::new(usage, declaration))
            }
            CursorKind::Projected(ProjectionAuthority::Validation(projection)) => {
                match projection.evidence() {
                    super::cursor::ProjectionEvidence::Carried(source, carried) => {
                        Some(Self::Validation {
                            node: carried.first().node,
                            declaration,
                            source: source.judgement_stamp(),
                        })
                    }
                    super::cursor::ProjectionEvidence::Free(_) => None,
                }
            }
            CursorKind::Projected(ProjectionAuthority::Operational(_)) => None,
        }
    }
    fn node(&self) -> NodeId {
        match self {
            Self::Authored { node, .. } | Self::Validation { node, .. } => *node,
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
        match usage.node().kind() {
            ExprKind::Convert { .. } => error(usage.conversion_owner(), kind, message),
            ExprKind::If | ExprKind::Coalesce => {
                let failure = error(usage.node(), kind, message);
                match usage.node().judgement().conversion_sources().first() {
                    Some(site) => failure.owned_by(site.node),
                    None => failure,
                }
            }
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
            | ExprKind::RequireCondition(_)
            | ExprKind::ProviderDeclaration
            | ExprKind::Unsupported(_) => {
                match usage.node().judgement().conversion_sources().first() {
                    Some(site) => {
                        let failure =
                            ConversionError::new(kind, &site.location, message).owned_by(site.node);
                        if usage.node().judgement().diagnostic_is_secret() {
                            failure.inside_secret()
                        } else {
                            failure
                        }
                    }
                    None => error(usage.conversion_owner(), kind, message),
                }
            }
        },
        false,
    )
}

/// Shared with the future ordinary discarded-node rule (#4370). This reads
/// static judgement only: no Convert, value, token, or pending obligation.
pub(crate) enum ScalarKindCheck<'a> {
    Compatible(CompatibleScalar<'a>),
    WrongScalarKind,
    NotScalar,
    NoStaticJudgement,
}

pub(crate) enum ScalarSource<'a> {
    Judged(&'a JudgedExpr),
    Declaration(crate::schema::TypeInSchema<'a, crate::schema::Compiled>),
}

pub(crate) enum CompatibleScalar<'a> {
    Judged(&'a TypeExpr),
    Declaration(crate::schema::ResolvedAttrType<'a, crate::schema::Compiled>),
}

pub(crate) struct ConversionContext<'a> {
    pub(crate) catalog: &'a crate::parser::ProviderContext,
    pub(crate) reference: crate::validation::admission::ReferenceSource,
}

pub(crate) fn check_scalar_kind<'a>(
    declaration: &CompiledAttributeType,
    evidence: ScalarSource<'a>,
) -> ScalarKindCheck<'a> {
    let Some(expected) = diagnostic::scalar_kind(declaration) else {
        return ScalarKindCheck::NotScalar;
    };
    let (actual, source) = match evidence {
        ScalarSource::Declaration(source) => {
            let source = match source.resolve() {
                Ok(source) => source,
                Err(_) => return ScalarKindCheck::NoStaticJudgement,
            };
            let actual = match diagnostic::scalar_kind(source.as_attr()) {
                Some(kind) => kind,
                None => match source.as_attr().kind() {
                    AttrTypeKind::StringLiteralSet { .. } | AttrTypeKind::Enum { .. } => {
                        RefinedScalarKind::String
                    }
                    AttrTypeKind::UnloadedSchemaType(_)
                    | AttrTypeKind::Union(_)
                    | AttrTypeKind::Ref(_) => return ScalarKindCheck::NoStaticJudgement,
                    AttrTypeKind::String { .. }
                    | AttrTypeKind::Int { .. }
                    | AttrTypeKind::Float { .. }
                    | AttrTypeKind::Reference { .. }
                    | AttrTypeKind::ResourceHandle { .. }
                    | AttrTypeKind::Bool
                    | AttrTypeKind::Duration
                    | AttrTypeKind::List { .. }
                    | AttrTypeKind::Map { .. }
                    | AttrTypeKind::Struct { .. } => return ScalarKindCheck::NotScalar,
                },
            };
            (actual, CompatibleScalar::Declaration(source))
        }
        ScalarSource::Judged(judgement) => {
            let Some(source) = judgement.assignment_source_type() else {
                return ScalarKindCheck::NoStaticJudgement;
            };
            let actual = match source {
                TypeExpr::String | TypeExpr::StringLiteral(_) => RefinedScalarKind::String,
                TypeExpr::Int => RefinedScalarKind::Int,
                TypeExpr::Float => RefinedScalarKind::Float,
                TypeExpr::SchemaType { owned } => owned.scalar_kind(),
                TypeExpr::Bool
                | TypeExpr::Duration
                | TypeExpr::Ref(_)
                | TypeExpr::List(_)
                | TypeExpr::Map(_)
                | TypeExpr::Struct { .. } => return ScalarKindCheck::NotScalar,
                TypeExpr::Simple(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::UnloadedSchemaType(_)
                | TypeExpr::Union(_) => return ScalarKindCheck::NoStaticJudgement,
            };
            (actual, CompatibleScalar::Judged(source))
        }
    };
    match scalar_kind_fit(expected, actual) {
        ScalarKindFit::Exact | ScalarKindFit::IntToFloat => ScalarKindCheck::Compatible(source),
        ScalarKindFit::Rejected => ScalarKindCheck::WrongScalarKind,
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
pub(crate) fn check<K: AssignmentKind, P: crate::resource::PlaceholderKind, B: BindingAddress>(
    declaration: &CompiledAttributeType,
    checked: CheckValue<'_, P, B>,
    path: &LeafPath,
    cursor: AuthoredCursor<'_, B>,
    defs: &std::collections::BTreeMap<String, CompiledAttributeType>,
    context: ConversionContext<'_>,
) -> Result<AuthoredOutcome, TypeError> {
    let (usage, covered) = match &cursor.kind {
        CursorKind::Authored(usage) => (usage.clone(), false),
        CursorKind::CoveredBy(usage) => (usage.clone(), true),
        CursorKind::Projected(ProjectionAuthority::Operational(phase)) => {
            return Ok(AuthoredOutcome::Continue(
                NoConversion::operational(*phase),
                Default::default(),
            ));
        }
        CursorKind::Projected(ProjectionAuthority::Validation(projection)) => {
            match projection.evidence() {
                super::cursor::ProjectionEvidence::Free(proof) => {
                    return Ok(AuthoredOutcome::Continue(proof, Default::default()));
                }
                super::cursor::ProjectionEvidence::Carried(source, carried) => {
                    let site = carried.first();
                    return Ok(AuthoredOutcome::Claimed(ConversionJudgement {
                        stamp: SourceStamp::Validation {
                            node: site.node,
                            declaration,
                            source: source.judgement_stamp(),
                        },
                        location: site.location.clone(),
                        verdict: match source.conversion_type_judgement() {
                            Some(record) => match record.owner(declaration) {
                                static_source::StaticOwner::JudgedStatically(verdict) => verdict
                                    .resume::<K>(
                                        path,
                                        static_source::SourceKnowledge::at(checked, source),
                                        source,
                                        &context,
                                    ),
                                static_source::StaticOwner::WalkOwned => {
                                    ConversionVerdict::unchecked(source, carried)
                                }
                                static_source::StaticOwner::Missing(error) => {
                                    ConversionVerdict::invalid_witness(error)
                                }
                            },
                            None => match source.static_rejection() {
                                Some(_rejection) => ConversionVerdict(Box::new(
                                    Decision::Rejected(AssignmentFindings {
                                        claim_effect:
                                            crate::schema::value_validation::ClaimEffect::Rejected,
                                        ..Default::default()
                                    }),
                                )),
                                None => ConversionVerdict::unchecked(source, carried),
                            },
                        },
                    }));
                }
            }
        }
    };
    let node = usage.node();
    if node.judgement().contains_any_conversion()
        && matches!(
            node.conversion_role(),
            flow::PlacementRole::Computed
                | flow::PlacementRole::Boundary
                | flow::PlacementRole::SecretArgument
        )
    {
        return Ok(rejection(
            &usage,
            declaration,
            Kind::UnsupportedPosition,
            "conversion is not supported at this position",
        ));
    }
    match node.kind() {
        ExprKind::Convert { .. } => {
            match checked {
                CheckValue::Current(_) | CheckValue::Pending | CheckValue::Discarded => {}
                CheckValue::Covered => {
                    return Ok(rejection(
                        &usage,
                        declaration,
                        Kind::UnsupportedPosition,
                        "conversion is not supported inside this expression",
                    ));
                }
            }
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
            if source.contains_any_conversion() {
                return Ok(rejection(
                    &usage,
                    declaration,
                    Kind::UnsupportedPosition,
                    "a conversion cannot be the operand of another conversion",
                ));
            }
            use static_source::{SourceKnowledge, StaticOwner};
            let knowledge = SourceKnowledge::at(checked, source);
            let Some(record) = node.judgement().conversion_type_judgement() else {
                return Ok(rejection(
                    &usage,
                    declaration,
                    Kind::Source,
                    "conversion has no static source judgement",
                ));
            };
            let verdict = match record.owner(declaration) {
                StaticOwner::WalkOwned => record
                    .check_walk(source, declaration, defs, &context, knowledge)
                    .consume::<K>(path, knowledge, static_source::VerdictOrigin::Walk),
                StaticOwner::JudgedStatically(verdict) => {
                    verdict.resume::<K>(path, knowledge, source, &context)
                }
                StaticOwner::Missing(error) => {
                    return Ok(rejected(&usage, declaration, error, false));
                }
            };
            Ok(claimed(&usage, declaration, verdict))
        }

        ExprKind::If | ExprKind::Coalesce => {
            if !node.judgement().contains_any_conversion() {
                return Ok(pass(&usage, declaration));
            }
            match checked {
                CheckValue::Covered => return Ok(pass(&usage, declaration)),
                CheckValue::Current(_) | CheckValue::Pending | CheckValue::Discarded => {
                    if covered {
                        return Ok(pass(&usage, declaration));
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
                let converted = initial.judgement().contains_any_conversion();
                for child in alternatives {
                    if child
                        .map_err(crate::validation::admission::assignment_seam_error)?
                        .judgement()
                        .contains_any_conversion()
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
            Ok(pass(&usage, declaration))
        }
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
        | ExprKind::RequireCondition(_)
        | ExprKind::ProviderDeclaration
        | ExprKind::Unsupported(_) => Ok(pass(&usage, declaration)),
    }
}
