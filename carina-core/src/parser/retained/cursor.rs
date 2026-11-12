//! Closed authored inputs, shared syntax/judgement snapshots and per-use coverage.

use super::{
    AuthoredExpr, ConditionSnapshot, ExprKind, ExpressionInstance, ExpressionLocation,
    ExpressionReport, JudgedExpr, MissingEvidence, Projection,
};
use crate::resource::knowledge::{Apply, Knowledge, PlanResolved};
use crate::resource::{
    BindingAddress, ParserPlaceholder, PlaceholderKind, RelativeBindingName, Value,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[cfg(test)]
mod tests;

/// Syntax identity, independent of source ranges and projected values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u64);

impl NodeId {
    pub(super) fn fresh() -> Self {
        Self(fresh_identity())
    }
}

/// Parser-assigned resource declaration address within its source file. It
/// selects a storage owner across equivalent source views; only that owner's
/// NodeId, use, instance and generation can authorize authored admission.
/// No source ranges, attribute values or projected resource IDs participate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SlotDeclarationId {
    file: crate::parser::SourceFileId,
    ordinal: usize,
}
impl SlotDeclarationId {
    pub(super) fn new(file: &crate::parser::SourceFileId, ordinal: usize) -> Self {
        Self {
            file: file.clone(),
            ordinal,
        }
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
    pub(crate) fn latest(self, other: Self) -> Self {
        if self.0 >= other.0 { self } else { other }
    }
}

fn fresh_identity() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("retained identity exhausted")
}

/// The explicit authored edge used to reach a node. Grammar forwarding is
/// transparent; operations and definition uses retain distinct roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeRole {
    SlotRoot,
    Parens,
    IfBranch,
    CoalesceOperand,
    AliasDefinition,
    ListItem,
    MapValue,
    MapKey,
    StructField,
    SecretArgument,
    InterpolationPart,
    CallArgument,
    ArithmeticOperand,
    AccessBase,
    AccessIndex,
    Condition,
    SequenceResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthoredForm {
    Forward,
    Parens,
    Definition,
    Binding,
    Reference,
    Literal,
    List,
    Map,
    Secret,
    If,
    Coalesce,
    Computed,
}

/// Syntax navigation shared by current and discarded assignment walks.
pub(crate) enum AuthoredNavigation<'s, B: BindingAddress> {
    Forward {
        child: AuthoredUse<'s, B>,
        authored: bool,
    },
    Secret(AuthoredUse<'s, B>),
    Alternatives {
        first: usize,
        edge: EdgeRole,
    },
    List,
    Map,
    Parts,
    Leaf,
    Covered,
}

fn is_boundary_definition<B: BindingAddress>(
    mut node: &AuthoredExpr<ParserPlaceholder, B>,
) -> bool {
    loop {
        match &node.kind {
            ExprKind::Resource { .. } | ExprKind::Function { .. } => return true,
            ExprKind::Alias | ExprKind::Parens | ExprKind::Definition | ExprKind::Sequence
                if node.children.len() == 1 =>
            {
                node = &node.children[0].authored;
            }
            _ => return false,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct UseInterner(
    std::sync::Mutex<std::collections::HashMap<(UseId, NodeId, ExpansionInstanceId), UseId>>,
);
impl UseInterner {
    fn edge(&self, parent: UseId, node: NodeId, instance: ExpansionInstanceId) -> UseId {
        *self
            .0
            .lock()
            .expect("authored use interner")
            .entry((parent, node, instance))
            .or_insert_with(UseId::fresh)
    }
}

#[derive(Debug, Clone)]
pub(super) struct UseRoot<B: BindingAddress> {
    pub(super) node: Arc<AuthoredExpr<ParserPlaceholder, B>>,
    pub(super) occurrence: UseId,
    pub(super) instance: ExpansionInstanceId,
    pub(super) generation: ProjectionGeneration,
    pub(super) definition: bool,
    pub(super) environment: ExpressionInstance,
    pub(super) conditions: ConditionSnapshot,
}
impl<B: BindingAddress> UseRoot<B> {
    pub(super) fn map_bindings<C: BindingAddress>(
        &self,
        map: &mut impl FnMut(B) -> C,
        memo: &mut std::collections::HashMap<usize, Arc<AuthoredExpr<ParserPlaceholder, C>>>,
    ) -> UseRoot<C> {
        let node = super::ExprValue {
            authored: self.node.clone(),
            payload: Projection::Pending,
        }
        .map_bindings(map, memo)
        .authored;
        UseRoot {
            node,
            occurrence: self.occurrence,
            instance: self.instance,
            generation: ProjectionGeneration::fresh(),
            definition: self.definition,
            environment: self.environment.clone(),
            conditions: self.conditions.clone(),
        }
    }
}

/// A direct syntax/judgement pair within the slot's retained snapshot.
#[derive(Clone)]
pub struct JudgedNodeRef<'s, B: BindingAddress = RelativeBindingName> {
    node: &'s AuthoredExpr<ParserPlaceholder, B>,
    judgement: &'s JudgedExpr,
    snapshot: &'s ExpressionReport<B>,
    generation: ProjectionGeneration,
    environment: ExpressionInstance,
    conditions: &'s ConditionSnapshot,
}

impl<B: BindingAddress> std::fmt::Debug for JudgedNodeRef<'_, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgedNodeRef")
            .field("node", &self.node.id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// Grammar aliases have no distinct authored position. Live cursors and
/// unavailable-slot identities use exactly the same normalization.
fn authored_chain<P: crate::resource::ValueKind, B: BindingAddress>(
    node: &AuthoredExpr<P, B>,
) -> impl Iterator<Item = &AuthoredExpr<P, B>> {
    std::iter::successors(Some(node), |node| {
        (matches!(node.kind, ExprKind::Alias) && node.children.len() == 1)
            .then(|| node.children[0].authored.as_ref())
    })
}

impl<'s, B: BindingAddress> JudgedNodeRef<'s, B> {
    fn canonical(mut self) -> Result<Self, MissingEvidence> {
        // Alias nodes are pass-through grammar levels, not authored syntax.
        // Retain them in the expression report for diagnostic compatibility.
        for node in authored_chain(self.node).skip(1) {
            self.judgement = self
                .snapshot
                .node(&self.environment, node.id)
                .ok_or_else(|| MissingEvidence::new(format!("authored node {:?}", node.id)))?;
            self.node = node;
        }
        Ok(self)
    }

    pub fn node_id(&self) -> NodeId {
        self.node.id
    }

    pub fn judgement(&self) -> &'s JudgedExpr {
        self.judgement
    }

    pub fn generation(&self) -> ProjectionGeneration {
        self.generation
    }

    pub(crate) fn environment(&self) -> &ExpressionInstance {
        &self.environment
    }

    /// Direct authored children in syntax order, paired with the same snapshot
    /// and projection generation. A missing child judgement is a typed
    /// correspondence failure, including on diagnostic-only operand reads.
    pub fn children(
        &self,
    ) -> impl Iterator<Item = Result<JudgedNodeRef<'s, B>, MissingEvidence>> + '_ {
        self.node.children.iter().map(move |child| {
            JudgedNodeRef {
                node: &child.authored,
                judgement: self
                    .snapshot
                    .node(&self.environment, child.node_id())
                    .ok_or_else(|| {
                        MissingEvidence::new(format!("child judgement {:?}", child.node_id()))
                    })?,
                snapshot: self.snapshot,
                generation: self.generation,
                conditions: self.conditions,
                environment: self.environment.clone(),
            }
            .canonical()
        })
    }

    /// Read an operand's parser-carried identity within this same snapshot.
    /// This is used by ordinary computed diagnostics after value folding.
    pub(crate) fn retained_node(
        &self,
        id: NodeId,
    ) -> Result<JudgedNodeRef<'s, B>, MissingEvidence> {
        let node = self
            .snapshot
            .syntax_node(&self.environment, id)
            .ok_or_else(|| MissingEvidence::new(format!("retained computed node {id:?}")))?;
        let judgement = self
            .snapshot
            .node(&self.environment, id)
            .ok_or_else(|| MissingEvidence::new(format!("computed judgement {id:?}")))?;
        Ok(Self {
            node,
            judgement,
            snapshot: self.snapshot,
            generation: self.generation,
            conditions: self.conditions,
            environment: self.environment.clone(),
        })
    }

    /// Diagnostic range only; NodeId and use/instance identity own admission.
    pub fn location(&self) -> ExpressionLocation {
        self.node.location.clone()
    }

    pub(in crate::parser::retained) fn kind(&self) -> &ExprKind<B> {
        &self.node.kind
    }
}

/// A folded selection has syntax of its own, but ordinary admission belongs
/// to the selected child. Policy still receives the parent and its alternatives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionVisit {
    Value,
    Selection,
    // A lowered child covered by the same owner is not another authored node.
    ProjectionOnly,
}

