//! Closed authored admission inputs. Stage 1 defines the seam; Stage 2 connects
//! retained construction and the existing schema walk to it.

use super::{
    AuthoredExpr, ExprKind, ExpressionReport, JudgedExpr, MissingEvidence, Projection,
    SinkExpression,
};
use crate::resource::knowledge::{Apply, Knowledge, PlanResolved};
use crate::resource::{
    BindingAddress, ParserPlaceholder, PlaceholderKind, RelativeBindingName, Value,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Syntax identity, independent of source ranges and projected values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u64);

impl NodeId {
    pub(super) fn fresh() -> Self {
        Self(fresh_identity())
    }
}

/// One static use edge; sharing a syntax node does not share its authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UseId(u64);

impl UseId {
    pub(super) fn fresh() -> Self {
        Self(fresh_identity())
    }
}

/// One resource/data-source expansion, independent of its source location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExpansionInstanceId(u64);

impl ExpansionInstanceId {
    pub(super) fn fresh() -> Self {
        Self(fresh_identity())
    }
}

/// Identifies the projection and judgement generation owned by a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectionGeneration(u64);

impl ProjectionGeneration {
    pub(super) fn fresh() -> Self {
        Self(fresh_identity())
    }
}

fn fresh_identity() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("retained identity exhausted")
}

/// A direct syntax/judgement pair within the slot's retained snapshot.
#[derive(Debug, Clone)]
pub struct JudgedNodeRef<'s, B: BindingAddress = RelativeBindingName> {
    node: &'s AuthoredExpr<ParserPlaceholder, B>,
    judgement: &'s JudgedExpr,
    snapshot: &'s ExpressionReport<B>,
    generation: ProjectionGeneration,
}

impl<'s, B: BindingAddress> JudgedNodeRef<'s, B> {
    pub fn node_id(&self) -> NodeId {
        self.node.id
    }

    pub fn judgement(&self) -> &'s JudgedExpr {
        self.judgement
    }

    pub fn generation(&self) -> ProjectionGeneration {
        self.generation
    }

    pub(in crate::parser::retained) fn kind(&self) -> &ExprKind<B> {
        &self.node.kind
    }
}

/// A judged node at a particular use in a particular expansion.
#[derive(Debug, Clone)]
pub struct AuthoredUse<'s, B: BindingAddress = RelativeBindingName> {
    node: JudgedNodeRef<'s, B>,
    occurrence: UseId,
    instance: ExpansionInstanceId,
}

impl<'s, B: BindingAddress> AuthoredUse<'s, B> {
    pub fn node(&self) -> &JudgedNodeRef<'s, B> {
        &self.node
    }

    pub fn occurrence(&self) -> UseId {
        self.occurrence
    }

    pub fn instance(&self) -> ExpansionInstanceId {
        self.instance
    }
}

/// Projection authority is created explicitly at an operational boundary.
/// Its private representation has no authored-association failure case.
///
/// Operational phases require a proof; callers cannot mint a fallback cursor.
///
/// ```compile_fail,E0061
/// use carina_core::parser::retained::ProjectedPhase;
/// let _ = ProjectedPhase::plan();
/// ```
///
/// ```compile_fail,E0061
/// use carina_core::parser::retained::ProjectedPhase;
/// let _ = ProjectedPhase::apply();
/// ```
///
/// ```compile_fail
/// use carina_core::parser::retained::ProjectedPhase;
/// let _ = ProjectedPhase::synthetic();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectedPhase(ProjectedOrigin);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ProjectedOrigin {
    Plan,
    Apply,
    Synthetic(SyntheticSource),
}

/// The non-authored attribute producers on main. Variant payloads are sealed
/// by their producer modules; there is no raw-value or lookup-failure source.
/// Schema defaults used only for display are not an attribute producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SyntheticSource {
    DefaultTagsMerge(crate::executor::normalized::DefaultTagsMergeSource),
    ProviderNormalization(crate::executor::normalized::ProviderNormalizationSource),
    SavedPlanReplay(crate::saved_replay::SavedPlanReplaySource),
}

