//! Shared, immutable knowledge and membership for one write-site cohort.
use super::*;
use crate::resource::{
    DiagnosticScope,
    knowledge::{Classification, ClassifiedRef, KnowledgeContext},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Membership {
    Present,
    Possible,
}

#[derive(Clone, Copy)]
enum Omissions {
    Absent,
    Withheld,
}

pub(super) enum FieldEvidence<'s> {
    Member(InputEvidence<'s>),
    Absent,
    Withheld,
    Pending,
}

#[derive(Debug, Clone)]
pub(super) struct MapMember {
    pub key: String,
    pub membership: Membership,
}

#[derive(Clone)]
enum EvidenceValue<'s> {
    Classified(Classification<'s, ParserPlaceholder>),
    DefaultMap {
        actual: Option<Classification<'s, ParserPlaceholder>>,
        members: &'s [MapMember],
    },
    // An enclosing failure is independent of instance/branch membership.
    Unselected,
}

#[derive(Clone)]
pub(super) struct InputEvidence<'s> {
    value: EvidenceValue<'s>,
    scope: DiagnosticScope,
    enclosing_failure: bool,
    omissions: Omissions,
}

pub(super) struct SiteEvidence<'s> {
    attributes: Option<HashMap<String, InputEvidence<'s>>>,
    update: Option<&'s UpdateInputContext>,
}