/// A judged node at a particular use in a particular expansion.
#[derive(Debug, Clone)]
pub struct AuthoredUse<'s, B: BindingAddress = RelativeBindingName> {
    node: JudgedNodeRef<'s, B>,
    occurrence: UseId,
    instance: ExpansionInstanceId,
    edge: EdgeRole,
    visit: ProjectionVisit,
}

type AuthoredUseIdentity = (NodeId, UseId, ExpansionInstanceId, ProjectionGeneration);

impl<'s, B: BindingAddress> AuthoredUse<'s, B> {
    fn identity(&self) -> AuthoredUseIdentity {
        (
            self.node.node.id,
            self.occurrence,
            self.instance,
            self.node.generation,
        )
    }

    pub fn edge(&self) -> EdgeRole {
        self.edge
    }

    pub(crate) fn form(&self) -> AuthoredForm {
        match self.node.kind() {
            ExprKind::Alias | ExprKind::Sink(_) | ExprKind::Sequence => AuthoredForm::Forward,
            ExprKind::Parens => AuthoredForm::Parens,
            ExprKind::Definition => AuthoredForm::Definition,
            ExprKind::Binding(_) => AuthoredForm::Binding,
            ExprKind::Reference { .. } => AuthoredForm::Reference,
            ExprKind::List => AuthoredForm::List,
            ExprKind::Map(_) => AuthoredForm::Map,
            ExprKind::Secret => AuthoredForm::Secret,
            ExprKind::If => AuthoredForm::If,
            ExprKind::Coalesce => AuthoredForm::Coalesce,
            ExprKind::Scalar(_)
            | ExprKind::Parameter(_)
            | ExprKind::LoopSlot(_)
            | ExprKind::UntypedParameter
            | ExprKind::Element => AuthoredForm::Literal,
            ExprKind::Interpolation { .. }
            | ExprKind::EmptyInterpolation
            | ExprKind::Call(_)
            | ExprKind::Function { .. }
            | ExprKind::Access(_)
            | ExprKind::Integer { .. }
            | ExprKind::Iterable
            | ExprKind::Resource { .. }
            | ExprKind::For
            | ExprKind::Wait
            | ExprKind::Compose
            | ExprKind::DynamicAccess
            | ExprKind::ModuleCall
            | ExprKind::Boolean
            | ExprKind::Declaration
            | ExprKind::ProviderDeclaration
            | ExprKind::Unsupported(_)
            | ExprKind::NestedBlock { .. } => AuthoredForm::Computed,
        }
    }

    pub(crate) fn child_count(&self) -> usize {
        self.node.node.children.len()
    }

