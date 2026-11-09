//! Retain the ordinary validator's complete selected tree for this value cohort.
use super::*;
use crate::schema::{Fit, FitKind};
use declarations::DeclarationNode;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SelectionNode {
    declaration: DeclarationNode,
    evidence: crate::resource::knowledge::KnownNodeId,
}

impl SelectionNode {
    fn of(ty: &CompiledAttributeType, value: &NodeRef<'_>) -> Option<Self> {
        Some(Self {
            declaration: DeclarationNode::of(ty),
            evidence: value.proof()?.known_node_id()?,
        })
    }
}

#[derive(Default)]
pub(super) struct UnionFits<'a> {
    selections: HashMap<SelectionNode, Option<&'a CompiledAttributeType>>,
}

impl<'a> UnionFits<'a> {
    pub(super) fn select(
        &mut self,
        ty: &'a CompiledAttributeType,
        value: &NodeRef<'_>,
        defs: &'a BTreeMap<String, CompiledAttributeType>,
        visited: &mut Vec<&'a str>,
    ) -> Option<&'a CompiledAttributeType> {
        let node = SelectionNode::of(ty, value)?;
        if let Some(selected) = self.selections.get(&node) {
            return *selected;
        }
        let crate::schema::RawShape::Union(members) = ty.raw_shape() else {
            unreachable!("union declaration");
        };
        let selected = crate::schema::value_validation::select_input_union_fit_on_path(
            members,
            value.proof()?,
            defs,
            visited,
        );
        self.selections
            .insert(node, selected.as_ref().map(|selected| selected.member));
        if let Some(selected) = selected {
            self.retain(value, &selected.fit);
            Some(selected.member)
        } else {
            None
        }
    }

    fn retain(&mut self, value: &NodeRef<'_>, fit: &Fit<'a>) {
        match &fit.kind {
            FitKind::Union {
                member,
                fit: selected,
            } => {
                if let Some(node) = SelectionNode::of(fit.attr.as_attr(), value) {
                    self.selections.insert(node, Some(member));
                }
                self.retain(value, selected);
            }
            FitKind::Struct(fits) => {
                if let Some(fields) = value.fields() {
                    for ((_, value), fit) in fields.into_iter().zip(fits) {
                        self.retain(&value, fit);
                    }
                }
            }
            FitKind::Map(fits) => {
                if let Some(fields) = value.fields() {
                    for ((_, value), fit) in fields.into_iter().zip(fits) {
                        self.retain(&value, &fit.value);
                    }
                }
            }
            FitKind::List(fits) | FitKind::StringList(fits) => {
                for (index, fit) in fits.iter().enumerate() {
                    if let Some(value) = value.index(index) {
                        self.retain(&value, fit);
                    }
                }
            }
            // Secret evidence is already opened by the named, protected
            // input-rule computation, while its diagnostic scope is retained.
            FitKind::Secret(fit) => self.retain(value, fit),
            FitKind::Coalesce { .. }
            | FitKind::PassThrough
            | FitKind::Deferred(_)
            | FitKind::Failed
            | FitKind::HistoricalOpaque
            | FitKind::Enum { .. } => {}
        }
    }
}
