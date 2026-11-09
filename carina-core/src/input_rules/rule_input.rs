//! The sole immutable input capability for authored, planned and dispatched rules.
//! All knowledge, membership, expression and masking queries originate here.
use super::*;
use crate::resource::knowledge::{Classification, ClassifiedRef, KnowledgeContext};
use crate::resource::{DiagnosticScope, ResolvedIgnoreChanges};
use yoke::{Yoke, Yokeable};

/// An immutable current-stage desired input. Only stage-specific constructors
/// can create one; neither transport nor a sparse patch is a desired input.
///
/// ```compile_fail,E0277
/// use carina_core::{input_rules::RuleInput, provider::UpdatePatch};
/// fn forge(patch: UpdatePatch) -> RuleInput { patch.into() }
/// ```
/// ```compile_fail,E0277
/// use carina_core::{input_rules::RuleInput, resource::Value};
/// fn forge(transport: indexmap::IndexMap<String, Value>) -> RuleInput { transport.into() }
/// ```
pub struct RuleInput {
    graph: Yoke<EvidenceGraph<'static>, Box<InputStorage>>,
}

impl std::fmt::Debug for RuleInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RuleInput(<protected stage input>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct NodeId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Membership {
    Present,
    Possible,
}

#[derive(Clone, Copy)]
enum Completeness {
    Replacement,
    Unsent,
}

/// A proof whose only producers are complete values and explicit removals.
#[derive(Clone, Debug)]
pub struct AbsenceProof {
    depth: usize,
    _origin: AbsenceOrigin,
}
#[derive(Clone, Debug)]
enum AbsenceOrigin {
    CompleteNode,
    CompleteRoot,
    RemovedRoot,
}

impl AbsenceProof {
    pub(super) fn depth(&self) -> usize {
        self.depth
    }
}

/// Patch omission cannot be used as an absence witness.
///
/// ```compile_fail,E0277
/// use carina_core::input_rules::{InputRead, PatchRead};
/// fn forge() -> InputRead<'static> {
///     InputRead::Absent(PatchRead::NotInPatch.into())
/// }
/// ```
pub enum PatchRead<'a> {
    Set(NodeRef<'a>),
    Remove(AbsenceProof),
    NotInPatch,
}

pub enum InputRead<'a> {
    Present(NodeRef<'a>),
    Absent(AbsenceProof),
    Withheld(Withheld<'a>),
    Failed(FailureRef<'a>),
}

#[derive(Clone, Copy, Debug)]
pub enum WithheldReason {
    Ignored,
    UnsentOmission,
    UnknownMembership,
}

pub struct Withheld<'a> {
    reason: WithheldReason,
    node: Option<NodeRef<'a>>,
}
pub struct FailureRef<'a> {
    node: NodeRef<'a>,
}

#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    input: &'a RuleInput,
    id: NodeId,
}

impl std::fmt::Debug for NodeRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NodeRef(<protected>)")
    }
}

#[derive(Clone, Copy)]
pub(super) enum InstanceRef {
    Resource,
    Node(NodeId),
    Deferred {
        node: Option<NodeId>,
        failed: bool,
        scope: DiagnosticScope,
    },
}

/// An expression can only be obtained by following a graph node's carried
/// correspondence. It never looks up an authored resource by id/name/index.
pub(super) struct BoundExpression<'a> {
    value: &'a InputValue,
    declarations: &'a TypeFacts,
}

impl BoundExpression<'_> {
    pub(super) fn value(&self) -> &InputValue {
        self.value
    }
    pub(super) fn declarations(&self) -> &TypeFacts {
        self.declarations
    }
}

#[derive(Clone)]
enum PatchOperation {
    Set(InputValue),
    Remove,
}

struct UpdateOverlay {
    operations: IndexMap<String, PatchOperation>,
    ignored: IgnoreIndex,
}

#[derive(Default)]
struct IgnoreIndex {
    roots: HashMap<String, IgnoreNode>,
}

#[derive(Default)]
struct IgnoreNode {
    terminal: bool,
    names: HashMap<String, IgnoreNode>,
    indices: HashMap<usize, IgnoreNode>,
}