    pub(crate) fn map_keys(&self) -> &[String] {
        match &self.node.node.kind {
            ExprKind::Map(keys) => keys,
            _ => &[],
        }
    }

    pub(crate) fn map_child(&self, key: &str, edge: EdgeRole) -> Result<Self, MissingEvidence> {
        let ExprKind::Map(keys) = &self.node.node.kind else {
            return Err(MissingEvidence::new("authored map container"));
        };
        let index = keys
            .iter()
            .rposition(|candidate| candidate == key)
            .ok_or_else(|| MissingEvidence::new(format!("authored map child {key}")))?;
        let entry = self.child(index, edge)?;
        entry.child(0, edge)
    }

    pub(crate) fn definition(&self) -> Result<Option<Self>, MissingEvidence> {
        let ExprKind::Binding(binding) = &self.node.node.kind else {
            return Ok(None);
        };
        let Some(expression) = self.node.snapshot.definitions.get(binding) else {
            return Ok(None);
        };
        if is_boundary_definition(&expression.authored) {
            return Ok(None);
        }
        let judgement = self
            .node
            .snapshot
            .node(&expression.authored.location.instance, expression.node_id())
            .ok_or_else(|| MissingEvidence::new(format!("definition {binding}")))?;
        Ok(Some(
            Self {
                node: JudgedNodeRef {
                    node: &expression.authored,
                    judgement,
                    snapshot: self.node.snapshot,
                    generation: self.node.generation,
                    conditions: self.node.conditions,
                    environment: expression.authored.location.instance.clone(),
                },
                occurrence: self.node.snapshot.use_interner.edge(
                    self.occurrence,
                    self.node.node.id,
                    self.instance,
                ),
                instance: self.instance,
                edge: EdgeRole::AliasDefinition,
                visit: ProjectionVisit::Value,
            }
            .canonical()?,
        ))
    }

    pub(crate) fn parens_operand(&self) -> Result<Self, MissingEvidence> {
        self.child(0, EdgeRole::Parens)
    }

    pub(crate) fn navigation(&self) -> Result<AuthoredNavigation<'s, B>, MissingEvidence> {
        Ok(match self.form() {
            AuthoredForm::Forward if self.child_count() != 0 => AuthoredNavigation::Forward {
                child: self.child(self.child_count() - 1, self.edge())?,
                authored: !matches!(self.node.kind(), ExprKind::Alias),
            },
            AuthoredForm::Parens => AuthoredNavigation::Forward {
                child: self.parens_operand()?,
                authored: true,
            },
            AuthoredForm::Definition => {
                if self
                    .node
                    .node
                    .children
                    .first()
                    .is_some_and(|child| is_boundary_definition(&child.authored))
                {
                    AuthoredNavigation::Leaf
                } else {
                    AuthoredNavigation::Forward {
                        child: self.child(0, EdgeRole::AliasDefinition)?,
                        authored: true,
                    }
                }
            }
            AuthoredForm::Binding => match self.definition()? {
                Some(definition) => AuthoredNavigation::Forward {
                    child: definition,
                    authored: true,
                },
                None => AuthoredNavigation::Leaf,
            },
            AuthoredForm::Secret => {
                AuthoredNavigation::Secret(self.child(0, EdgeRole::SecretArgument)?)
            }
            AuthoredForm::If => AuthoredNavigation::Alternatives {
                first: 1,
                edge: EdgeRole::IfBranch,
            },
            AuthoredForm::Coalesce => AuthoredNavigation::Alternatives {
                first: 0,
                edge: EdgeRole::CoalesceOperand,
            },
            AuthoredForm::List => AuthoredNavigation::List,
            AuthoredForm::Map => AuthoredNavigation::Map,
            AuthoredForm::Computed => match self.node.kind() {
                ExprKind::Interpolation { .. }
                | ExprKind::Call(_)
                | ExprKind::Integer { .. }
                | ExprKind::Access(_)
                | ExprKind::DynamicAccess => AuthoredNavigation::Parts,
                _ => AuthoredNavigation::Covered,
            },
            AuthoredForm::Forward | AuthoredForm::Literal | AuthoredForm::Reference => {
                AuthoredNavigation::Leaf
            }
        })
    }

    pub(crate) fn part(&self, index: usize) -> Result<Self, MissingEvidence> {
        let role = match self.node.kind() {
            ExprKind::Interpolation { .. } => EdgeRole::InterpolationPart,
            ExprKind::Call(_) => EdgeRole::CallArgument,
            ExprKind::Integer { .. } => EdgeRole::ArithmeticOperand,
            ExprKind::Access(_) | ExprKind::DynamicAccess => {
                if index == 0 {
                    EdgeRole::AccessBase
                } else {
                    EdgeRole::AccessIndex
                }
            }
            _ => return Err(MissingEvidence::new("authored expression part")),
        };
        self.child(index, role)
    }

    pub(crate) fn canonical(mut self) -> Result<Self, MissingEvidence> {
        self.node = self.node.canonical()?;
        Ok(self)
    }

    pub(crate) fn alternative(&self, index: usize) -> Result<Self, MissingEvidence> {
        let AuthoredNavigation::Alternatives { first, edge } = self.navigation()? else {
            return Err(MissingEvidence::new("authored alternative"));
        };
        self.child(first + index, edge)
    }

    pub(crate) fn known_condition(&self) -> Result<bool, MissingEvidence> {
        let condition = self
            .node
            .node
            .children
            .first()
            .ok_or_else(|| MissingEvidence::new("if condition"))?;
        if let Some(selected) = self.node.conditions.get(condition.node_id()) {
            return Ok(selected);
        }
        let value = self
            .node
            .snapshot
            .projections
            .get(&(self.node.environment.clone(), condition.node_id()))
            .or_else(|| condition.projection())
            .ok_or_else(|| MissingEvidence::new("if condition projection"))?;
        let known = value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map_err(|_| MissingEvidence::new("if condition knowledge"))?
            .into_classification()
            .into_known()
            .map_err(|_| MissingEvidence::new("if condition is pending"))?;
        known.visit(
            |view| match view {
                crate::resource::knowledge::KnownView::Bool(value) => Ok(value),
                _ => Err(MissingEvidence::new("if condition Bool")),
            },
            |_| Err(MissingEvidence::new("protected if condition")),
        )
    }

    pub(crate) fn child(&self, index: usize, edge: EdgeRole) -> Result<Self, MissingEvidence> {
        let child = self
            .node
            .node
            .children
            .get(index)
            .ok_or_else(|| MissingEvidence::new(format!("authored child {index}")))?;
        let judgement = self
            .node
            .snapshot
            .node(&self.node.environment, child.node_id())
            .ok_or_else(|| MissingEvidence::new(format!("authored node {:?}", child.node_id())))?;
        Self {
            node: JudgedNodeRef {
                node: &child.authored,
                judgement,
                snapshot: self.node.snapshot,
                generation: self.node.generation,
                conditions: self.node.conditions,
                environment: self.node.environment.clone(),
            },
            occurrence: if self.form() == AuthoredForm::Definition {
                self.node.snapshot.use_interner.edge(
                    self.occurrence,
                    self.node.node.id,
                    self.instance,
                )
            } else {
                self.occurrence
            },
            instance: self.instance,
            edge,
            visit: ProjectionVisit::Value,
        }
        .canonical()
    }

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
    ClassifiedInput,
    Synthetic(SyntheticSource),
}

