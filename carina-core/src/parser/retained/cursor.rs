//! Closed authored inputs, shared syntax/judgement snapshots and per-use coverage.

use super::{
    AuthoredExpr, ExprKind, ExpressionInstance, ExpressionLocation, ExpressionReport, JudgedExpr,
    MissingEvidence, Projection, SinkExpression,
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
    pub(super) projections:
        Arc<std::collections::HashMap<NodeId, Projection<ParserPlaceholder, B>>>,
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
            projections: Arc::new(
                self.projections
                    .iter()
                    .map(|(id, value)| {
                        (
                            *id,
                            match value {
                                Projection::Pending => Projection::Pending,
                                Projection::Value(value) => {
                                    Projection::Value(value.clone().map_bindings(map))
                                }
                            },
                        )
                    })
                    .collect(),
            ),
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
    projections: &'s std::collections::HashMap<NodeId, Projection<ParserPlaceholder, B>>,
}

impl<B: BindingAddress> std::fmt::Debug for JudgedNodeRef<'_, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgedNodeRef")
            .field("node", &self.node.id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
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

    pub(crate) fn environment(&self) -> &ExpressionInstance {
        &self.environment
    }

    /// Direct authored children in syntax order, paired with the same snapshot
    /// and projection generation. Slot sealing verifies this correspondence.
    pub fn children(&self) -> impl Iterator<Item = JudgedNodeRef<'s, B>> + '_ {
        self.node.children.iter().map(move |child| JudgedNodeRef {
            node: &child.authored,
            judgement: self
                .snapshot
                .node(&self.environment, child.node_id())
                .expect("sealed authored child judgement"),
            snapshot: self.snapshot,
            generation: self.generation,
            projections: self.projections,
            environment: self.environment.clone(),
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
            projections: self.projections,
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

impl<'s, B: BindingAddress> AuthoredUse<'s, B> {
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
        let judgement = self
            .node
            .snapshot
            .node(&expression.authored.location.instance, expression.node_id())
            .ok_or_else(|| MissingEvidence::new(format!("definition {binding}")))?;
        Ok(Some(Self {
            node: JudgedNodeRef {
                node: &expression.authored,
                judgement,
                snapshot: self.node.snapshot,
                generation: self.node.generation,
                projections: self.node.projections,
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
        }))
    }

    pub(crate) fn known_condition(&self) -> Result<bool, MissingEvidence> {
        let condition = self
            .node
            .node
            .children
            .first()
            .ok_or_else(|| MissingEvidence::new("if condition"))?;
        let value = match self.node.projections.get(&condition.node_id()) {
            Some(Projection::Value(value)) => value,
            Some(Projection::Pending) => {
                return Err(MissingEvidence::new(
                    "substituted if condition projection is unavailable",
                ));
            }
            None => self
                .node
                .snapshot
                .projections
                .get(&(self.node.environment.clone(), condition.node_id()))
                .or_else(|| condition.projection())
                .ok_or_else(|| MissingEvidence::new("if condition projection"))?,
        };
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
        Ok(Self {
            node: JudgedNodeRef {
                node: &child.authored,
                judgement,
                snapshot: self.node.snapshot,
                generation: self.node.generation,
                projections: self.node.projections,
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
        })
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
#[allow(
    dead_code,
    reason = "normalization/replay source cursors are reserved for their later admission boundaries"
)]
pub(crate) enum SyntheticSource {
    DefaultTagsMerge(crate::executor::normalized::DefaultTagsMergeSource),
    ProviderNormalization(crate::executor::normalized::ProviderNormalizationSource),
    SavedPlanReplay(crate::saved_replay::SavedPlanReplaySource),
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

    #[allow(
        dead_code,
        reason = "normalization/replay currently enter through their Plan/Apply proofs"
    )]
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

impl<'s, B: BindingAddress> AuthoredCursor<'s, B> {
    pub(crate) fn selection(&self) -> Self {
        let mut cursor = self.clone();
        match &mut cursor {
            Self::Authored(usage) | Self::CoveredBy(usage) => {
                usage.visit = ProjectionVisit::Selection
            }
            Self::Projected(_) => {}
        }
        cursor
    }
    pub(crate) fn is_selection(&self) -> bool {
        match self {
            Self::Authored(usage) | Self::CoveredBy(usage) => {
                usage.visit == ProjectionVisit::Selection
            }
            Self::Projected(_) => false,
        }
    }
    pub(crate) fn identity(
        &self,
    ) -> Option<(NodeId, UseId, ExpansionInstanceId, ProjectionGeneration)> {
        match self {
            Self::Authored(usage) | Self::CoveredBy(usage) => Some((
                usage.node.node.id,
                usage.occurrence,
                usage.instance,
                usage.node.generation,
            )),
            Self::Projected(_) => None,
        }
    }
    pub(crate) fn for_map_key(&self) -> Self {
        match self {
            Self::Authored(usage) | Self::CoveredBy(usage) => {
                let mut usage = usage.clone();
                usage.edge = EdgeRole::MapKey;
                Self::CoveredBy(usage)
            }
            Self::Projected(phase) => Self::Projected(*phase),
        }
    }

    pub(crate) fn covered(&self) -> Self {
        match self {
            Self::Authored(usage) | Self::CoveredBy(usage) => Self::CoveredBy(usage.clone()),
            Self::Projected(phase) => Self::Projected(*phase),
        }
    }
    pub(crate) fn child(&self, index: usize, edge: EdgeRole) -> Result<Self, MissingEvidence> {
        match self {
            Self::Authored(usage) => match usage.form() {
                // A substituted parameter/loop leaf retains its own authored
                // source; its projected container has no authored child edges.
                AuthoredForm::Reference | AuthoredForm::Binding | AuthoredForm::Literal => {
                    Ok(self.covered())
                }
                AuthoredForm::List
                | AuthoredForm::If
                | AuthoredForm::Coalesce
                | AuthoredForm::Secret => usage.child(index, edge).map(Self::Authored),
                _ => Err(MissingEvidence::new(
                    "required authored child correspondence",
                )),
            },
            Self::CoveredBy(_) | Self::Projected(_) => Ok(self.clone()),
        }
    }
    pub(crate) fn field_child(
        &self,
        key: &str,
        alias: Option<&str>,
    ) -> Result<Self, MissingEvidence> {
        let key = match self {
            Self::Authored(usage)
                if usage.form() == AuthoredForm::Map
                    && !usage.map_keys().iter().any(|candidate| candidate == key) =>
            {
                alias.unwrap_or(key)
            }
            Self::Authored(_) | Self::CoveredBy(_) | Self::Projected(_) => key,
        };
        self.map_child(key, EdgeRole::StructField)
    }

    pub(crate) fn map_child(&self, key: &str, edge: EdgeRole) -> Result<Self, MissingEvidence> {
        match self {
            Self::Authored(usage) => match usage.form() {
                AuthoredForm::Map => usage.map_child(key, edge).map(Self::Authored),
                AuthoredForm::Reference | AuthoredForm::Binding | AuthoredForm::Literal => {
                    Ok(self.covered())
                }
                _ => Err(MissingEvidence::new("required authored map correspondence")),
            },
            Self::CoveredBy(_) | Self::Projected(_) => Ok(self.clone()),
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
    schema_kind: crate::schema::SchemaKind,
    node_projections: Arc<std::collections::HashMap<NodeId, Projection<ParserPlaceholder, B>>>,
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
    /// Capture the complete parser sink. Only a current report can subsequently
    /// seal its cursor; the parser cannot mint judgement from a reduced value.
    pub(in crate::parser) fn from_sink(
        sink: SinkExpression,
        schema_kind: crate::schema::SchemaKind,
        instance: ExpansionInstanceId,
    ) -> Self {
        Self::from_expression(sink.expression, schema_kind, instance)
    }

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
            schema_kind,
            node_projections: Arc::default(),
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
        Ok(())
    }

    pub fn projection(&self) -> Option<&Value<ParserPlaceholder, B>> {
        match &self.projection {
            Projection::Value(value) => Some(value),
            Projection::Pending => None,
        }
    }

    pub fn cursor(&self) -> Result<AuthoredCursor<'_, B>, MissingEvidence> {
        if self
            .snapshot
            .node(&self.snapshot.instance, self.root.id)
            .is_none()
        {
            return Err(MissingEvidence::new(format!(
                "slot {:?} requires a current judgement",
                self.root.id
            )));
        }
        Ok(AuthoredCursor::Authored(AuthoredUse {
            node: JudgedNodeRef {
                node: &self.root,
                judgement: self
                    .snapshot
                    .node(&self.snapshot.instance, self.root.id)
                    .expect("checked current slot judgement"),
                snapshot: &self.snapshot,
                generation: self.generation,
                projections: &self.node_projections,
                environment: self.snapshot.instance.clone(),
            },
            occurrence: self.occurrence,
            instance: self.instance,
            edge: EdgeRole::SlotRoot,
            visit: ProjectionVisit::Value,
        }))
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

    pub(crate) fn map_archived_projections<E>(
        &mut self,
        mut transform: impl FnMut(
            &Value<ParserPlaceholder, B>,
        ) -> Result<Value<ParserPlaceholder, B>, E>,
    ) {
        let nodes = self
            .node_projections
            .iter()
            .map(|(id, projection)| {
                let projection = match projection {
                    Projection::Pending => Projection::Pending,
                    Projection::Value(value) => match transform(value) {
                        Ok(value) => Projection::Value(value),
                        // A structural handle cannot become a Value. Its old
                        // archived payload is invalidated; required live access
                        // to it returns a typed correspondence error.
                        Err(_) => Projection::Pending,
                    },
                };
                (*id, projection)
            })
            .collect();
        self.node_projections = Arc::new(nodes);
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

/// Per-use completeness, including definition uses hidden by a projection.
#[derive(Debug)]
pub struct CompletenessReport<'s, B: BindingAddress = RelativeBindingName> {
    unreached: Vec<AuthoredUse<'s, B>>,
    work: usize,
    missing: Vec<MissingEvidence>,
}

impl<'s, B: BindingAddress> CompletenessReport<'s, B> {
    #[cfg(test)]
    pub(crate) fn work(&self) -> usize {
        self.work
    }

    pub fn correspondence_errors(&self) -> &[MissingEvidence] {
        &self.missing
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
                visit(usage.child(index, role)?, coverage, seen, report)?;
            }
            Ok(())
        }
        let mut report = CompletenessReport {
            unreached: Vec::new(),
            work: 0,
            missing: Vec::new(),
        };
        let mut seen = Default::default();
        for root in self.use_roots.iter() {
            // A let initializer is not a qualifying use. Once a real slot use
            // reaches it, completeness is decided by each reference edge.
            if root.definition && coverage.has_node(root.node.id, &root.environment) {
                continue;
            }
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
                    projections: &root.projections,
                    environment: root.environment.clone(),
                },
                occurrence: root.occurrence,
                instance: root.instance,
                edge: EdgeRole::SlotRoot,
                visit: ProjectionVisit::Value,
            };
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
            projections: slot.node_projections.clone(),
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
    pub(crate) fn synchronize(
        &mut self,
        attributes: &indexmap::IndexMap<String, Value<ParserPlaceholder, B>>,
    ) {
        for (name, value) in attributes {
            let authored = if self.slots.contains_key(name) {
                name
            } else {
                self.aliases.get(name).unwrap_or(name)
            };
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
            // A live association failure remains a typed error at cursor().
            match &snapshot {
                Ok(snapshot) => {
                    let _ = slot.attach_snapshot(snapshot.clone());
                }
                Err(_) => slot.snapshot = Arc::new(ExpressionReport::empty()),
            }
        }
    }
    pub(crate) fn reinstance(&mut self) {
        let instance = ExpansionInstanceId::fresh();
        for slot in self.slots.values_mut() {
            slot.instance = instance;
            slot.occurrence = UseId::fresh();
            slot.generation = ProjectionGeneration::fresh();
            slot.snapshot = Arc::new(ExpressionReport::empty());
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
                    let projection = match slot.projection {
                        Projection::Pending => Projection::Pending,
                        Projection::Value(value) => Projection::Value(value.map_bindings(map)),
                    };
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
                            schema_kind: slot.schema_kind,
                            node_projections: Arc::new(
                                slot.node_projections
                                    .iter()
                                    .map(|(id, value)| {
                                        (
                                            *id,
                                            match value {
                                                Projection::Pending => Projection::Pending,
                                                Projection::Value(value) => Projection::Value(
                                                    value.clone().map_bindings(map),
                                                ),
                                            },
                                        )
                                    })
                                    .collect(),
                            ),
                        },
                    )
                })
                .collect(),
        }
    }
    pub(crate) fn record_condition_selections(
        &mut self,
        selected: &std::collections::HashMap<NodeId, bool>,
    ) {
        for slot in self.slots.values_mut() {
            for (id, projection) in Arc::make_mut(&mut slot.node_projections) {
                if let Some(value) = selected.get(id) {
                    *projection = Projection::Value(Value::Concrete(
                        crate::resource::ConcreteValue::Bool(*value),
                    ));
                }
            }
        }
    }
    pub(crate) fn replace_expanded_projections(
        &mut self,
        attributes: &indexmap::IndexMap<String, Value<crate::resource::NoPlaceholder, B>>,
    ) {
        let attributes = attributes
            .iter()
            .map(|(name, value)| (name.clone(), value.clone().into_placeholder_type()))
            .collect();
        self.synchronize(&attributes);
    }

    pub(crate) fn replace_projections(
        &mut self,
        attributes: &indexmap::IndexMap<String, Value<ParserPlaceholder, B>>,
    ) {
        let mut updated = std::collections::HashSet::new();
        for (name, value) in attributes {
            let authored = if self.slots.contains_key(name) {
                name
            } else {
                self.aliases.get(name).unwrap_or(name)
            };
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
        sink: SinkExpression,
        kind: crate::schema::SchemaKind,
        instance: ExpansionInstanceId,
        projections: std::collections::HashMap<
            NodeId,
            Value<ParserPlaceholder, RelativeBindingName>,
        >,
    ) {
        self.insert_slot(
            name,
            JudgedSlot::from_sink(sink, kind, instance),
            projections,
        );
    }

    pub(in crate::parser) fn insert_container(
        &mut self,
        name: String,
        expression: super::ExprValue,
        kind: crate::schema::SchemaKind,
        instance: ExpansionInstanceId,
        projections: std::collections::HashMap<
            NodeId,
            Value<ParserPlaceholder, RelativeBindingName>,
        >,
    ) {
        self.insert_slot(
            name,
            JudgedSlot::from_expression(expression, kind, instance),
            projections,
        );
    }

    fn insert_slot(
        &mut self,
        name: String,
        mut slot: JudgedSlot,
        projections: std::collections::HashMap<
            NodeId,
            Value<ParserPlaceholder, RelativeBindingName>,
        >,
    ) {
        slot.node_projections = Arc::new(
            projections
                .into_iter()
                .map(|(id, value)| (id, Projection::Value(value)))
                .collect(),
        );
        self.slots.insert(name, slot);
    }
}