/// A cursor retains the matched prefix, so descending never rescans an ignored
/// path from its root. A terminal ancestor continues to withhold every child.
#[derive(Clone, Copy, Default)]
struct IgnoreCursor<'a> {
    node: Option<&'a IgnoreNode>,
    blocked: bool,
}

impl IgnoreIndex {
    fn new(policy: &ResolvedIgnoreChanges) -> Self {
        use crate::schema::LeafPathSegment;
        let mut index = Self::default();
        for path in policy.iter() {
            #[cfg(test)]
            record_ignore_path_work(1);
            let mut node = index.roots.entry(path.attribute().into()).or_default();
            for segment in path.leaf().segments() {
                #[cfg(test)]
                record_ignore_path_work(1);
                node = match segment {
                    LeafPathSegment::StructField(name) => {
                        node.names.entry(name.clone()).or_default()
                    }
                    LeafPathSegment::MapKey(name) => node
                        .names
                        .entry(name.expose_plaintext().into())
                        .or_default(),
                    LeafPathSegment::ListIndex(index) => node.indices.entry(*index).or_default(),
                };
            }
            node.terminal = true;
        }
        index
    }

    fn root(&self, name: &str) -> IgnoreCursor<'_> {
        #[cfg(test)]
        record_ignore_path_work(1);
        IgnoreCursor::new(self.roots.get(name), false)
    }
}

impl<'a> IgnoreCursor<'a> {
    fn new(node: Option<&'a IgnoreNode>, ancestor_blocked: bool) -> Self {
        Self {
            node,
            blocked: ancestor_blocked || node.is_some_and(|node| node.terminal),
        }
    }

    fn field(self, name: &str) -> Self {
        #[cfg(test)]
        record_ignore_path_work(1);
        Self::new(
            self.node.and_then(|node| node.names.get(name)),
            self.blocked,
        )
    }

    fn item(self, index: usize) -> Self {
        #[cfg(test)]
        record_ignore_path_work(1);
        Self::new(
            self.node.and_then(|node| node.indices.get(&index)),
            self.blocked,
        )
    }

    fn masked_children(self) -> HashSet<String> {
        self.node.map_or_else(HashSet::new, |node| {
            #[cfg(test)]
            record_ignore_path_work(node.names.len());
            node.names.keys().cloned().collect()
        })
    }
}

struct InputStorage {
    desired: InputAttributes,
    update: Option<UpdateOverlay>,
    declarations: Arc<TypeFacts>,
    id: ResourceId,
    accepts_defaults: bool,
    declaration: crate::parser::DeclarationProvenance,
}

#[derive(Yokeable)]
struct EvidenceGraph<'a> {
    nodes: Vec<EvidenceNode<'a>>,
    roots: HashMap<String, NodeId>,
    removed: HashSet<String>,
    update: bool,
}

struct EvidenceNode<'a> {
    proof: Option<Classification<'a, ParserPlaceholder>>,
    derived_failure: bool,
    children: Children<'a>,
    scope: DiagnosticScope,
    enclosing_failure: bool,
    blocked: bool,
    masked_children: HashSet<String>,
    membership: Membership,
    completeness: Completeness,
    correspondence: ExpressionCorrespondence,
    full_proof: bool,
}

enum Children<'a> {
    Map {
        members: IndexMap<&'a str, NodeId>,
        complete: bool,
        tail: bool,
    },
    List(Vec<NodeId>),
    Scalar,
}

#[derive(Clone, Copy)]
enum ExpressionCorrespondence {
    RetainedCurrent,
    Erased,
}