/// The non-authored attribute producers on main. Variant payloads are sealed
/// by their producer modules; there is no raw-value or lookup-failure source.
/// Schema defaults used only for display are not an attribute producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SyntheticSource {
    ResourcePrefix(crate::identifier::ResourcePrefixSource),
}

impl ProjectedPhase {
    /// Explicit schema-only inputs supplied without an authored configuration
    /// (e.g. programmatic declarations). Resource analysis never uses this path.
    pub(crate) fn classified<P: PlaceholderKind, B: BindingAddress>(
        _: &crate::resource::knowledge::Classification<'_, P, B>,
    ) -> Self {
        Self(ProjectedOrigin::ClassifiedInput)
    }

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

#[derive(Debug, Clone)]
pub(crate) enum CursorKind<'s, B: BindingAddress = RelativeBindingName> {
    Authored(AuthoredUse<'s, B>),
    CoveredBy(AuthoredUse<'s, B>),
    Projected(ProjectedPhase),
}

/// Downstream code cannot manufacture a cursor phase.
/// ```compile_fail,E0599
/// use carina_core::parser::retained::{AuthoredCursor, AuthoredUse};
/// fn cover(usage: AuthoredUse<'_>) { let _ = AuthoredCursor::CoveredBy(usage); }
/// ```
/// ```compile_fail,E0599
/// use carina_core::parser::retained::{AuthoredCursor, AuthoredUse};
/// fn author(usage: AuthoredUse<'_>) { let _ = AuthoredCursor::Authored(usage); }
/// ```
/// ```compile_fail,E0599
/// use carina_core::parser::retained::{AuthoredCursor, ProjectedPhase};
/// fn project(phase: ProjectedPhase) { let _: AuthoredCursor<'_> = AuthoredCursor::Projected(phase); }
/// ```
/// Source authority carried through assignment checks. Construction is
/// confined to core; report-based paths cannot manufacture authored authority.
#[derive(Debug, Clone)]
pub struct AuthoredCursor<'s, B: BindingAddress = RelativeBindingName> {
    pub(crate) kind: CursorKind<'s, B>,
}

/// Borrowed reference syntax paired with the identity of its authored use.
pub(crate) struct SourceReference<'s, B: BindingAddress> {
    pub(crate) binding: &'s B,
    attribute: &'s str,
    segments: &'s [crate::resource::PathSegment],
    pub(crate) identity: AuthoredUseIdentity,
}

impl<B: BindingAddress> SourceReference<'_, B> {
    pub(crate) fn into_path(self) -> crate::resource::ReferencePath<B> {
        crate::resource::ReferencePath::Access(crate::resource::AccessPath::from_binding(
            self.binding.clone(),
            self.attribute.to_owned(),
            self.segments.to_vec(),
        ))
    }
}

impl<'s, B: BindingAddress> AuthoredCursor<'s, B> {
    pub(crate) fn authored(usage: AuthoredUse<'s, B>) -> Self {
        Self {
            kind: CursorKind::Authored(usage),
        }
    }
    pub(crate) fn projected(phase: ProjectedPhase) -> Self {
        Self {
            kind: CursorKind::Projected(phase),
        }
    }
    pub(crate) fn covered_by(usage: AuthoredUse<'s, B>) -> Self {
        Self {
            kind: CursorKind::CoveredBy(usage),
        }
    }

