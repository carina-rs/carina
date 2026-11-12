//! Checked conversion policy over the walk's authored cursor. No value routing.

use super::*;
use crate::conversion::{ConversionError, ConversionErrorKind as Kind, DeclarationBounds};
use crate::schema::value_validation::{AssignmentFindings, AuthoredOutcome, CheckValue};
use crate::schema::{AttrTypeKind, CompiledAttributeType, LeafPath, RefinedScalarKind, TypeError};
use crate::validation::admission::DestinationKind;

/// A matched source is consumed only at its exact authored use and borrowed
/// declaration. It carries no value, placement receipt, or reusable authority.
///
/// ```compile_fail,E0451
/// use carina_core::parser::retained::MatchedConversionSource;
/// fn forge() -> MatchedConversionSource {
///     MatchedConversionSource { stamp: unreachable!(), location: unreachable!() }
/// }
/// ```
/// ```compile_fail,E0616
/// use carina_core::parser::retained::MatchedConversionSource;
/// fn read(source: MatchedConversionSource) { let _ = source.stamp; }
/// ```
/// ```compile_fail,E0616
/// use carina_core::parser::retained::MatchedConversionSource;
/// fn change(mut source: MatchedConversionSource) { source.stamp = unreachable!(); }
/// ```
/// ```compile_fail,E0277
/// use carina_core::parser::retained::MatchedConversionSource;
/// let _: MatchedConversionSource = Default::default();
/// ```
/// ```compile_fail,E0277
/// use carina_core::parser::retained::MatchedConversionSource;
/// let _: MatchedConversionSource = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail,E0277
/// use carina_core::parser::retained::MatchedConversionSource;
/// fn duplicate(source: MatchedConversionSource) { let _ = Clone::clone(&source); }
/// ```
pub struct MatchedConversionSource {
    stamp: SourceStamp,
    location: DeclarationLocation,
}

/// A conversion's accept or reject verdict for one authored use and borrowed
/// destination. Only conversion policy can mint it; the schema walk consumes it.
///
/// ```compile_fail,E0451
/// use carina_core::parser::retained::ConversionJudgement;
/// fn forge() -> ConversionJudgement {
///     ConversionJudgement { stamp: unreachable!(), location: unreachable!(), verdict: unreachable!(), destination_matched: true }
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
    destination_matched: bool,
}
enum ConversionVerdict {
    Accepted(AssignmentFindings),
    Rejected(AssignmentFindings),
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
    ) -> Result<AssignmentFindings, TypeError> {
        let matches = match &cursor.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
                self.stamp == SourceStamp::new(usage, declaration)
            }
            CursorKind::Projected(_) => false,
        };
        if !matches {
            return Err(typed(
                ConversionError::new(
                    Kind::Destination,
                    &self.location,
                    "conversion verdict belongs to another authored use or destination",
                )
                .owned_by(self.stamp.node),
            ));
        }
        let mut findings = match self.verdict {
            ConversionVerdict::Accepted(findings) | ConversionVerdict::Rejected(findings) => {
                findings
            }
        };
        findings.matched_destination |= self.destination_matched;
        Ok(findings)
    }
}