impl RuleInput {
    fn construct(storage: InputStorage) -> Arc<Self> {
        Arc::new(Self {
            graph: Yoke::<EvidenceGraph<'static>, Box<InputStorage>>::attach_to_cart(
                Box::new(storage),
                |storage| EvidenceGraph::build(storage),
            ),
        })
    }

    pub(super) fn for_authored(
        source: &authored::AuthoredResource<'_>,
        schema: &ValidatedResourceSchema,
        declarations: Arc<TypeFacts>,
        schemas: &SchemaRegistry,
    ) -> crate::provider::HostResult<Arc<Self>> {
        let resource = source.resource();
        let mut projected = crate::resource::Resource::<ModuleSource, ParserPlaceholder>::from_id(
            resource.id.clone(),
        );
        projected.attributes = project_attributes(&resource.attributes);
        projected.declaration = resource.declaration.clone();
        let conflicts = crate::schema::resolve_block_names_detailed(
            std::slice::from_mut(&mut projected),
            schemas,
        );
        if !conflicts.is_empty() {
            return Err(crate::provider::HostError::invalid_input(
                conflicts
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }
        Ok(Self::complete(
            projected.id,
            projected.attributes,
            projected.declaration,
            schema,
            declarations,
        ))
    }

    pub(super) fn for_plan_create<S>(
        resource: &crate::resource::ResolvedResource<S>,
        schema: &ValidatedResourceSchema,
        declarations: Arc<TypeFacts>,
    ) -> Arc<Self> {
        Self::complete(
            resource.id.clone(),
            project_attributes(&resource.attributes),
            resource.declaration.clone(),
            schema,
            declarations,
        )
    }

    pub(super) fn for_plan_deferred_create<
        S,
        P: crate::resource::PlaceholderKind,
        B: crate::resource::BindingAddress,
        D,
    >(
        resource: &crate::parser::DeferredForTemplateResource<S, P, B, D>,
        schema: &ValidatedResourceSchema,
        declarations: Arc<TypeFacts>,
    ) -> Arc<Self> {
        Self::complete(
            resource.id().clone(),
            project_attributes(resource.attributes()),
            resource.as_resource().declaration.clone(),
            schema,
            declarations,
        )
    }

    pub(super) fn for_plan_deferred_replace<
        S,
        P: crate::resource::PlaceholderKind,
        B: crate::resource::BindingAddress,
        D,
    >(
        resource: &crate::parser::DeferredForTemplateResource<S, P, B, D>,
        schema: &ValidatedResourceSchema,
        declarations: Arc<TypeFacts>,
    ) -> Arc<Self> {
        Self::for_plan_deferred_create(resource, schema, declarations)
    }

    pub(super) fn for_plan_update<S>(
        resource: &crate::resource::ResolvedResource<S>,
        previous: &crate::resource::SnapshotState,
        changed: &[String],
        schema: &ValidatedResourceSchema,
        declarations: Arc<TypeFacts>,
    ) -> Result<Arc<Self>, crate::provider::UpdatePatchError> {
        let patch =
            crate::provider::plan_update_patch(changed, resource, previous, schema.as_schema())?;
        Ok(Self::construct(InputStorage {
            desired: project_attributes(&resource.attributes),
            update: Some(UpdateOverlay::new(
                &patch,
                resource.ignore_changes().clone(),
            )),
            declarations,
            id: resource.id.clone(),
            accepts_defaults: accepts_defaults(schema),
            declaration: resource.declaration.clone(),
        }))
    }

    pub(crate) fn for_dispatch_create(
        desired: &crate::executor::provider_ready::ProtectedDesiredTree<'_>,
        schema: &ValidatedResourceSchema,
    ) -> Arc<Self> {
        Self::complete(
            desired.id().clone(),
            project_attributes(desired.attributes()),
            Default::default(),
            schema,
            Arc::new(TypeFacts::default()),
        )
    }

    pub(crate) fn for_dispatch_update(
        desired: &crate::executor::provider_ready::ProtectedDesiredTree<'_>,
        patch: &crate::executor::provider_ready::AnnotatedExactPatch,
        schema: &ValidatedResourceSchema,
    ) -> Arc<Self> {
        Self::construct(InputStorage {
            desired: project_attributes(desired.attributes()),
            update: Some(UpdateOverlay::new(
                patch.as_patch(),
                desired.ignored().clone(),
            )),
            declarations: Arc::new(TypeFacts::default()),
            id: desired.id().clone(),
            accepts_defaults: accepts_defaults(schema),
            declaration: Default::default(),
        })
    }

    fn complete(
        id: ResourceId,
        desired: InputAttributes,
        declaration: crate::parser::DeclarationProvenance,
        schema: &ValidatedResourceSchema,
        declarations: Arc<TypeFacts>,
    ) -> Arc<Self> {
        Self::construct(InputStorage {
            desired,
            update: None,
            declarations,
            id,
            accepts_defaults: accepts_defaults(schema),
            declaration,
        })
    }

    fn storage(&self) -> &InputStorage {
        self.graph.backing_cart()
    }
    fn graph(&self) -> &EvidenceGraph<'_> {
        self.graph.get()
    }
    pub(super) fn resource_id(&self) -> &ResourceId {
        &self.storage().id
    }
    pub(super) fn declaration(&self) -> &crate::parser::DeclarationProvenance {
        &self.storage().declaration
    }
    pub(super) fn is_update(&self) -> bool {
        self.graph().update
    }
    pub(super) fn changed(&self, root: &str) -> bool {
        self.storage()
            .update
            .as_ref()
            .is_some_and(|update| update.operations.contains_key(root))
    }

    pub(super) fn patch_read(&self, root: &str) -> PatchRead<'_> {
        match self
            .storage()
            .update
            .as_ref()
            .and_then(|update| update.operations.get(root))
        {
            Some(PatchOperation::Set(_)) => PatchRead::Set(NodeRef {
                input: self,
                id: self.graph().roots[root],
            }),
            Some(PatchOperation::Remove) => PatchRead::Remove(AbsenceProof {
                depth: 0,
                _origin: AbsenceOrigin::RemovedRoot,
            }),
            None => PatchRead::NotInPatch,
        }
    }

    pub(super) fn read(&self, owner: InstanceRef, path: &InputPath) -> InputRead<'_> {
        let mut parts = path.segments().enumerate();
        let mut node = match owner {
            InstanceRef::Resource => {
                let (_, root) = parts.next().expect("admitted path");
                match self.root(root) {
                    InputRead::Present(node) => node,
                    other => return other,
                }
            }
            InstanceRef::Node(id) => NodeRef { input: self, id },
            InstanceRef::Deferred { node, failed, .. } => {
                return if failed {
                    match node {
                        Some(id) => InputRead::Failed(FailureRef {
                            node: NodeRef { input: self, id },
                        }),
                        None => unreachable!("failure has a graph source"),
                    }
                } else {
                    InputRead::Withheld(Withheld {
                        reason: WithheldReason::UnknownMembership,
                        node: node.map(|id| NodeRef { input: self, id }),
                    })
                };
            }
        };
        for (depth, name) in parts {
            match self.field(node, name, depth) {
                InputRead::Present(child) => node = child,
                other => return other,
            }
        }
        self.finish(node)
    }

    pub(super) fn root(&self, name: &str) -> InputRead<'_> {
        if let Some(&id) = self.graph().roots.get(name) {
            return self.finish(NodeRef { input: self, id });
        }
        if !self.is_update() || self.graph().removed.contains(name) {
            InputRead::Absent(AbsenceProof {
                depth: 0,
                _origin: if self.graph().removed.contains(name) {
                    AbsenceOrigin::RemovedRoot
                } else {
                    AbsenceOrigin::CompleteRoot
                },
            })
        } else {
            InputRead::Withheld(Withheld {
                reason: WithheldReason::UnsentOmission,
                node: None,
            })
        }
    }

    fn field<'a>(&'a self, node: NodeRef<'a>, name: &str, depth: usize) -> InputRead<'a> {
        let raw = node.raw();
        if raw.blocked {
            return InputRead::Withheld(Withheld {
                reason: WithheldReason::Ignored,
                node: Some(node),
            });
        }
        if node.computation_failed() {
            return InputRead::Failed(FailureRef { node });
        }
        let Children::Map {
            members, complete, ..
        } = &raw.children
        else {
            return InputRead::Withheld(Withheld {
                reason: WithheldReason::UnknownMembership,
                node: Some(node),
            });
        };
        if let Some(&id) = members.get(name) {
            return self.finish(NodeRef { input: self, id });
        }
        if raw.masked_children.contains(name) {
            return InputRead::Withheld(Withheld {
                reason: WithheldReason::Ignored,
                node: None,
            });
        }
        if *complete && matches!(raw.completeness, Completeness::Replacement) {
            InputRead::Absent(AbsenceProof {
                depth,
                _origin: AbsenceOrigin::CompleteNode,
            })
        } else {
            InputRead::Withheld(Withheld {
                reason: if *complete {
                    WithheldReason::UnsentOmission
                } else {
                    WithheldReason::UnknownMembership
                },
                node: None,
            })
        }
    }

    fn finish<'a>(&'a self, node: NodeRef<'a>) -> InputRead<'a> {
        if node.raw().blocked {
            return InputRead::Withheld(Withheld {
                reason: WithheldReason::Ignored,
                node: Some(node),
            });
        }
        if matches!(node.raw().membership, Membership::Possible) && node.failed() {
            return InputRead::Failed(FailureRef { node });
        }
        if matches!(node.raw().membership, Membership::Possible) {
            return InputRead::Withheld(Withheld {
                reason: WithheldReason::UnknownMembership,
                node: Some(node),
            });
        }
        if matches!(node.raw().children, Children::Scalar) && node.failed() {
            return InputRead::Failed(FailureRef { node });
        }
        InputRead::Present(node)
    }

    pub(super) fn expression<'a>(&'a self, node: NodeRef<'a>) -> Option<BoundExpression<'a>> {
        if node.raw().blocked
            || !matches!(
                node.raw().correspondence,
                ExpressionCorrespondence::RetainedCurrent
            )
        {
            return None;
        }
        Some(BoundExpression {
            value: ClassifiedRef::from(node.raw().proof.as_ref()?).diagnostic_value(),
            declarations: &self.storage().declarations,
        })
    }
}