impl ProjectedPhase {
    pub fn plan<P: PlaceholderKind, B: BindingAddress>(_proof: &PlanResolved<'_, P, B>) -> Self {
        Self(ProjectedOrigin::Plan)
    }

    // KnownValue alone is stage-independent. Retain the Apply marker that the
    // apply boundary holds before consuming it with KnownValue::from_apply.
    pub fn apply<P: PlaceholderKind, B: BindingAddress>(
        _proof: &Knowledge<'_, P, B, Apply>,
    ) -> Self {
        Self(ProjectedOrigin::Apply)
    }

    pub(crate) fn synthetic(source: SyntheticSource) -> Self {
        Self(ProjectedOrigin::Synthetic(source))
    }
}

/// Mandatory source authority for an assignment check. There is no missing or
/// default case, and projection authority cannot be made from a failed lookup.
#[derive(Debug, Clone)]
pub enum AuthoredCursor<'s, B: BindingAddress = RelativeBindingName> {
    Authored(AuthoredUse<'s, B>),
    CoveredBy(AuthoredUse<'s, B>),
    Projected(ProjectedPhase),
}

/// The complete attribute input, sealed by retained construction. Replacing or
/// expanding a projection must transport this bundle and renew its generation.
#[derive(Debug, Clone)]
pub struct JudgedSlot<B: BindingAddress = RelativeBindingName> {
    root: Arc<AuthoredExpr<ParserPlaceholder, B>>,
    projection: Projection<ParserPlaceholder, B>,
    occurrence: UseId,
    instance: ExpansionInstanceId,
    generation: ProjectionGeneration,
    snapshot: Arc<ExpressionReport<B>>,
    judgement: JudgedExpr,
}

impl JudgedSlot {
    /// Stage 2 construction consumes the complete sink retained at both
    /// attribute branches of parser/blocks/resource.rs. The inventory and slot
    /// must share its root; no source-location join can construct this pair.
    pub(in crate::parser) fn from_sink(
        _sink: SinkExpression,
        _snapshot: Arc<ExpressionReport>,
        _instance: ExpansionInstanceId,
    ) -> Result<Self, MissingEvidence> {
        todo!("Stage 2: seal the sink with its directly paired judgement")
    }
}

impl<B: BindingAddress> JudgedSlot<B> {
    pub fn projection(&self) -> Option<&Value<ParserPlaceholder, B>> {
        match &self.projection {
            Projection::Value(value) => Some(value),
            Projection::Pending => None,
        }
    }

    pub fn cursor(&self) -> AuthoredCursor<'_, B> {
        AuthoredCursor::Authored(AuthoredUse {
            node: JudgedNodeRef {
                node: &self.root,
                judgement: &self.judgement,
                snapshot: &self.snapshot,
                generation: self.generation,
            },
            occurrence: self.occurrence,
            instance: self.instance,
        })
    }

    pub fn occurrence(&self) -> UseId {
        self.occurrence
    }

    pub fn instance(&self) -> ExpansionInstanceId {
        self.instance
    }

    pub fn generation(&self) -> ProjectionGeneration {
        self.generation
    }
}

/// Per-use completeness, including definition uses hidden by a projection.
#[derive(Debug)]
pub struct CompletenessReport<'s, B: BindingAddress = RelativeBindingName> {
    unreached: Vec<AuthoredUse<'s, B>>,
}

impl<'s, B: BindingAddress> CompletenessReport<'s, B> {
    /// The predicate inspects retained syntax; it does not classify a Value.
    pub fn unreached(
        &self,
        kind: impl Fn(&JudgedNodeRef<'s, B>) -> bool,
    ) -> impl Iterator<Item = &AuthoredUse<'s, B>> {
        self.unreached.iter().filter(move |usage| kind(&usage.node))
    }
}

impl<B: BindingAddress> ExpressionReport<B> {
    pub(crate) fn completeness_report(
        &self,
        _coverage: &crate::schema::value_validation::WalkCoverage,
    ) -> CompletenessReport<'_, B> {
        todo!("Stage 2: compare static use edges with authored visits")
    }
}