    pub(crate) fn selection(&self) -> Self {
        let mut cursor = self.clone();
        match &mut cursor.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
                usage.visit = ProjectionVisit::Selection
            }
            CursorKind::Projected(_) => {}
        }
        cursor
    }
    pub(crate) fn is_selection(&self) -> bool {
        match &self.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
                usage.visit == ProjectionVisit::Selection
            }
            CursorKind::Projected(_) => false,
        }
    }

    pub(crate) fn is_projection_only(&self) -> bool {
        match &self.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
                usage.visit == ProjectionVisit::ProjectionOnly
            }
            CursorKind::Projected(_) => true,
        }
    }

    /// The source reference survives directory substitution independently of
    /// the current value. Forwarding and selections have already advanced to
    /// their authored child before ordinary assignment asks for this origin.
    pub(crate) fn source_reference(&self) -> Option<SourceReference<'_, B>> {
        let CursorKind::Authored(usage) = &self.kind else {
            return None;
        };
        match usage.node.kind() {
            ExprKind::Reference {
                binding,
                attribute,
                segments,
            } => Some(SourceReference {
                binding,
                attribute,
                segments,
                identity: usage.identity(),
            }),
            _ => None,
        }
    }

    pub(crate) fn covered_projection(&self) -> Self {
        let mut cursor = self.covered();
        if let CursorKind::CoveredBy(usage) = &mut cursor.kind {
            usage.visit = ProjectionVisit::ProjectionOnly;
        }
        cursor
    }
    pub(crate) fn identity(&self) -> Option<AuthoredUseIdentity> {
        match &self.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => Some(usage.identity()),
            CursorKind::Projected(_) => None,
        }
    }
    pub(crate) fn covered(&self) -> Self {
        match &self.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
                Self::covered_by(usage.clone())
            }
            CursorKind::Projected(phase) => Self::projected(*phase),
        }
    }
    pub(crate) fn child(&self, index: usize, edge: EdgeRole) -> Result<Self, MissingEvidence> {
        match &self.kind {
            CursorKind::Authored(usage) => match usage.form() {
                // A substituted parameter/loop leaf retains its own authored
                // source; its projected container has no authored child edges.
                AuthoredForm::Reference | AuthoredForm::Binding | AuthoredForm::Literal => {
                    Ok(self.covered_projection())
                }
                AuthoredForm::List
                | AuthoredForm::If
                | AuthoredForm::Coalesce
                | AuthoredForm::Secret => usage.child(index, edge).map(Self::authored),
                _ => Err(MissingEvidence::new(
                    "required authored child correspondence",
                )),
            },
            CursorKind::CoveredBy(_) => Ok(self.covered_projection()),
            CursorKind::Projected(_) => Ok(self.clone()),
        }
    }

    pub(crate) fn alternative(&self, index: usize) -> Result<Self, MissingEvidence> {
        match &self.kind {
            CursorKind::Authored(usage) => match usage.form() {
                AuthoredForm::Reference | AuthoredForm::Binding | AuthoredForm::Literal => {
                    Ok(self.covered_projection())
                }
                _ => usage.alternative(index).map(Self::authored),
            },
            CursorKind::CoveredBy(_) => Ok(self.covered_projection()),
            CursorKind::Projected(_) => Ok(self.clone()),
        }
    }
    pub(crate) fn field_child(
        &self,
        key: &str,
        alias: Option<&str>,
    ) -> Result<Self, MissingEvidence> {
        let key = match &self.kind {
            CursorKind::Authored(usage)
                if usage.form() == AuthoredForm::Map
                    && !usage.map_keys().iter().any(|candidate| candidate == key) =>
            {
                alias.unwrap_or(key)
            }
            CursorKind::Authored(_) | CursorKind::CoveredBy(_) | CursorKind::Projected(_) => key,
        };
        self.map_child(key, EdgeRole::StructField)
    }

    pub(crate) fn map_child(&self, key: &str, edge: EdgeRole) -> Result<Self, MissingEvidence> {
        match &self.kind {
            CursorKind::Authored(usage) => match usage.form() {
                AuthoredForm::Map => usage.map_child(key, edge).map(Self::authored),
                AuthoredForm::Reference | AuthoredForm::Binding | AuthoredForm::Literal => {
                    Ok(self.covered_projection())
                }
                _ => Err(MissingEvidence::new("required authored map correspondence")),
            },
            CursorKind::CoveredBy(_) => Ok(self.covered_projection()),
            CursorKind::Projected(_) => Ok(self.clone()),
        }
    }
}

/// The complete attribute input, sealed by retained construction. Replacing or
/// expanding a projection must transport this bundle and renew its generation.
#[derive(Clone)]
pub struct JudgedSlot<B: BindingAddress = RelativeBindingName> {
    root: Arc<AuthoredExpr<ParserPlaceholder, B>>,
    projection: Projection<ParserPlaceholder, B>,
    occurrence: UseId,
    instance: ExpansionInstanceId,
    generation: ProjectionGeneration,
    snapshot: Arc<ExpressionReport<B>>,
    snapshot_error: Option<MissingEvidence>,
    schema_kind: crate::schema::SchemaKind,
    conditions: ConditionSnapshot,
}

impl<B: BindingAddress> std::fmt::Debug for JudgedSlot<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgedSlot")
            .field("node", &self.root.id)
            .field("instance", &self.instance)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl JudgedSlot {
    fn from_expression(
        expression: super::ExprValue,
        schema_kind: crate::schema::SchemaKind,
        instance: ExpansionInstanceId,
    ) -> Self {
        Self {
            root: expression.authored,
            projection: expression.payload,
            occurrence: UseId::fresh(),
            instance,
            generation: ProjectionGeneration::fresh(),
            snapshot: Arc::new(ExpressionReport::empty()),
            snapshot_error: None,
            schema_kind,
            conditions: ConditionSnapshot::default(),
        }
    }
}

impl<B: BindingAddress> JudgedSlot<B> {
    pub fn schema_kind(&self) -> crate::schema::SchemaKind {
        self.schema_kind
    }

    pub(crate) fn attach_snapshot(
        &mut self,
        snapshot: Arc<ExpressionReport<B>>,
    ) -> Result<(), MissingEvidence> {
        self.snapshot = Arc::new(ExpressionReport::empty());
        let _ = snapshot
            .node(&snapshot.instance, self.root.id)
            .ok_or_else(|| MissingEvidence::new(format!("slot node {:?}", self.root.id)))?;
        self.root = snapshot
            .syntax_node(&snapshot.instance, self.root.id)
            .ok_or_else(|| MissingEvidence::new(format!("slot syntax {:?}", self.root.id)))?
            .clone();
        self.snapshot = snapshot;
        self.snapshot_error = None;
        Ok(())
    }