impl UpdateOverlay {
    fn new(patch: &crate::provider::UpdatePatch, ignored: ResolvedIgnoreChanges) -> Self {
        Self {
            operations: patch
                .ops
                .iter()
                .map(|op| {
                    (
                        op.key().to_owned(),
                        match op.sent_value() {
                            Some(value) => PatchOperation::Set(project_known_value(value)),
                            None => PatchOperation::Remove,
                        },
                    )
                })
                .collect(),
            ignored: IgnoreIndex::new(&ignored),
        }
    }
}

impl<'a> EvidenceGraph<'a> {
    fn build(input: &'a InputStorage) -> Self {
        let mut graph = Self {
            nodes: Vec::new(),
            roots: HashMap::new(),
            removed: HashSet::new(),
            update: input.update.is_some(),
        };
        let mut sources = input
            .desired
            .iter()
            .map(|(key, value)| (key.as_str(), value))
            .collect::<IndexMap<_, _>>();
        if let Some(update) = &input.update {
            for (key, op) in &update.operations {
                match op {
                    PatchOperation::Set(value) => {
                        sources.insert(key, value);
                    }
                    PatchOperation::Remove => {
                        sources.shift_remove(key.as_str());
                        graph.removed.insert(key.clone());
                    }
                }
            }
        }
        for (key, value) in sources {
            let proof = classify(value);
            let completeness = if input
                .update
                .as_ref()
                .is_some_and(|update| !update.operations.contains_key(key))
            {
                Completeness::Unsent
            } else {
                Completeness::Replacement
            };
            let id = graph.push(
                ClassifiedRef::from(&proof),
                DiagnosticScope::Public,
                false,
                completeness,
                input
                    .update
                    .as_ref()
                    .map_or_else(IgnoreCursor::default, |update| update.ignored.root(key)),
            );
            graph.roots.insert(key.to_owned(), id);
        }
        if input.accepts_defaults
            && !input
                .update
                .as_ref()
                .is_some_and(|update| update.operations.contains_key("tags"))
        {
            graph.default_nodes(input);
        }
        graph
    }

