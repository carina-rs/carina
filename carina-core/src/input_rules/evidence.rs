//! Shared, immutable knowledge for one write-site evaluation cohort.
use super::*;
use crate::resource::{
    DiagnosticScope,
    knowledge::{Classification, ClassifiedRef, KnowledgeContext},
};

#[derive(Clone)]
pub(super) struct InputEvidence<'s> {
    proof: Classification<'s, ParserPlaceholder>,
    scope: DiagnosticScope,
    enclosing_failure: bool,
}

pub(super) struct SiteEvidence<'s> {
    attributes: Option<HashMap<String, InputEvidence<'s>>>,
}

#[derive(Clone)]
pub(super) enum RuleEvidence<'s> {
    Resource(Arc<SiteEvidence<'s>>),
    Instance(Option<InputEvidence<'s>>),
}

impl<'s> SiteEvidence<'s> {
    pub(super) fn new(site: &'s WriteSite<'_>) -> Self {
        let attributes = site.attributes.as_deref().map(|attributes| {
            crate::resource::knowledge::collect_classified_attributes(
                attributes.iter().map(|(key, value)| (key.clone(), value)),
                &KnowledgeContext::validation(),
                |_, _| {},
            )
            .into_iter()
            .map(|(key, proof)| {
                (
                    key,
                    InputEvidence::from_ref(ClassifiedRef::from(&proof), DiagnosticScope::Public),
                )
            })
            .collect()
        });
        Self { attributes }
    }
    pub(super) fn field(&self, name: &str) -> Option<InputEvidence<'s>> {
        self.attributes.as_ref()?.get(name).cloned()
    }
    pub(super) fn membership_known(&self) -> bool {
        self.attributes.is_some()
    }
}

impl<'s> InputEvidence<'s> {
    fn from_ref(
        proof: ClassifiedRef<'_, 's, ParserPlaceholder, crate::resource::AbsoluteBindingAddress>,
        scope: DiagnosticScope,
    ) -> Self {
        let (proof, protection) = proof.for_input_rules(&InputEvidenceAuthority(()));
        Self {
            proof,
            scope: if scope.is_secret() { scope } else { protection },
            enclosing_failure: false,
        }
    }
    fn child(
        &self,
        proof: ClassifiedRef<'_, 's, ParserPlaceholder, crate::resource::AbsoluteBindingAddress>,
    ) -> Self {
        let mut child = Self::from_ref(proof, self.scope);
        child.enclosing_failure = self.computation_failed();
        child
    }
    pub(super) fn source(&self) -> &'s InputValue {
        ClassifiedRef::from(&self.proof).diagnostic_value()
    }
    pub(super) fn proof(&self) -> &Classification<'s, ParserPlaceholder> {
        &self.proof
    }
    pub(super) fn scope(&self) -> DiagnosticScope {
        self.scope
    }
    pub(super) fn failed(&self) -> bool {
        self.enclosing_failure || self.proof.has_failure()
    }
    pub(super) fn known(&self) -> bool {
        matches!(self.proof, Classification::Known(_))
    }
    pub(super) fn shape_known(&self) -> bool {
        matches!(
            self.proof,
            Classification::Known(_) | Classification::ShapeKnown(_)
        )
    }
    pub(super) fn map_known(&self) -> bool {
        ClassifiedRef::from(&self.proof).map_len().is_some()
    }
    pub(super) fn list_len(&self) -> Option<usize> {
        ClassifiedRef::from(&self.proof).list_len()
    }
    pub(super) fn field(&self, name: &str) -> Option<Self> {
        ClassifiedRef::from(&self.proof)
            .map_field(name)
            .map(|child| self.child(child))
    }
    pub(super) fn index(&self, index: usize) -> Option<Self> {
        ClassifiedRef::from(&self.proof)
            .list_item(index)
            .map(|child| self.child(child))
    }
    pub(super) fn fields(&self) -> Option<Vec<(&'s str, Self)>> {
        ClassifiedRef::from(&self.proof).map().map(|fields| {
            fields
                .into_iter()
                .map(|(key, child)| (key, self.child(child)))
                .collect()
        })
    }
    // A literal container's failed sibling is not a failed enclosing
    // computation. Selection must precede aggregate payload failure checks.
    pub(super) fn computation_failed(&self) -> bool {
        self.enclosing_failure || self.proof.has_computation_failure()
    }
}