    pub fn projection(&self) -> Option<&Value<ParserPlaceholder, B>> {
        match &self.projection {
            Projection::Value(value) => Some(value),
            Projection::Pending => None,
        }
    }

    pub fn cursor(&self) -> Result<AuthoredUse<'_, B>, MissingEvidence> {
        if let Some(error) = &self.snapshot_error {
            return Err(error.clone());
        }
        let judgement = self
            .snapshot
            .node(&self.snapshot.instance, self.root.id)
            .ok_or_else(|| {
                MissingEvidence::new(format!(
                    "slot {:?} requires a current judgement",
                    self.root.id
                ))
            })?;
        AuthoredUse {
            node: JudgedNodeRef {
                node: &self.root,
                judgement,
                snapshot: &self.snapshot,
                generation: self.generation,
                conditions: &self.conditions,
                environment: self.snapshot.instance.clone(),
            },
            occurrence: self.occurrence,
            instance: self.instance,
            edge: EdgeRole::SlotRoot,
            visit: ProjectionVisit::Value,
        }
        .canonical()
    }

    /// Materialization keeps original-source judgement. It renews value-proof
    /// generation when evaluation changes the projection, without substituting
    /// a new syntax or binding environment.
    pub(crate) fn materialized(&self, value: &Value<ParserPlaceholder, B>) -> Self {
        let mut slot = self.clone();
        if slot.projection() != Some(value) {
            slot.projection = Projection::Value(value.clone());
            slot.generation = ProjectionGeneration::fresh();
        }
        slot
    }

    /// Substitution installs its live value at the producer. Decisions already
    /// taken by lowering remain immutable; expansion adds its own decisions.
    pub(crate) fn invalidate_judgement(&mut self) {
        self.generation = ProjectionGeneration::fresh();
        self.snapshot = Arc::new(ExpressionReport::empty());
    }

    pub(crate) fn replace_projection(&mut self, value: Value<ParserPlaceholder, B>) {
        self.projection = Projection::Value(value);
        self.generation = ProjectionGeneration::fresh();
        self.snapshot = Arc::new(ExpressionReport::empty());
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

/// Ordinary compatibility checks have no authority to reach these slots.
#[derive(Debug, Clone)]
pub struct NoAuthoredEvidence {
    resource: crate::resource::ResourceId,
    slots: Vec<UnreachedSlot>,
}

impl NoAuthoredEvidence {
    /// Identify unchecked source slots without granting admission authority.
    /// The evidence owner may differ from the resource whose values are checked.
    pub fn for_resource<B: BindingAddress, D>(
        source: &crate::parser::AnalysisResourceRef<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let resource = source.id();
        let slots = source.authored_slots();
        Self {
            resource: resource.clone(),
            slots: slots
                .into_iter()
                .flat_map(|slots| slots.slots.iter())
                .map(|(name, slot)| {
                    let root =
                        authored_chain(slot.root.as_ref()).fold(slot.root.as_ref(), |_, node| node);
                    UnreachedSlot {
                        resource: resource.clone(),
                        attribute: name.clone(),
                        node: root.id,
                        occurrence: slot.occurrence,
                        instance: slot.instance,
                        generation: slot.generation,
                    }
                })
                .collect(),
        }
    }
    pub fn resource_id(&self) -> &crate::resource::ResourceId {
        &self.resource
    }
    pub fn slots(&self) -> &[UnreachedSlot] {
        &self.slots
    }
}

/// A real authored slot whose ordinary report-based check supplied no cursor.
/// It carries identity, never a fabricated judgement or authored proof.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnreachedSlot {
    resource: crate::resource::ResourceId,
    attribute: String,
    node: NodeId,
    occurrence: UseId,
    instance: ExpansionInstanceId,
    generation: ProjectionGeneration,
}
impl UnreachedSlot {
    pub fn resource_id(&self) -> &crate::resource::ResourceId {
        &self.resource
    }
    pub fn attribute(&self) -> &str {
        &self.attribute
    }
    pub fn node_id(&self) -> NodeId {
        self.node
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

/// Per-use completeness, including unavailable authored slot evidence.
#[derive(Debug)]
pub struct CompletenessReport<'s, B: BindingAddress = RelativeBindingName> {
    unreached: Vec<AuthoredUse<'s, B>>,
    work: usize,
    missing: Vec<MissingEvidence>,
    unreached_slots: Vec<UnreachedSlot>,
}

impl<'s, B: BindingAddress> CompletenessReport<'s, B> {
    #[cfg(test)]
    pub(crate) fn work(&self) -> usize {
        self.work
    }

    pub fn correspondence_errors(&self) -> &[MissingEvidence] {
        &self.missing
    }

    /// Slots checked only through main's ordinary report path. These remain
    /// unreached even when this report has no judgement for their source tree.
    pub fn unreached_slots(&self) -> impl Iterator<Item = &UnreachedSlot> {
        self.unreached_slots.iter()
    }