    fn push(
        &mut self,
        classified: ClassifiedRef<
            '_,
            'a,
            ParserPlaceholder,
            crate::resource::AbsoluteBindingAddress,
        >,
        scope: DiagnosticScope,
        enclosing_failure: bool,
        completeness: Completeness,
        ignore: IgnoreCursor<'_>,
    ) -> NodeId {
        let (proof, protection) = classified.for_input_rules(&InputEvidenceAuthority(()));
        let scope = if scope.is_secret() { scope } else { protection };
        let blocked = ignore.blocked;
        let source = ClassifiedRef::from(&proof).diagnostic_value();
        let correspondence = if matches!(
            peel_secret(source),
            Value::Deferred(DeferredValue::Unknown(_))
        ) {
            ExpressionCorrespondence::Erased
        } else {
            ExpressionCorrespondence::RetainedCurrent
        };
        let child_failure = enclosing_failure || proof.has_computation_failure();
        let mut full_proof = !blocked;
        let children = if let Some(fields) = ClassifiedRef::from(&proof).map() {
            let mut members = IndexMap::new();
            for (key, value) in fields {
                let child = self.push(value, scope, child_failure, completeness, ignore.field(key));
                full_proof &= self.nodes[child.0].full_proof;
                members.insert(key, child);
            }
            Children::Map {
                members,
                complete: true,
                tail: false,
            }
        } else if let Some(count) = ClassifiedRef::from(&proof).list_len() {
            let mut items = Vec::with_capacity(count);
            for index in 0..count {
                let value = ClassifiedRef::from(&proof)
                    .list_item(index)
                    .expect("known list membership");
                let child = self.push(
                    value,
                    scope,
                    child_failure,
                    completeness,
                    ignore.item(index),
                );
                full_proof &= self.nodes[child.0].full_proof;
                items.push(child);
            }
            Children::List(items)
        } else {
            Children::Scalar
        };
        let id = NodeId(self.nodes.len());
        self.nodes.push(EvidenceNode {
            proof: Some(proof),
            derived_failure: false,
            children,
            scope,
            enclosing_failure,
            blocked,
            masked_children: ignore.masked_children(),
            membership: Membership::Present,
            completeness,
            correspondence,
            full_proof,
        });
        id
    }

