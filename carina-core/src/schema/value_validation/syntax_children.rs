//! Closed child decisions for pending and discarded authored syntax. A decline
//! is observable only after its conversion evidence has been consumed and its
//! findings published into this probe; iteration arms cannot manufacture it.
use super::*;
use crate::parser::retained::conversion::DeclineReason;

pub(super) struct SyntaxChild<'d, 's, B: BindingAddress> {
    pub(super) declaration: &'d CompiledAttributeType,
    pub(super) cursor: AuthoredUse<'s, B>,
    pub(super) path: FieldPath,
    pub(super) ancestry: FieldAncestry,
}

pub(super) struct ChildStep<'d, 's, B: BindingAddress> {
    decision: ChildDecision<'d, 's, B>,
}
enum ChildDecision<'d, 's, B: BindingAddress> {
    Descend(SyntaxChild<'d, 's, B>),
    Declined(DeclineReason),
}

/// The ancestor's single judgement fact proves absence on all its transported
/// children. Carrying ancestors require the actual selected child's evidence.
/// This also preserves main's lack of navigation for a declined no-as field.
enum ChildEvidence<'s, B: BindingAddress> {
    Free(crate::parser::retained::NoConversion),
    Judged(AuthoredUse<'s, B>),
}

/// Concrete publication authority, not a caller-supplied observer closure.
pub(super) struct DeclineSink<'a, 'c, P: ValueKind, B: BindingAddress, M: WalkMode> {
    callbacks: &'a WalkCallbacks<'c, P, B, M>,
    memo: &'a mut M::Declines,
    findings: &'a mut AssignmentFindings<M::Channel>,
}
impl<'a, 'c, P: ValueKind, B: BindingAddress, M: WalkMode> DeclineSink<'a, 'c, P, B, M> {
    pub(super) fn new(
        callbacks: &'a WalkCallbacks<'c, P, B, M>,
        memo: &'a mut M::Declines,
        findings: &'a mut AssignmentFindings<M::Channel>,
    ) -> Self {
        Self {
            callbacks,
            memo,
            findings,
        }
    }
    fn record(
        self,
        child: &AuthoredUse<'_, B>,
        reason: &DeclineReason,
        path: &FieldPath,
        ancestry: FieldAncestry,
    ) {
        self.findings.extend(M::decline_child(
            self.callbacks.policy,
            self.memo,
            child,
            reason,
            &path.canonical(),
            ancestry.in_secret,
        ));
    }
}