    /// The predicate inspects retained syntax; it does not classify a Value.
    pub fn unreached(
        &self,
        kind: impl Fn(&JudgedNodeRef<'s, B>) -> bool,
    ) -> impl Iterator<Item = &AuthoredUse<'s, B>> {
        self.unreached.iter().filter(move |usage| kind(&usage.node))
    }
}

impl<B: BindingAddress> ExpressionReport<B> {
    pub fn completeness_report(
        &self,
        coverage: &crate::schema::value_validation::WalkCoverage,
    ) -> CompletenessReport<'_, B> {
        fn visit<'s, B: BindingAddress>(
            usage: AuthoredUse<'s, B>,
            coverage: &crate::schema::value_validation::WalkCoverage,
            seen: &mut std::collections::HashSet<(
                NodeId,
                UseId,
                ExpansionInstanceId,
                ProjectionGeneration,
            )>,
            report: &mut CompletenessReport<'s, B>,
        ) -> Result<(), MissingEvidence> {
            let usage = usage.canonical()?;
            if !seen.insert((
                usage.node.node.id,
                usage.occurrence,
                usage.instance,
                usage.node.generation,
            )) {
                return Ok(());
            }
            report.work += 1;
            if !coverage.reached(&usage) {
                report.unreached.push(usage.clone());
            }
            // Attribute slots supply their own per-instance roots. Do not
            // manufacture a second use for the enclosing resource declaration.
            if matches!(usage.node.node.kind, ExprKind::Resource { .. }) {
                return Ok(());
            }
            if let Some(definition) = usage.definition()? {
                visit(definition, coverage, seen, report)?;
            }
            let role = match &usage.node.node.kind {
                ExprKind::Parens => EdgeRole::Parens,
                ExprKind::Definition => EdgeRole::AliasDefinition,
                ExprKind::Secret => EdgeRole::SecretArgument,
                ExprKind::List => EdgeRole::ListItem,
                ExprKind::Map(_) => EdgeRole::MapValue,
                ExprKind::Interpolation { .. } => EdgeRole::InterpolationPart,
                ExprKind::Call(_) | ExprKind::Function { .. } => EdgeRole::CallArgument,
                ExprKind::Integer { .. } => EdgeRole::ArithmeticOperand,
                ExprKind::Access(_) | ExprKind::DynamicAccess => EdgeRole::AccessBase,
                ExprKind::If => EdgeRole::IfBranch,
                ExprKind::Coalesce => EdgeRole::CoalesceOperand,
                _ => usage.edge,
            };
            for index in 0..usage.child_count() {
                let child = if matches!(usage.navigation()?, AuthoredNavigation::Parts) {
                    usage.part(index)?
                } else {
                    usage.child(index, role)?
                };
                visit(child, coverage, seen, report)?;
            }
            Ok(())
        }
        let mut report = CompletenessReport {
            unreached: Vec::new(),
            work: 0,
            missing: Vec::new(),
            unreached_slots: coverage.unreached_slots().cloned().collect(),
        };
        let mut seen = Default::default();
        for root in self.use_roots.iter() {
            // A let initializer is not a qualifying use. Once a real slot use
            // reaches it, completeness is decided by each reference edge.
            let Some(judgement) = self.node(&root.environment, root.node.id) else {
                report.missing.push(MissingEvidence::new(format!(
                    "completeness node {:?}",
                    root.node.id
                )));
                continue;
            };
            let usage = AuthoredUse {
                node: JudgedNodeRef {
                    node: &root.node,
                    judgement,
                    snapshot: self,
                    generation: coverage
                        .current_generation(root.occurrence, root.instance)
                        .map_or(root.generation, |current| root.generation.latest(current)),
                    conditions: &root.conditions,
                    environment: root.environment.clone(),
                },
                occurrence: root.occurrence,
                instance: root.instance,
                edge: EdgeRole::SlotRoot,
                visit: ProjectionVisit::Value,
            };
            let usage = match usage.canonical() {
                Ok(usage) => usage,
                Err(error) => {
                    report.missing.push(error);
                    continue;
                }
            };
            if root.definition && coverage.has_node(usage.node.node.id, &root.environment) {
                continue;
            }
            if let Err(error) = visit(usage, coverage, &mut seen, &mut report) {
                report.missing.push(error);
            }
        }
        report
    }
}

/// Parser-owned attribute inputs, transported with their resource. Empty only
/// for genuinely non-authored producers; a missing live slot is an error.
#[derive(Clone)]
pub struct AuthoredSlots<B: BindingAddress = RelativeBindingName> {
    slots: indexmap::IndexMap<String, JudgedSlot<B>>,
    aliases: std::collections::HashMap<String, String>,
    synthetic: indexmap::IndexMap<String, SyntheticSource>,
    declaration: Option<SlotDeclarationId>,
}
// Admission metadata is not part of a resource's value semantics.
impl<B: BindingAddress> PartialEq for AuthoredSlots<B> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl<B: BindingAddress> std::fmt::Debug for AuthoredSlots<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthoredSlots(..)")
    }
}

impl<B: BindingAddress> Default for AuthoredSlots<B> {
    fn default() -> Self {
        Self {
            slots: Default::default(),
            aliases: Default::default(),
            synthetic: Default::default(),
            declaration: None,
        }
    }
}
impl<B: BindingAddress> AuthoredSlots<B> {
    #[cfg(test)]
    pub(crate) fn unbind_alias_for_test(&mut self, canonical: &str, authored: &str) {
        let slot = self.slots.shift_remove(canonical).unwrap();
        self.slots.insert(authored.to_owned(), slot);
        self.aliases.clear();
    }