    fn default_nodes(&mut self, input: &'a InputStorage) {
        for config in &input.declarations.defaults {
            let route = crate::provider::ProviderInstanceKey::from(config);
            if route.provider() != input.id.provider
                || route.provider_instance() != input.id.provider_instance.as_deref()
            {
                continue;
            }
            let ignore = input
                .update
                .as_ref()
                .map_or_else(IgnoreCursor::default, |update| update.ignored.root("tags"));
            for (key, value) in &config.default_tags {
                let proof = classify(value);
                let encodable = crate::provider::assert_default_tag_value_encodable(&proof).is_ok();
                let membership = if encodable {
                    Membership::Present
                } else {
                    Membership::Possible
                };
                let root = if let Some(&id) = self.roots.get("tags") {
                    id
                } else {
                    let id = NodeId(self.nodes.len());
                    self.nodes.push(EvidenceNode {
                        proof: None,
                        derived_failure: false,
                        children: Children::Map {
                            members: IndexMap::new(),
                            complete: true,
                            tail: false,
                        },
                        scope: DiagnosticScope::Public,
                        enclosing_failure: false,
                        blocked: ignore.blocked,
                        masked_children: ignore.masked_children(),
                        membership,
                        completeness: if input.update.is_some() {
                            Completeness::Unsent
                        } else {
                            Completeness::Replacement
                        },
                        correspondence: ExpressionCorrespondence::Erased,
                        full_proof: false,
                    });
                    self.roots.insert("tags".into(), id);
                    id
                };
                if let Children::Map { members, .. } = &self.nodes[root.0].children {
                    if members.contains_key(key.as_str()) {
                        continue;
                    }
                } else {
                    self.nodes[root.0].children = Children::Map {
                        members: IndexMap::new(),
                        complete: false,
                        tail: true,
                    };
                }
                let child = self.push(
                    ClassifiedRef::from(&proof),
                    self.nodes[root.0].scope,
                    false,
                    self.nodes[root.0].completeness,
                    ignore.field(key),
                );
                self.nodes[child.0].membership = membership;
                self.nodes[root.0].derived_failure |= self.nodes[child.0]
                    .proof
                    .as_ref()
                    .is_some_and(Classification::has_failure);
                if matches!(membership, Membership::Present) {
                    self.nodes[root.0].membership = Membership::Present;
                }
                self.nodes[root.0].full_proof = false;
                if let Children::Map { members, .. } = &mut self.nodes[root.0].children {
                    members.insert(key, child);
                }
            }
        }
    }
}