#[derive(Clone)]
pub(super) enum RuleEvidence<'s> {
    Resource(Arc<SiteEvidence<'s>>),
    Instance(Option<InputEvidence<'s>>),
}

impl<'s> SiteEvidence<'s> {
    pub(super) fn new(site: &'s WriteSite<'_>) -> Self {
        let mut attributes: Option<HashMap<_, _>> = site.attributes.as_deref().map(|attributes| {
            crate::resource::knowledge::collect_classified_attributes(
                attributes.iter().map(|(key, value)| (key.clone(), value)),
                &KnowledgeContext::validation(),
                |_, _| {},
            )
            .into_iter()
            .map(|(key, proof)| {
                let mut value =
                    InputEvidence::from_ref(ClassifiedRef::from(&proof), DiagnosticScope::Public);
                if site
                    .update
                    .as_ref()
                    .is_some_and(|update| !update.changed.contains(&key))
                {
                    value.omissions = Omissions::Withheld;
                }
                (key, value)
            })
            .collect()
        });
        let members = site.default_members.or_else(|| {
            site.types
                .and_then(|types| types.default_keys.get(site.resource_id))
                .map(Vec::as_slice)
        });
        if let (Some(attributes), Some(members)) =
            (&mut attributes, members.filter(|m| !m.is_empty()))
            && !site
                .update
                .as_ref()
                .is_some_and(|update| update.removed.contains("tags"))
        {
            let actual = attributes.remove("tags");
            let (proof, scope, enclosing_failure) = match actual {
                Some(value) => (value.proof().cloned(), value.scope, value.enclosing_failure),
                None => (None, DiagnosticScope::Public, false),
            };
            attributes.insert(
                "tags".into(),
                InputEvidence {
                    value: EvidenceValue::DefaultMap {
                        actual: proof,
                        members,
                    },
                    scope,
                    enclosing_failure,
                    omissions: if site
                        .update
                        .as_ref()
                        .is_some_and(|update| !update.changed.contains("tags"))
                    {
                        Omissions::Withheld
                    } else {
                        Omissions::Absent
                    },
                },
            );
        }
        Self {
            attributes,
            update: site.update.as_ref(),
        }
    }
    pub(super) fn field(&self, name: &str) -> FieldEvidence<'s> {
        let Some(attributes) = &self.attributes else {
            return FieldEvidence::Pending;
        };
        if let Some(value) = attributes.get(name) {
            return FieldEvidence::Member(value.clone());
        }
        if self
            .update
            .is_some_and(|update| !update.removed.contains(name))
        {
            FieldEvidence::Withheld
        } else {
            FieldEvidence::Absent
        }
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
            value: EvidenceValue::Classified(proof),
            scope: if scope.is_secret() { scope } else { protection },
            enclosing_failure: false,
            omissions: Omissions::Absent,
        }
    }
    fn child(
        &self,
        proof: ClassifiedRef<'_, 's, ParserPlaceholder, crate::resource::AbsoluteBindingAddress>,
    ) -> Self {
        let mut child = Self::from_ref(proof, self.scope);
        child.enclosing_failure = self.computation_failed();
        child.omissions = self.omissions;
        child
    }
    pub(super) fn unselected(&self) -> Self {
        Self {
            value: EvidenceValue::Unselected,
            scope: self.scope,
            enclosing_failure: self.failed(),
            omissions: Omissions::Withheld,
        }
    }
    pub(super) fn unknown() -> Self {
        Self {
            value: EvidenceValue::Unselected,
            scope: DiagnosticScope::Public,
            enclosing_failure: false,
            omissions: Omissions::Withheld,
        }
    }
    pub(super) fn source(&self) -> Option<&'s InputValue> {
        self.proof()
            .map(|proof| ClassifiedRef::from(proof).diagnostic_value())
    }
    pub(super) fn proof(&self) -> Option<&Classification<'s, ParserPlaceholder>> {
        match &self.value {
            EvidenceValue::Classified(proof) => Some(proof),
            EvidenceValue::DefaultMap { actual, .. } => actual.as_ref(),
            EvidenceValue::Unselected => None,
        }
    }
    pub(super) fn membership(&self) -> Membership {
        match &self.value {
            EvidenceValue::Classified(_) => Membership::Present,
            EvidenceValue::DefaultMap { actual, members } => {
                if actual.is_some()
                    || members
                        .iter()
                        .any(|member| member.membership == Membership::Present)
                {
                    Membership::Present
                } else {
                    Membership::Possible
                }
            }
            EvidenceValue::Unselected => Membership::Possible,
        }
    }
    pub(super) fn additional_members(&self) -> &[MapMember] {
        match &self.value {
            EvidenceValue::DefaultMap { members, .. } => members,
            EvidenceValue::Classified(_) | EvidenceValue::Unselected => &[],
        }
    }
    pub(super) fn scope(&self) -> DiagnosticScope {
        self.scope
    }
    pub(super) fn failed(&self) -> bool {
        self.enclosing_failure || self.proof().is_some_and(Classification::has_failure)
    }
    pub(super) fn known(&self) -> bool {
        matches!(self.proof(), Some(Classification::Known(_)))
    }
    pub(super) fn shape_known(&self) -> bool {
        matches!(
            self.proof(),
            Some(Classification::Known(_) | Classification::ShapeKnown(_))
        )
    }
    pub(super) fn map_known(&self) -> bool {
        self.proof()
            .and_then(|proof| ClassifiedRef::from(proof).map_len())
            .is_some()
    }
    pub(super) fn list_len(&self) -> Option<usize> {
        ClassifiedRef::from(self.proof()?).list_len()
    }
    pub(super) fn field(&self, name: &str) -> FieldEvidence<'s> {
        let Some(proof) = self.proof() else {
            return FieldEvidence::Pending;
        };
        if let Some(child) = ClassifiedRef::from(proof).map_field(name) {
            return FieldEvidence::Member(self.child(child));
        }
        if !self.map_known() {
            return FieldEvidence::Pending;
        }
        match self.omissions {
            Omissions::Absent => FieldEvidence::Absent,
            Omissions::Withheld => FieldEvidence::Withheld,
        }
    }
    pub(super) fn index(&self, index: usize) -> Option<Self> {
        ClassifiedRef::from(self.proof()?)
            .list_item(index)
            .map(|child| self.child(child))
    }
    pub(super) fn fields(&self) -> Option<Vec<(&'s str, Self)>> {
        ClassifiedRef::from(self.proof()?).map().map(|fields| {
            fields
                .into_iter()
                .map(|(key, child)| (key, self.child(child)))
                .collect()
        })
    }
    pub(super) fn computation_failed(&self) -> bool {
        self.enclosing_failure
            || self
                .proof()
                .is_some_and(Classification::has_computation_failure)
    }
}