impl<'d, 's, B: BindingAddress> ChildStep<'d, 's, B> {
    pub(super) fn descend(
        declaration: &'d CompiledAttributeType,
        cursor: AuthoredUse<'s, B>,
        path: FieldPath,
        ancestry: FieldAncestry,
    ) -> Self {
        Self {
            decision: ChildDecision::Descend(SyntaxChild {
                declaration,
                cursor,
                path,
                ancestry,
            }),
        }
    }
    fn routed(
        declaration: &'d CompiledAttributeType,
        parent: &AuthoredUse<'s, B>,
        route: &ChildRoute<'_>,
        path: &FieldPath,
        ancestry: FieldAncestry,
    ) -> Result<Self, TypeError> {
        Ok(Self::descend(
            declaration,
            routed_child(parent, route)?,
            route.path(path),
            ancestry,
        ))
    }
    pub(super) fn list(
        declaration: &'d CompiledAttributeType,
        parent: &AuthoredUse<'s, B>,
        index: usize,
        path: &FieldPath,
        ancestry: FieldAncestry,
    ) -> Result<Self, TypeError> {
        Self::routed(
            declaration,
            parent,
            &ChildRoute::ListItem(index),
            path,
            ancestry,
        )
    }
    pub(super) fn map<P: ValueKind, M: WalkMode>(
        declaration: &'d CompiledAttributeType,
        parent: &AuthoredUse<'s, B>,
        key: &crate::parser::AttributeKey,
        path: &FieldPath,
        ancestry: FieldAncestry,
        sink: DeclineSink<'_, '_, P, B, M>,
    ) -> Result<Self, TypeError> {
        let map_route = ChildRoute::MapValue(std::borrow::Cow::Borrowed(key));
        match &declaration.kind {
            AttrTypeKind::Map { value, .. } => {
                Self::routed(value, parent, &map_route, path, ancestry)
            }
            AttrTypeKind::Struct { fields, .. } => {
                match struct_field_for_key(fields, key.transport_identity(), ancestry) {
                    None => Self::declined(
                        DeclineReason::UndeclaredField {
                            field: key.to_string(),
                        },
                        child_evidence(parent, &map_route)?,
                        sink,
                        &map_route.path(path),
                        ancestry,
                    ),
                    Some(field) => {
                        let route = ChildRoute::StructField {
                            key: std::borrow::Cow::Borrowed(key),
                            alias: ancestry.block_name(field),
                        };
                        match validate_struct_field_assignment(field, &key.to_string(), ancestry) {
                            Some(error) => Self::declined(
                                DeclineReason::FieldAssignment {
                                    field: key.to_string(),
                                    error,
                                },
                                child_evidence(parent, &route)?,
                                sink,
                                &route.path(path),
                                ancestry,
                            ),
                            None => Self::routed(
                                &field.field_type,
                                parent,
                                &route,
                                path,
                                ancestry.descend(field),
                            ),
                        }
                    }
                }
            }
            AttrTypeKind::UnloadedSchemaType(_)
            | AttrTypeKind::Reference { .. }
            | AttrTypeKind::String { .. }
            | AttrTypeKind::ResourceHandle { .. }
            | AttrTypeKind::StringLiteralSet { .. }
            | AttrTypeKind::Int { .. }
            | AttrTypeKind::Float { .. }
            | AttrTypeKind::Bool
            | AttrTypeKind::Duration
            | AttrTypeKind::Enum { .. }
            | AttrTypeKind::Union(_)
            | AttrTypeKind::Ref(_)
            | AttrTypeKind::List { .. } => {
                Self::routed(declaration, parent, &map_route, path, ancestry)
            }
        }
    }
    fn declined<P: ValueKind, M: WalkMode>(
        reason: DeclineReason,
        evidence: ChildEvidence<'s, B>,
        sink: DeclineSink<'_, '_, P, B, M>,
        path: &FieldPath,
        ancestry: FieldAncestry,
    ) -> Result<Self, TypeError> {
        // Publication happens before a Declined decision can escape. Even
        // dropping the returned step cannot silently discard its finding.
        match evidence {
            ChildEvidence::Free(_proof) => {}
            ChildEvidence::Judged(child) => sink.record(&child, &reason, path, ancestry),
        }
        Ok(Self {
            decision: ChildDecision::Declined(reason),
        })
    }
    pub(super) fn visit(
        self,
        descend: impl FnOnce(SyntaxChild<'d, 's, B>) -> Result<(), TypeError>,
    ) -> Result<(), TypeError> {
        match self.decision {
            ChildDecision::Descend(child) => descend(child),
            ChildDecision::Declined(reason) => match reason {
                DeclineReason::UndeclaredField { .. } | DeclineReason::FieldAssignment { .. } => {
                    Ok(())
                }
            },
        }
    }
}

fn routed_child<'s, B: BindingAddress>(
    parent: &AuthoredUse<'s, B>,
    route: &ChildRoute<'_>,
) -> Result<AuthoredUse<'s, B>, TypeError> {
    match route
        .step(&AuthoredCursor::authored(parent.clone()))
        .map_err(seam_error)?
        .kind
    {
        CursorKind::Authored(child) => Ok(child),
        CursorKind::CoveredBy(_) | CursorKind::Projected(_) => Err(seam_error(
            crate::parser::retained::MissingEvidence::new("authored syntax child"),
        )),
    }
}
fn child_evidence<'s, B: BindingAddress>(
    parent: &AuthoredUse<'s, B>,
    route: &ChildRoute<'_>,
) -> Result<ChildEvidence<'s, B>, TypeError> {
    match parent.node().judgement().no_conversion() {
        Some(proof) => Ok(ChildEvidence::Free(proof)),
        None => routed_child(parent, route).map(ChildEvidence::Judged),
    }
}