fn classify(value: &InputValue) -> Classification<'_, ParserPlaceholder> {
    crate::resource::knowledge::collect_classified_attributes(
        std::iter::once((String::new(), value)),
        &KnowledgeContext::validation(),
        |_, _| {},
    )
    .remove("")
    .expect("one input")
}

fn accepts_defaults(schema: &ValidatedResourceSchema) -> bool {
    schema
        .as_schema()
        .attributes
        .get("tags")
        .is_some_and(|attribute| {
            match attribute
                .attr_type
                .shape_with_defs(schema.as_schema().defs())
            {
                crate::schema::Shape::Map { key, value, .. } => {
                    matches!(
                        key.shape_with_defs(schema.as_schema().defs()),
                        crate::schema::Shape::String { .. }
                    ) && matches!(
                        value.shape_with_defs(schema.as_schema().defs()),
                        crate::schema::Shape::String { .. }
                    )
                }
                _ => false,
            }
        })
}

#[cfg(test)]
thread_local! {
    static IGNORE_PATH_WORK: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn record_ignore_path_work(segments: usize) {
    IGNORE_PATH_WORK.set(IGNORE_PATH_WORK.get().map(|work| work + segments));
}

#[cfg(test)]
pub(super) fn with_ignore_path_work<R>(operation: impl FnOnce() -> R) -> (R, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            IGNORE_PATH_WORK.set(None);
        }
    }
    IGNORE_PATH_WORK.set(Some(0));
    let _reset = Reset;
    let result = operation();
    (result, IGNORE_PATH_WORK.get().unwrap())
}

impl<'a> NodeRef<'a> {
    fn raw(self) -> &'a EvidenceNode<'a> {
        &self.input.graph().nodes[self.id.0]
    }
    pub(super) fn input_read(self) -> InputRead<'a> {
        self.input.finish(self)
    }
    pub(super) fn instance(self) -> InstanceRef {
        InstanceRef::Node(self.id)
    }
    pub(super) fn scope(self) -> DiagnosticScope {
        self.raw().scope
    }
    pub(super) fn failed(self) -> bool {
        self.raw().enclosing_failure
            || self.raw().derived_failure
            || self
                .raw()
                .proof
                .as_ref()
                .is_some_and(Classification::has_failure)
    }
    pub(super) fn computation_failed(self) -> bool {
        self.raw().enclosing_failure
            || self
                .raw()
                .proof
                .as_ref()
                .is_some_and(Classification::has_computation_failure)
    }
    pub(super) fn known(self) -> bool {
        self.raw().full_proof && matches!(self.raw().proof, Some(Classification::Known(_)))
    }
    pub(super) fn shape_known(self) -> bool {
        !self.raw().blocked
            && match &self.raw().children {
                Children::Map { complete, .. } => *complete,
                Children::List(_) => true,
                Children::Scalar => matches!(
                    self.raw().proof,
                    Some(Classification::Known(_) | Classification::ShapeKnown(_))
                ),
            }
    }
    pub(super) fn proof(self) -> Option<&'a Classification<'a, ParserPlaceholder>> {
        self.raw().full_proof.then_some(self.raw().proof.as_ref()?)
    }
    pub(super) fn source(self) -> Option<&'a InputValue> {
        self.input
            .expression(self)
            .map(|expression| expression.value)
    }
    pub(super) fn expression(self) -> Option<BoundExpression<'a>> {
        self.input.expression(self)
    }
    pub(super) fn field(self, name: &str) -> InputRead<'a> {
        self.input.read(
            self.instance(),
            &InputPath::new(name).expect("admitted canonical field"),
        )
    }
    pub(super) fn list_len(self) -> Option<usize> {
        match &self.raw().children {
            Children::List(items) if !self.raw().blocked => Some(items.len()),
            _ => None,
        }
    }
    pub(super) fn index(self, index: usize) -> Option<Self> {
        match &self.raw().children {
            Children::List(items) if !self.raw().blocked => items.get(index).map(|&id| Self {
                input: self.input,
                id,
            }),
            _ => None,
        }
    }
    pub(super) fn fields(self) -> Option<Vec<(&'a str, Self)>> {
        match &self.raw().children {
            Children::Map { members, .. } if !self.raw().blocked => Some(
                members
                    .iter()
                    .map(|(key, &id)| {
                        (
                            *key,
                            Self {
                                input: self.input,
                                id,
                            },
                        )
                    })
                    .collect(),
            ),
            _ => None,
        }
    }
    pub(super) fn keys(self) -> MapKeyCursor<'a> {
        let entries = self
            .fields()
            .unwrap_or_default()
            .into_iter()
            .map(|(key, node)| {
                (
                    key,
                    if node.raw().blocked {
                        Membership::Possible
                    } else {
                        node.raw().membership
                    },
                )
            })
            .collect();
        let remainder = match self.raw().completeness {
            Completeness::Unsent => KeyRemainder::Withheld,
            Completeness::Replacement => match &self.raw().children {
                Children::Map {
                    complete: true,
                    tail: false,
                    ..
                } => KeyRemainder::Complete,
                Children::Map { .. } | Children::Scalar
                    if !self.raw().masked_children.is_empty() || self.raw().blocked =>
                {
                    KeyRemainder::Withheld
                }
                Children::Map { .. } | Children::Scalar | Children::List(_) => self
                    .expression()
                    .map_or(KeyRemainder::Withheld, |expression| {
                        KeyRemainder::Expression(BoundKeyExpression(expression))
                    }),
            },
        };
        MapKeyCursor { entries, remainder }
    }
    pub(super) fn unselected(self) -> InstanceRef {
        InstanceRef::Deferred {
            node: Some(self.id),
            failed: self.failed(),
            scope: self.scope(),
        }
    }
}