fn claimed<B: BindingAddress>(
    usage: &AuthoredUse<'_, B>,
    declaration: &CompiledAttributeType,
    findings: AssignmentFindings,
    destination_matched: bool,
) -> AuthoredOutcome {
    let verdict = if findings.errors.is_empty() {
        ConversionVerdict::Accepted(findings)
    } else {
        ConversionVerdict::Rejected(findings)
    };
    AuthoredOutcome::Claimed(ConversionJudgement {
        stamp: SourceStamp::new(usage, declaration),
        location: usage.node().location().authored,
        verdict,
        destination_matched,
    })
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
impl std::fmt::Debug for MatchedConversionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MatchedConversionSource")
            .field(&self.stamp)
            .finish()
    }
}
impl MatchedConversionSource {
    pub(crate) fn consume<B: BindingAddress>(
        self,
        usage: &AuthoredUse<'_, B>,
        declaration: &CompiledAttributeType,
    ) -> Result<(), ConversionError> {
        if self.stamp != SourceStamp::new(usage, declaration) {
            return Err(ConversionError::new(
                Kind::Destination,
                &self.location,
                "conversion source belongs to another authored use or destination",
            )
            .owned_by(self.stamp.node));
        }
        Ok(())
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
fn typed(error: ConversionError) -> TypeError {
    TypeError::Conversion {
        error,
        attribute: None,
    }
}
fn rejection<B: BindingAddress>(
    usage: &AuthoredUse<'_, B>,
    declaration: &CompiledAttributeType,
    kind: Kind,
    message: &str,
    path: &LeafPath,
) -> AuthoredOutcome {
    claimed(
        usage,
        declaration,
        AssignmentFindings {
            errors: vec![(path.clone(), typed(error(usage.node(), kind, message)))],
            ..Default::default()
        },
        false,
    )
}

/// Shared with the future ordinary discarded-node rule (#4370). This reads
/// static judgement only: no Convert, value, token, or pending obligation.
pub(crate) fn check_scalar_kind<'a>(
    declaration: &CompiledAttributeType,
    judgement: &'a JudgedExpr,
) -> Result<&'a TypeExpr, Kind> {
    let expected = crate::conversion::scalar_kind(declaration).ok_or(Kind::Source)?;
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

fn placement(edge: EdgeRole) -> Result<(), Kind> {
    match edge {
        EdgeRole::SecretArgument
        | EdgeRole::ConvertOperand
        | EdgeRole::InterpolationPart
        | EdgeRole::CallArgument
        | EdgeRole::ArithmeticOperand
        | EdgeRole::AccessBase
        | EdgeRole::AccessIndex
        | EdgeRole::MapKey
        | EdgeRole::Condition => Err(Kind::UnsupportedPosition),
        EdgeRole::SlotRoot
        | EdgeRole::Parens
        | EdgeRole::IfBranch
        | EdgeRole::CoalesceOperand
        | EdgeRole::AliasDefinition
        | EdgeRole::ListItem
        | EdgeRole::MapValue
        | EdgeRole::StructField
        | EdgeRole::SequenceResult => Ok(()),
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
    match node.kind() {
        ExprKind::Convert { target } => {
            match checked {
                CheckValue::Covered => {
                    return Ok(rejection(
                        &usage,
                        declaration,
                        Kind::UnsupportedPosition,
                        "conversion is not supported inside this expression",
                        path,
                    ));
                }
                CheckValue::Current(_) | CheckValue::Discarded => {}
            }
            if covered || placement(usage.placement_edge()).is_err() {
                return Ok(rejection(
                    &usage,
                    declaration,
                    Kind::UnsupportedPosition,
                    "conversion requires an independently typed destination",
                    path,
                ));
            }
            let mut destination_matched = false;
            let result = (|| {
                let target = crate::conversion::resolve_target(
                    target.detail(),
                    &node.location().authored,
                    config,
                )
                .map_err(|e| typed(e.owned_by(node.node_id())))?;
                let selected = destination(declaration, &target, defs).map_err(|kind| {
                    error(
                        node,
                        kind,
                        "conversion destination must have exactly one matching type identity",
                    )
                })?;
                destination_matched = true;
                let operand = node
                    .children()
                    .next()
                    .ok_or_else(|| {
                        crate::validation::admission::assignment_seam_error(MissingEvidence::new(
                            "conversion operand",
                        ))
                    })?
                    .map_err(crate::validation::admission::assignment_seam_error)?;
                if operand.judgement().is_conversion_output() {
                    return Err(typed(error(
                        node,
                        Kind::UnsupportedPosition,
                        "a conversion cannot be the operand of another conversion",
                    )));
                }
                let source = operand.judgement();
                match checked {
                    CheckValue::Discarded => {
                        check_scalar_kind(selected, source).map_err(|kind| {
                            typed(error(
                                node,
                                kind,
                                "discarded conversion operand has an incompatible scalar kind",
                            ))
                        })?;
                    }
                    CheckValue::Current(_) => {
                        let source_type = check_scalar_kind(selected, source).map_err(|kind| {
                            typed(error(
                                node,
                                kind,
                                "conversion operand has no compatible scalar source judgement",
                            ))
                        })?;
                        if let TypeExpr::SchemaType { owned } = source_type {
                            let declaration = crate::conversion::source_declaration(
                                owned,
                                &node.location().authored,
                                config,
                            )
                            .map_err(|error| typed(error.owned_by(node.node_id())))?;
                            if let Some(bounds) = DeclarationBounds::from_declaration(selected)
                                && DeclarationBounds::from_declaration(&declaration)
                                    .is_some_and(|source| source.disjoint(&bounds))
                            {
                                return Err(typed(error(
                                    node,
                                    Kind::Disjoint,
                                    "conversion source and destination declaration bounds are disjoint",
                                )));
                            }
                        }
                        let matched = MatchedConversionSource {
                            stamp: SourceStamp::new(&usage, selected),
                            location: node.location().authored,
                        };
                        return crate::validation::admission::conversion_local::<K, B>(
                            matched, &usage, selected, path,
                        )
                        .map(|findings| claimed(&usage, declaration, findings, true));
                    }
                    CheckValue::Covered => {
                        return Err(typed(error(
                            node,
                            Kind::UnsupportedPosition,
                            "covered conversion",
                        )));
                    }
                }
                Ok(claimed(&usage, declaration, Default::default(), true))
            })();
            Ok(match result {
                Ok(outcome) => outcome,
                Err(failure) => {
                    let failure = if node.judgement().diagnostic_is_secret() {
                        failure.inside_secret()
                    } else {
                        failure
                    };
                    claimed(
                        &usage,
                        declaration,
                        AssignmentFindings {
                            errors: vec![(path.clone(), failure)],
                            ..Default::default()
                        },
                        destination_matched,
                    )
                }
            })
        }
        ExprKind::If | ExprKind::Coalesce => {
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
                            path,
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
        | ExprKind::Sink(_)
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

impl<B: BindingAddress> ExpressionReport<B> {
    /// B1 boundary errors are placeholders until B2 supplies boundary cursors.
    /// Completeness, including unavailable slot authority, never grants admission.
    pub fn conversion_completeness(
        &self,
        coverage: &crate::schema::value_validation::WalkCoverage,
    ) -> crate::ConversionChannel {
        let report = self.completeness_report(coverage);
        // A real boundary use owns the rejection ahead of its definition-only
        // record. Partition completed evidence; do not revisit syntax or values.
        let (definitions, uses): (Vec<_>, Vec<_>) = report
            .unreached(|node| matches!(node.kind(), ExprKind::Convert { .. }))
            .partition(|usage| usage.definition_only());
        uses.into_iter()
            .chain(definitions)
            .map(|usage| {
                error(
                    usage.node(),
                    if usage.definition_only() {
                        Kind::Unused
                    } else {
                        Kind::UnsupportedPosition
                    },
                    if usage.definition_only() {
                        "conversion has no typed destination use"
                    } else {
                        "conversion boundary is not supported until B2"
                    },
                )
            })
            .collect()
    }
}