    pub fn declaration_id(&self) -> Option<&SlotDeclarationId> {
        self.declaration.as_ref()
    }
    pub(super) fn set_declaration(&mut self, declaration: SlotDeclarationId) {
        self.declaration = Some(declaration);
    }
    pub(crate) fn record_synthetic(&mut self, name: String, source: SyntheticSource) {
        self.synthetic.insert(name, source);
    }
    pub(crate) fn projected_phase(&self, name: &str) -> Option<ProjectedPhase> {
        self.synthetic
            .get(name)
            .copied()
            .map(ProjectedPhase::synthetic)
    }
    pub fn get(&self, name: &str) -> Result<&JudgedSlot<B>, MissingEvidence> {
        self.slots
            .get(name)
            .or_else(|| {
                self.aliases
                    .get(name)
                    .and_then(|authored| self.slots.get(authored))
            })
            .ok_or_else(|| MissingEvidence::new(format!("authored attribute {name}")))
    }
    pub(super) fn use_roots(&self) -> impl Iterator<Item = UseRoot<B>> + '_ {
        self.slots.values().map(|slot| UseRoot {
            node: slot.root.clone(),
            occurrence: slot.occurrence,
            instance: slot.instance,
            generation: slot.generation,
            definition: false,
            environment: slot.snapshot.instance.clone(),
            conditions: slot.conditions.clone(),
        })
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut JudgedSlot<B>)> {
        self.slots
            .iter_mut()
            .map(|(name, slot)| (name.as_str(), slot))
    }
    pub(super) fn selected_schema_kind(&self) -> Option<crate::schema::SchemaKind> {
        self.slots.values().next().map(JudgedSlot::schema_kind)
    }
    pub(crate) fn bind_schema(&mut self, schema: &crate::schema::CompiledResourceSchema) {
        self.aliases.clear();
        for (authored, canonical) in schema.block_name_map() {
            if self.slots.contains_key(&authored) {
                self.aliases.insert(canonical, authored);
            }
        }
    }
    fn authored_name<'n>(
        slots: &indexmap::IndexMap<String, JudgedSlot<B>>,
        aliases: &'n std::collections::HashMap<String, String>,
        name: &'n str,
    ) -> &'n str {
        if slots.contains_key(name) {
            name
        } else {
            aliases.get(name).map(String::as_str).unwrap_or(name)
        }
    }

    pub(crate) fn synchronize(
        &mut self,
        attributes: &indexmap::IndexMap<String, Value<ParserPlaceholder, B>>,
    ) {
        for (name, value) in attributes {
            let authored = Self::authored_name(&self.slots, &self.aliases, name);
            if let Some(slot) = self.slots.get_mut(authored)
                && slot.projection() != Some(value)
            {
                slot.replace_projection(value.clone());
            }
        }
    }

    pub(crate) fn attach_snapshot(
        &mut self,
        snapshot: Result<Arc<ExpressionReport<B>>, MissingEvidence>,
    ) {
        for slot in self.slots.values_mut() {
            // Keep the exact typed attachment outcome. A failed association
            // cannot silently turn into an empty "judged" snapshot.
            slot.snapshot_error = match &snapshot {
                Ok(snapshot) => slot.attach_snapshot(snapshot.clone()).err(),
                Err(error) => {
                    slot.snapshot = Arc::new(ExpressionReport::empty());
                    Some(error.clone())
                }
            };
        }
    }
    pub(crate) fn reinstance(&mut self) {
        let instance = ExpansionInstanceId::fresh();
        for slot in self.slots.values_mut() {
            slot.instance = instance;
            slot.occurrence = UseId::fresh();
            slot.generation = ProjectionGeneration::fresh();
            // Identity-only cloning changes neither syntax, projection nor
            // binding environment. Materialized slots retain that judgement;
            // later substitution/remapping still invalidates it explicitly.
            // Pending slots must first acquire their instance projection.
            if slot.projection().is_none() {
                slot.snapshot = Arc::new(ExpressionReport::empty());
            }
        }
    }
    pub(crate) fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
    ) -> AuthoredSlots<C> {
        let mut memo = std::collections::HashMap::new();
        AuthoredSlots {
            aliases: self.aliases,
            synthetic: self.synthetic,
            declaration: self.declaration,
            slots: self
                .slots
                .into_iter()
                .map(|(name, slot)| {
                    let projection = slot.projection.map_bindings(map);
                    let expression = super::ExprValue {
                        authored: slot.root,
                        payload: Projection::Pending,
                    }
                    .map_bindings(map, &mut memo);
                    (
                        name,
                        JudgedSlot {
                            root: expression.authored,
                            projection,
                            occurrence: slot.occurrence,
                            instance: slot.instance,
                            generation: ProjectionGeneration::fresh(),
                            snapshot: Arc::new(ExpressionReport::empty()),
                            snapshot_error: None,
                            schema_kind: slot.schema_kind,
                            conditions: slot.conditions,
                        },
                    )
                })
                .collect(),
        }
    }
    pub(crate) fn record_condition_selections(&mut self, selected: &super::ConditionRecorder) {
        // The expansion's immutable version includes newly decided nodes,
        // even when their parse-time projections were unavailable. Slots share
        // that store and retain their original parser decisions underneath it.
        let selected = selected.snapshot();
        if selected.version == 0 {
            return;
        }
        for slot in self.slots.values_mut() {
            slot.conditions = selected.over(&slot.conditions);
            slot.generation = ProjectionGeneration::fresh();
        }
    }

    pub(crate) fn replace_projections(
        &mut self,
        attributes: &indexmap::IndexMap<String, Value<ParserPlaceholder, B>>,
    ) {
        let mut updated = std::collections::HashSet::new();
        for (name, value) in attributes {
            let authored = Self::authored_name(&self.slots, &self.aliases, name);
            if let Some(slot) = self.slots.get_mut(authored) {
                updated.insert(slot.occurrence);
                slot.replace_projection(value.clone());
            }
        }
        // An absent or not-yet-bound replacement cannot retain an old value
        // proof. Schema binding can materialize that slot at the next judgement.
        for slot in self.slots.values_mut() {
            if !updated.contains(&slot.occurrence) {
                slot.projection = Projection::Pending;
                slot.generation = ProjectionGeneration::fresh();
                slot.snapshot = Arc::new(ExpressionReport::empty());
            }
        }
    }
}
impl AuthoredSlots {
    pub(in crate::parser) fn insert(
        &mut self,
        name: String,
        expression: super::ExprValue,
        kind: crate::schema::SchemaKind,
        instance: ExpansionInstanceId,
        conditions: ConditionSnapshot,
    ) {
        let mut slot = JudgedSlot::from_expression(expression, kind, instance);
        slot.conditions = conditions;
        self.slots.insert(name, slot);
    }
}