impl InputRead<'_> {
    pub(super) fn instance(&self) -> InstanceRef {
        match self {
            Self::Present(node) => node.instance(),
            Self::Failed(failure) => failure.node.instance(),
            Self::Withheld(value) => InstanceRef::Deferred {
                node: value.node.map(|node| node.id),
                failed: false,
                scope: value.node.map_or(DiagnosticScope::Public, NodeRef::scope),
            },
            Self::Absent(_) => unreachable!("absence has no instance"),
        }
    }
}

impl<'a> Withheld<'a> {
    pub(super) fn possible_node(&self) -> Option<NodeRef<'a>> {
        matches!(self.reason, WithheldReason::UnknownMembership)
            .then_some(self.node)
            .flatten()
    }
}

impl InstanceRef {
    pub(super) fn node(self, input: &RuleInput) -> Option<NodeRef<'_>> {
        match self {
            Self::Node(id) => Some(NodeRef { input, id }),
            Self::Resource | Self::Deferred { .. } => None,
        }
    }
    pub(super) fn deferred(self, input: &RuleInput) -> Self {
        match self {
            Self::Node(id) => NodeRef { input, id }.unselected(),
            Self::Deferred { .. } => self,
            Self::Resource => Self::Deferred {
                node: None,
                failed: false,
                scope: DiagnosticScope::Public,
            },
        }
    }
    pub(super) fn scope(self, input: &RuleInput) -> DiagnosticScope {
        match self {
            Self::Node(id) => NodeRef { input, id }.scope(),
            Self::Deferred { scope, .. } => scope,
            Self::Resource => DiagnosticScope::Public,
        }
    }
}

/// A key cursor never borrows proof from the payload's raw expression. Only
/// replacement membership can grant an expression handle for its unknown tail.
pub(super) struct MapKeyCursor<'a> {
    pub entries: Vec<(&'a str, Membership)>,
    pub remainder: KeyRemainder<'a>,
}
pub(super) enum KeyRemainder<'a> {
    Complete,
    Withheld,
    Expression(BoundKeyExpression<'a>),
}

/// Only the key cursor can mint this capability. A scalar expression handle
/// cannot be interpreted as proof about map-key membership by the estimator.
pub(super) struct BoundKeyExpression<'a>(BoundExpression<'a>);
pub(super) enum EstimatorInput<'a> {
    Value(BoundExpression<'a>),
    Keys(BoundKeyExpression<'a>),
}
impl<'a> EstimatorInput<'a> {
    pub(super) fn into_parts(self) -> (BoundExpression<'a>, bool) {
        match self {
            Self::Value(expression) => (expression, false),
            Self::Keys(expression) => (expression.0, true),
        }
    }
}
