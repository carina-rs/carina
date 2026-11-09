//! Immutable rule reachability for one admitted declaration graph.
use super::*;
use crate::schema::{AttrTypeKind, CompiledResourceSchema};
use std::collections::VecDeque;

/// Node identity belongs to the immutable, shared compiled schema allocation.
/// It never depends on provider names or user values and is never persisted.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct DeclarationNode(usize);

impl DeclarationNode {
    pub(super) fn of(ty: &CompiledAttributeType) -> Self {
        Self(std::ptr::from_ref(ty) as usize)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Scopes {
    create_only: bool,
    every_write: bool,
}

impl Scopes {
    fn rules<'a>(rules: impl IntoIterator<Item = &'a InputRule<Compiled>>) -> Self {
        let mut scopes = Self::default();
        for rule in rules {
            match rule.scope() {
                InputRuleScope::CreateOnly => scopes.create_only = true,
                InputRuleScope::EveryWrite => scopes.every_write = true,
            }
        }
        scopes
    }
    fn extend(&mut self, other: Self) -> bool {
        let previous = *self;
        self.create_only |= other.create_only;
        self.every_write |= other.every_write;
        *self != previous
    }
    fn applicable(self, context: InputWriteContext) -> bool {
        (self.create_only && scope_classification(InputRuleScope::CreateOnly, context).is_some())
            || (self.every_write
                && scope_classification(InputRuleScope::EveryWrite, context).is_some())
    }
}

pub(crate) struct InputRuleDeclarations {
    nodes: HashMap<DeclarationNode, Scopes>,
    resource: Scopes,
}

impl InputRuleDeclarations {
    pub(crate) fn new(schema: &CompiledResourceSchema) -> Self {
        let mut nodes = HashMap::new();
        let mut parents: HashMap<DeclarationNode, Vec<DeclarationNode>> = HashMap::new();
        let mut pending = schema
            .attributes
            .values()
            .map(|a| &a.attr_type)
            .chain(schema.defs().values())
            .collect::<Vec<_>>();
        while let Some(ty) = pending.pop() {
            let node = DeclarationNode::of(ty);
            if nodes.contains_key(&node) {
                continue;
            }
            #[cfg(test)]
            instances::schema_visit();
            let mut children = Vec::new();
            let direct = match ty.kind() {
                AttrTypeKind::Struct {
                    fields,
                    input_rules,
                    ..
                } => {
                    children.extend(fields.iter().map(|field| &field.field_type));
                    Scopes::rules(input_rules)
                }
                AttrTypeKind::List { element_type, .. } => {
                    children.push(element_type.as_ref());
                    Scopes::default()
                }
                AttrTypeKind::Map { value, .. } => {
                    children.push(value.as_ref());
                    Scopes::default()
                }
                AttrTypeKind::Union(_) => {
                    let crate::schema::RawShape::Union(members) = ty.raw_shape() else {
                        unreachable!("union declaration");
                    };
                    children.extend(members);
                    Scopes::default()
                }
                AttrTypeKind::Ref(name) => {
                    children.extend(schema.defs().get(name));
                    Scopes::default()
                }
                AttrTypeKind::UnloadedSchemaType(_)
                | AttrTypeKind::ResourceHandle { .. }
                | AttrTypeKind::String { .. }
                | AttrTypeKind::Reference { .. }
                | AttrTypeKind::StringLiteralSet { .. }
                | AttrTypeKind::Int { .. }
                | AttrTypeKind::Float { .. }
                | AttrTypeKind::Bool
                | AttrTypeKind::Duration
                | AttrTypeKind::Enum { .. } => Scopes::default(),
            };
            nodes.insert(node, direct);
            for child in children {
                parents
                    .entry(DeclarationNode::of(child))
                    .or_default()
                    .push(node);
                pending.push(child);
            }
        }
        // Each of the two scope facts can reach each parent only once. This
        // handles recursive defs without repeated descendant scans or a DFS
        // whose provisional cycle result could hide reachable declarations.
        let mut work = nodes
            .iter()
            .filter_map(|(&node, &scopes)| (scopes != Scopes::default()).then_some(node))
            .collect::<VecDeque<_>>();
        while let Some(child) = work.pop_front() {
            let scopes = nodes[&child];
            for parent in parents.get(&child).into_iter().flatten() {
                if nodes
                    .get_mut(parent)
                    .expect("recorded declaration")
                    .extend(scopes)
                {
                    work.push_back(*parent);
                }
            }
        }
        let mut resource = Scopes::rules(schema.input_rules());
        for attribute in schema.attributes.values() {
            resource.extend(nodes[&DeclarationNode::of(&attribute.attr_type)]);
        }
        Self { nodes, resource }
    }
    pub(super) fn reachable(&self, ty: &CompiledAttributeType, context: InputWriteContext) -> bool {
        self.nodes
            .get(&DeclarationNode::of(ty))
            .is_some_and(|scopes| scopes.applicable(context))
    }
    pub(super) fn has_declarations(&self, context: InputWriteContext) -> bool {
        self.resource.applicable(context)
    }
}
