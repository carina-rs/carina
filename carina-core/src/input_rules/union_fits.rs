//! Stage admission seals ordinary, provider-aware union fits before evaluation.
use super::*;
use crate::resource::knowledge::Classification;
use crate::schema::{Fit, FitKind, ValidatedCustomTypeLookup};
use declarations::DeclarationNode;
use std::sync::atomic::{AtomicBool, Ordering};

/// Catalog authority is captured only by a parser/admission stage owner. It
/// carries the shared resolver, never environment values or a transport map.
#[derive(Clone, Default)]
pub(crate) struct InputSelectionAuthority {
    catalog: Option<crate::parser::ProviderCustomTypes>,
}
impl std::fmt::Debug for InputSelectionAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InputSelectionAuthority(<stage catalog>)")
    }
}
impl InputSelectionAuthority {
    pub(crate) fn in_catalog(catalog: &crate::parser::ProviderContext) -> Self {
        Self {
            catalog: Some(catalog.custom_types.clone()),
        }
    }
    pub(crate) fn admit<
        'a,
        'v,
        P: crate::resource::PlaceholderKind,
        B: crate::resource::BindingAddress,
        T: crate::resource::knowledge::StructuralPending<'v, P, B>,
    >(
        &self,
        members: &'a [CompiledAttributeType],
        value: &Classification<'v, P, B, T>,
        defs: &'a BTreeMap<String, CompiledAttributeType>,
        owner: &str,
        visited: &mut Vec<&'a str>,
    ) -> InputUnionAdmission<'a> {
        let lookup = SelectionLookup {
            authority: self,
            owner,
            unavailable: AtomicBool::new(false),
        };
        let selected = crate::schema::value_validation::select_input_union_fit_on_path(
            members, value, defs, visited, &lookup,
        );
        if lookup.unavailable.load(Ordering::Relaxed) {
            InputUnionAdmission::Withheld
        } else {
            selected.map_or(InputUnionAdmission::Rejected, InputUnionAdmission::Selected)
        }
    }
}

pub(crate) enum InputUnionAdmission<'a> {
    Selected(crate::schema::value_validation::SelectedInputUnion<'a>),
    Withheld,
    Rejected,
}

struct SelectionLookup<'a> {
    authority: &'a InputSelectionAuthority,
    owner: &'a str,
    unavailable: AtomicBool,
}
impl ValidatedCustomTypeLookup for SelectionLookup<'_> {
    fn validate(
        &self,
        proof: crate::schema::LocallyValidatedCustomType<'_>,
    ) -> Result<(), crate::schema::TypeError> {
        use crate::parser::{CustomTypeResolver, CustomTypeValidationError};
        let result = match &self.authority.catalog {
            Some(catalog) => catalog.validate_validated(self.owner, proof),
            None => Err(CustomTypeValidationError::ProvidersNotLoaded),
        };
        if matches!(&result, Err(CustomTypeValidationError::ProvidersNotLoaded))
            || matches!(&result, Err(CustomTypeValidationError::ProviderRejected { error, .. })
                if matches!(error.detail().host_cause(), Some(crate::provider::HostErrorCause::ValidationSuspended(_))))
        {
            self.unavailable.store(true, Ordering::Relaxed);
        }
        result.map_err(Into::into)
    }
    fn canonicalization_error(
        &self,
        declaration: crate::parser::IdentifiedCustomTypeDeclaration<'_>,
        error: crate::schema::TypeError,
    ) -> crate::schema::TypeError {
        match &self.authority.catalog {
            Some(catalog) => crate::parser::custom_type_lookup_for_owner(catalog, self.owner)
                .canonicalization_error(declaration, error),
            None => error,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SelectionNode {
    declaration: DeclarationNode,
    evidence: super::rule_input::NodeId,
}
#[derive(Clone, Copy)]
pub(super) enum UnionSelection {
    Selected(usize),
    Withheld,
    Rejected,
}

#[derive(Default)]
pub(super) struct UnionSelections {
    selections: HashMap<SelectionNode, UnionSelection>,
}
impl UnionSelections {
    pub(super) fn selection(
        &self,
        ty: &CompiledAttributeType,
        node: super::rule_input::NodeId,
    ) -> UnionSelection {
        self.selections
            .get(&SelectionNode {
                declaration: DeclarationNode::of(ty),
                evidence: node,
            })
            .copied()
            .unwrap_or(UnionSelection::Withheld)
    }
    pub(super) fn admitted<'a>(
        &mut self,
        ty: &'a CompiledAttributeType,
        value: super::rule_input::NodeId,
        graph: &super::rule_input::EvidenceGraph<'_>,
        defs: &'a BTreeMap<String, CompiledAttributeType>,
        authority: &InputSelectionAuthority,
        owner: &str,
    ) -> Option<Fit<'a>> {
        let node = SelectionNode {
            declaration: DeclarationNode::of(ty),
            evidence: value,
        };
        if self.selections.contains_key(&node) {
            return None;
        }
        let Some(proof @ Classification::Known(_)) = graph.proof(value) else {
            self.selections.insert(node, UnionSelection::Withheld);
            return None;
        };
        let crate::schema::RawShape::Union(members) = ty.raw_shape() else {
            unreachable!("union declaration")
        };
        let selected = match authority.admit(members, proof, defs, owner, &mut Vec::new()) {
            InputUnionAdmission::Selected(selected) => selected,
            InputUnionAdmission::Withheld => {
                self.selections.insert(node, UnionSelection::Withheld);
                return None;
            }
            InputUnionAdmission::Rejected => {
                self.selections.insert(node, UnionSelection::Rejected);
                return None;
            }
        };
        let index = members
            .iter()
            .position(|member| std::ptr::eq(member, selected.member))
            .expect("ordinary fit is a declared member");
        self.selections
            .insert(node, UnionSelection::Selected(index));
        self.retain(value, &selected.fit, graph);
        Some(selected.fit)
    }
    fn retain(
        &mut self,
        value: super::rule_input::NodeId,
        fit: &Fit<'_>,
        graph: &super::rule_input::EvidenceGraph<'_>,
    ) {
        match &fit.kind {
            FitKind::Union {
                member,
                fit: selected,
            } => {
                let crate::schema::RawShape::Union(members) = fit.attr.as_attr().raw_shape() else {
                    unreachable!("ordinary union fit")
                };
                let index = members
                    .iter()
                    .position(|candidate| std::ptr::eq(candidate, *member))
                    .expect("declared fit member");
                self.selections.insert(
                    SelectionNode {
                        declaration: DeclarationNode::of(fit.attr.as_attr()),
                        evidence: value,
                    },
                    UnionSelection::Selected(index),
                );
                self.retain(value, selected, graph);
            }
            FitKind::Struct(fits) => {
                for (value, fit) in graph.children(value).zip(fits) {
                    self.retain(value, fit, graph);
                }
            }
            FitKind::Map(fits) => {
                for (value, fit) in graph.children(value).zip(fits) {
                    self.retain(value, &fit.value, graph);
                }
            }
            FitKind::List(fits) | FitKind::StringList(fits) => {
                for (value, fit) in graph.children(value).zip(fits) {
                    self.retain(value, fit, graph);
                }
            }
            FitKind::Secret(fit) => self.retain(value, fit, graph),
            FitKind::Coalesce { .. }
            | FitKind::PassThrough
            | FitKind::Deferred(_)
            | FitKind::Failed
            | FitKind::HistoricalOpaque
            | FitKind::Enum { .. } => {}
        }
    }
}
