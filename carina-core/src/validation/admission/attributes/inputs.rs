use super::*;
use crate::identifier::{CanonicalAttributeValues, CanonicalResource};
use crate::parser::retained::{
    JudgedExpr, JudgedSlot, MissingEvidence, ProjectionGeneration, ValidationProjection,
};
use crate::resource::ParserPlaceholder;
use crate::resource::knowledge::{
    Classification, ClassificationStage, KnowledgeContext, KnowledgeError,
    collect_classified_attributes,
};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;

/// Ordinary prechecks already reported by the caller for this source scope.
/// Without reported prechecks an omitted carrying input fails closed.
#[derive(Clone, Copy)]
pub enum SlotPrechecks<'a> {
    Reported(&'a crate::validation::BindingUseReport),
    Unreported,
}

#[cfg(test)]
mod classification_tests;
#[cfg(test)]
mod ordering_tests;

/// Keep supplied keys and values, taking matched slots' order from their owner.
/// Canonical aliases share the authored slot's occurrence and instance. Names
/// outside that inventory retain their producer's order and report-only handling.
pub(super) fn in_resource_order<'v, B: BindingAddress, D, V>(
    resource: &crate::parser::AnalysisResourceRef<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    values: &'v IndexMap<String, V>,
) -> std::vec::IntoIter<(&'v String, &'v V)>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let mut names_by_slot = HashMap::<_, Vec<_>>::new();
    for (index, name) in values.keys().enumerate() {
        if let Ok(slot) = resource.authored_slot(name) {
            names_by_slot
                .entry((slot.occurrence(), slot.instance()))
                .or_default()
                .push(index);
        }
    }
    let mut included = vec![false; values.len()];
    let mut ordered = Vec::with_capacity(values.len());
    let mut include = |index: usize| {
        if !included[index] {
            included[index] = true;
            ordered.push(values.get_index(index).expect("supplied attribute index"));
        }
    };
    for name in resource.attributes().keys() {
        if let Some(index) = values.get_index_of(name) {
            include(index);
        }
        if let Ok(slot) = resource.authored_slot(name)
            && let Some(aliases) = names_by_slot.get(&(slot.occurrence(), slot.instance()))
        {
            for index in aliases {
                include(*index);
            }
        }
    }
    for index in 0..values.len() {
        include(index);
    }
    ordered.into_iter()
}

/// Producer-owned attribute projections and their authored authority.
///
/// Admission consumes this product and classifies its stored projections.
pub struct ResourceAttributeInputs<B: BindingAddress> {
    awaiting: crate::identifier::PrefixObligations,
    owner: ResourceId,
    pub(super) declaration: Option<DeclarationIdentity>,
    precheck: Option<crate::parser::ResourcePrecheckIdentity>,
    pub(super) scope: crate::schema::value_validation::AssignmentScope,
    report: Option<Arc<crate::parser::retained::ExpressionReport<B>>>,
    inputs: IndexMap<String, PreparedAttribute<B>>,
    evidence: ResourceEvidence,
}

#[derive(Clone)]
enum ResourceEvidence {
    Slots,
    NoAuthoredEvidence(Arc<crate::parser::retained::NoAuthoredEvidence>),
}

pub(super) enum AttributeInput<'a, B: BindingAddress> {
    Authored { slot: &'a JudgedSlot<B> },
    Projected { phase: ValidationProjection },
    Reported { source: JudgedExpr },
}

/// Joining slots with values preserves an omitted value as an input, rather
/// than treating omission as successful admission.
#[derive(Clone)]
enum PreparedAttribute<B: BindingAddress, V = StoredInput<B>> {
    WithValue(V),
    WithoutValue {
        slot: Arc<JudgedSlot<B>>,
        precheck: SlotPrecheck,
    },
}
#[derive(Clone)]
enum SlotPrecheck {
    NoOrdinaryFinding,
    OrdinaryFinding,
}
impl<B: BindingAddress> PreparedAttribute<B> {
    fn value(&self) -> Option<&Value<ParserPlaceholder, B>> {
        match self {
            Self::WithValue(input) => Some(input.value()),
            Self::WithoutValue { .. } => None,
        }
    }
}

#[derive(Clone)]
enum StoredInput<B: BindingAddress> {
    Authored {
        slot: Arc<JudgedSlot<B>>,
        source_generation: ProjectionGeneration,
    },
    Projected {
        value: Arc<Value<ParserPlaceholder, B>>,
        phase: ValidationProjection,
    },
    // Inputs outside the slot inventory retain main's report-based checks.
    // This mode never grants authored or covered authority.
    Reported {
        value: Arc<Value<ParserPlaceholder, B>>,
        source: Result<JudgedExpr, MissingEvidence>,
    },
    Unavailable {
        value: Arc<Value<ParserPlaceholder, B>>,
        error: MissingEvidence,
        source: Result<JudgedExpr, MissingEvidence>,
    },
}

impl<B: BindingAddress> StoredInput<B> {
    fn value(&self) -> &Value<ParserPlaceholder, B> {
        match self {
            Self::Authored { slot, .. } => slot.projection().expect("prepared materialized slot"),
            Self::Projected { value, .. }
            | Self::Reported { value, .. }
            | Self::Unavailable { value, .. } => value,
        }
    }
}

impl<B: BindingAddress> ResourceAttributeInputs<B> {
    pub(super) fn awaiting_prefixes(&self) -> &crate::identifier::PrefixObligations {
        &self.awaiting
    }
    pub fn new<X, D>(owner: &CanonicalResource<'_, X, D, B>) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let mut result = Self::new_raw(owner);
        result.declaration = Some(super::declaration_identity(owner.resource_schema()));
        result.awaiting = owner.awaiting_prefixes().clone();
        result
    }
    pub fn for_resource<X, D>(
        owner: &CanonicalResource<'_, X, D, B>,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Result<Self, MissingEvidence>
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let mut result = Self::for_resource_raw(owner, report)?;
        result.declaration = Some(super::declaration_identity(owner.resource_schema()));
        result.awaiting = owner.awaiting_prefixes().clone();
        Ok(result)
    }
    pub fn from_resolved<X, D>(
        owner: &CanonicalResource<'_, X, D, B>,
        values: &CanonicalAttributeValues<ParserPlaceholder, B>,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Result<Self, MissingEvidence>
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let mut result = Self::from_resolved_raw(owner, values, report)?;
        result.declaration = Some(values.declaration.clone());
        result.awaiting = values.awaiting.clone();
        Ok(result)
    }
    pub fn from_report<X, D>(
        owner: &CanonicalResource<'_, X, D, B>,
        values: &CanonicalAttributeValues<ParserPlaceholder, B>,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let mut result = Self::from_report_raw(owner, values, report);
        result.declaration = Some(values.declaration.clone());
        result.awaiting = values.awaiting.clone();
        result
    }
    pub fn from_report_with_evidence<X, D>(
        owner: &CanonicalResource<'_, X, D, B>,
        values: &CanonicalAttributeValues<ParserPlaceholder, B>,
        report: &crate::parser::retained::ExpressionReport<B>,
        unchecked: crate::parser::retained::NoAuthoredEvidence,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let mut result = Self::from_report_with_evidence_raw(owner, values, report, unchecked);
        result.declaration = Some(values.declaration.clone());
        result.awaiting = values.awaiting.clone();
        result
    }

    #[cfg(test)]
    pub(crate) fn new_for_test<D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::new_raw(resource)
    }

    #[cfg(test)]
    pub(crate) fn for_resource_for_test<D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Result<Self, MissingEvidence>
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::for_resource_raw(resource, report)
    }

    #[cfg(test)]
    pub(crate) fn from_report_for_test<D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::from_report_raw(resource, values, report)
    }

    #[cfg(test)]
    pub(crate) fn from_resolved_for_test<D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Result<Self, MissingEvidence>
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::from_resolved_raw(resource, values, report)
    }

    fn new_raw<D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::prepare(resource, resource.attributes().iter(), false)
    }

    /// Require the resource report used by main's CLI/expanded admission path.
    /// Callers retain main's distinct missing-report outcome (error or fallback).
    fn for_resource_raw<D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Result<Self, MissingEvidence>
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let report = report.for_resource(resource.id())?;
        let mut inputs = Self::new_raw(resource);
        inputs.report = Some(Arc::new(report.clone()));
        Ok(inputs)
    }

    /// Main's buffer path when an authored owner/tree is not available.
    /// Values keep their classifications, and computed checks use this report.
    fn from_report_raw<D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::from_report_with_evidence_raw(
            resource,
            values,
            report,
            crate::parser::retained::NoAuthoredEvidence::for_resource(resource),
        )
    }

    /// Report-based checks keep their checked owner and resolved values.
    /// Unreached source identities are metadata, never a replacement owner.
    fn from_report_with_evidence_raw<D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
        report: &crate::parser::retained::ExpressionReport<B>,
        unchecked: crate::parser::retained::NoAuthoredEvidence,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let mut prepared = Self {
            owner: resource.id().clone(),
            declaration: None,
            awaiting: Default::default(),
            precheck: unchecked.precheck_identity(),
            scope: crate::schema::value_validation::AssignmentScope::Full,
            evidence: ResourceEvidence::NoAuthoredEvidence(Arc::new(unchecked)),
            report: Some(Arc::new(report.clone())),
            inputs: in_resource_order(resource, values)
                .map(|(name, value)| {
                    (
                        name.clone(),
                        PreparedAttribute::WithValue(StoredInput::Reported {
                            value: Arc::new(value.clone()),
                            source: source_judgement(resource, name),
                        }),
                    )
                })
                .collect(),
        };
        prepared.join_slots(resource);
        prepared
    }

    pub(super) fn report<'a>(
        &'a self,
        fallback: &'a crate::parser::retained::ExpressionReport<B>,
    ) -> &'a crate::parser::retained::ExpressionReport<B> {
        self.report
            .as_deref()
            .unwrap_or_else(|| fallback.for_resource(&self.owner).unwrap_or(fallback))
    }

    pub(super) fn is_reported(&self, name: &str) -> bool {
        matches!(
            self.inputs.get(name),
            Some(PreparedAttribute::WithValue(StoredInput::Reported { .. }))
        )
    }

    /// Resolved values are joined with every authored attribute slot. Evaluation
    /// owns materialization; a slot without a value remains an admission input.
    fn from_resolved_raw<D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Result<Self, MissingEvidence>
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let report = report.for_resource(resource.id())?;
        let mut inputs = Self::prepare(resource, in_resource_order(resource, values), true);
        inputs.report = Some(Arc::new(report.clone()));
        Ok(inputs)
    }

    fn prepare<'v, D>(
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        values: impl Iterator<Item = (&'v String, &'v Value<ParserPlaceholder, B>)>,
        materialize: bool,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
        B: 'v,
    {
        let authored = resource.attributes();
        // Resolved names may be canonical schema aliases. Verify the original
        // owner pairing by use identity before producing a new projection;
        // materialization cannot repair a stale or foreign source slot.
        let sources = materialize.then(|| {
            authored
                .iter()
                .filter_map(|(name, value)| {
                    resource
                        .authored_slot(name)
                        .ok()
                        .map(|slot| ((slot.occurrence(), slot.instance()), value))
                })
                .collect::<HashMap<_, _>>()
        });
        let inputs = values
            .map(|(name, value)| {
                let prepare = || {
                    if let Some(phase) = resource.projected_phase(name) {
                        return Ok(StoredInput::Projected {
                            value: Arc::new(value.clone()),
                            phase,
                        });
                    }
                    let slot = match resource.authored_slot(name) {
                        Ok(slot) => slot,
                        // No authored declaration exists for this resolved
                        // name. Main admitted it from the report; it is not a
                        // failed live correspondence or synthetic authority.
                        Err(_) => {
                            return Ok(StoredInput::Reported {
                                value: Arc::new(value.clone()),
                                source: source_judgement(resource, name),
                            });
                        }
                    };
                    let source = match &sources {
                        Some(sources) => {
                            sources.get(&(slot.occurrence(), slot.instance())).copied()
                        }
                        None => Some(value),
                    };
                    if slot.projection() != source {
                        return Err(MissingEvidence::new("resource slot/projection pairing"));
                    }
                    slot.cursor()?;
                    Ok(StoredInput::Authored {
                        slot: Arc::new(if materialize {
                            slot.materialized(value)
                        } else {
                            slot.clone()
                        }),
                        source_generation: slot.generation(),
                    })
                };
                (
                    name.clone(),
                    PreparedAttribute::WithValue(prepare().unwrap_or_else(|error| {
                        StoredInput::Unavailable {
                            value: Arc::new(value.clone()),
                            error,
                            source: source_judgement(resource, name),
                        }
                    })),
                )
            })
            .collect();
        let mut prepared = Self {
            owner: resource.id().clone(),
            declaration: None,
            awaiting: Default::default(),
            precheck: resource.precheck_identity(),
            scope: crate::schema::value_validation::AssignmentScope::Full,
            report: None,
            inputs,
            evidence: ResourceEvidence::Slots,
        };
        prepared.join_slots(resource);
        prepared
    }

    fn join_slots<D>(
        &mut self,
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
    ) where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        if let Some(slots) = resource.authored_slots() {
            join_slots(slots, &mut self.inputs);
        }
    }

    /// Carry actual slot precheck results, without inferring them from messages
    /// or an expectation that a later pass will report the error.
    pub fn with_prechecks(mut self, prechecks: SlotPrechecks<'_>) -> Self {
        for (name, input) in &mut self.inputs {
            match input {
                PreparedAttribute::WithValue(_) => {}
                PreparedAttribute::WithoutValue { precheck, .. } => {
                    *precheck = match prechecks {
                        SlotPrechecks::Reported(report)
                            if report.has_slot_finding(
                                &self.owner,
                                self.precheck.as_ref(),
                                name,
                            ) =>
                        {
                            SlotPrecheck::OrdinaryFinding
                        }
                        SlotPrechecks::Reported(_) | SlotPrechecks::Unreported => {
                            SlotPrecheck::NoOrdinaryFinding
                        }
                    };
                }
            }
        }
        self
    }

    pub(super) fn prepare_missing<Pass: ResourceAdmissionPass>(
        &self,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> MissingInputs {
        prepare_missing::<Pass, _, _>(self.inputs.iter(), report)
    }

    /// Input provenance, distinct from candidate sealing or successful admission.
    /// Report-only inputs never confer authored authority on their source slots.
    pub fn authored_evidence(&self) -> Result<(), &crate::parser::retained::NoAuthoredEvidence> {
        match &self.evidence {
            ResourceEvidence::Slots => Ok(()),
            ResourceEvidence::NoAuthoredEvidence(missing) => Err(missing),
        }
    }

    fn values(&self) -> impl Iterator<Item = (&str, &Value<ParserPlaceholder, B>)> {
        self.inputs
            .iter()
            .filter_map(|(name, input)| input.value().map(|value| (name.as_str(), value)))
    }

    fn value(&self, name: &str) -> Option<&Value<ParserPlaceholder, B>> {
        self.inputs.get(name).and_then(PreparedAttribute::value)
    }

    fn classify<'a, S: ClassificationStage>(
        &'a self,
        context: &KnowledgeContext<'_, ParserPlaceholder, B, S>,
        failed: impl FnMut(&str, KnowledgeError<B>),
    ) -> IndexMap<String, Classification<'a, ParserPlaceholder, B>> {
        collect_classified_attributes(
            self.values().map(|(name, value)| (name.to_owned(), value)),
            context,
            failed,
        )
    }

    /// Return a shared owner with the classifications borrowing its new values.
    /// Unchanged values keep their allocations and their existing proofs.
    fn materialize_and_classify<'a, S: ClassificationStage>(
        &'a mut self,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
        context: &KnowledgeContext<'_, ParserPlaceholder, B, S>,
        failed: impl FnMut(&str, KnowledgeError<B>),
    ) -> (
        &'a Self,
        IndexMap<String, Classification<'a, ParserPlaceholder, B>>,
    ) {
        // Additional resolved names have no authored authority. Preserve
        // main's classified/report path instead of inventing a seam finding.
        for (name, _) in self.unmatched_materializations(values) {
            self.inputs.insert(
                name.clone(),
                PreparedAttribute::WithValue(StoredInput::Reported {
                    value: Arc::new(values[&name].clone()),
                    source: Err(MissingEvidence::new(
                        "new materialized attribute has no source judgement",
                    )),
                }),
            );
        }
        self.apply_materializations(values);
        let inputs: &Self = self;
        let classified = collect_classified_attributes(
            inputs
                .values()
                .filter(|(name, _)| values.contains_key(*name))
                .map(|(name, value)| (name.to_owned(), value)),
            context,
            failed,
        );
        (inputs, classified)
    }

    /// Called where evaluation produces new projections. Unchanged slots share
    /// their allocations, so existing classifications still name that value.
    #[cfg(test)]
    pub(in crate::validation::admission) fn materialize(
        &mut self,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
    ) -> Result<(), MissingEvidence> {
        let unmatched = self.unmatched_materializations(values);
        if !unmatched.is_empty() {
            return Err(MissingEvidence::new(format!(
                "unmatched materialized attributes: {:?}",
                unmatched.iter().map(|(name, _)| name).collect::<Vec<_>>()
            )));
        }
        self.apply_materializations(values);
        Ok(())
    }

    fn fork(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            declaration: self.declaration.clone(),
            awaiting: self.awaiting.clone(),
            precheck: self.precheck.clone(),
            scope: self.scope,
            report: self.report.clone(),
            inputs: self.inputs.clone(),
            evidence: self.evidence.clone(),
        }
    }

    fn unmatched_materializations(
        &self,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
    ) -> Vec<(String, MissingEvidence)> {
        values
            .keys()
            .filter(|name| !self.inputs.contains_key(*name))
            .map(|name| {
                (
                    name.clone(),
                    MissingEvidence::new(format!("unmatched materialized attribute {name}")),
                )
            })
            .collect()
    }

    fn apply_materializations(&mut self, values: &IndexMap<String, Value<ParserPlaceholder, B>>) {
        for (name, input) in &mut self.inputs {
            let Some(value) = values.get(name) else {
                continue;
            };
            if input.value() == Some(value) {
                continue;
            }
            let stored = match input {
                PreparedAttribute::WithValue(stored) => stored,
                PreparedAttribute::WithoutValue { slot, .. } => {
                    let stored = match &self.evidence {
                        ResourceEvidence::Slots => StoredInput::Authored {
                            source_generation: slot.generation(),
                            slot: Arc::new(slot.materialized(value)),
                        },
                        ResourceEvidence::NoAuthoredEvidence(_) => StoredInput::Reported {
                            value: Arc::new(value.clone()),
                            source: match self.report.as_deref() {
                                Some(report) => slot
                                    .source_judgement_in(report)
                                    .or_else(|_| slot.source_judgement())
                                    .cloned(),
                                None => slot.source_judgement().cloned(),
                            },
                        },
                    };
                    *input = PreparedAttribute::WithValue(stored);
                    continue;
                }
            };
            match stored {
                StoredInput::Authored { slot, .. } => *slot = Arc::new(slot.materialized(value)),
                StoredInput::Projected {
                    value: projection, ..
                }
                | StoredInput::Reported {
                    value: projection, ..
                }
                | StoredInput::Unavailable {
                    value: projection, ..
                } => *projection = Arc::new(value.clone()),
            }
        }
    }

    pub(super) fn input<D>(
        &self,
        resource: &crate::parser::AnalysisResourceRef<
            '_,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        name: &str,
        report: &crate::parser::retained::ExpressionReport<B>,
        alias: Option<&str>,
    ) -> Result<AttributeInput<'_, B>, MissingEvidence>
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        if &self.owner != resource.id() {
            return Err(MissingEvidence::new("resource instance/projection pairing"));
        }
        let input = self
            .inputs
            .get(name)
            .ok_or_else(|| MissingEvidence::new(format!("prepared attribute {name}")))?;
        let input = match input {
            PreparedAttribute::WithValue(input) => input,
            PreparedAttribute::WithoutValue { .. } => {
                return Err(MissingEvidence::new("attribute has no runtime value"));
            }
        };
        match input {
            StoredInput::Authored {
                slot,
                source_generation,
            } => match resource.authored_slot(name) {
                Ok(source)
                    if source.instance() == slot.instance()
                        && source.generation() == *source_generation =>
                {
                    Ok(AttributeInput::Authored { slot })
                }
                Ok(_) => ValidationProjection::unavailable(
                    slot.source_judgement()?,
                    MissingEvidence::new("resource instance/generation pairing"),
                )
                .map(|phase| AttributeInput::Projected { phase }),
                Err(error) => ValidationProjection::unavailable(slot.source_judgement()?, error)
                    .map(|phase| AttributeInput::Projected { phase }),
            },
            StoredInput::Unavailable { error, source, .. } => match source {
                Ok(source) => ValidationProjection::unavailable(source, error.clone())
                    .map(|phase| AttributeInput::Projected { phase }),
                Err(_) => Err(error.clone()),
            },
            StoredInput::Projected { phase, .. } => Ok(AttributeInput::Projected {
                phase: phase.clone(),
            }),
            StoredInput::Reported { source, .. } => Ok(AttributeInput::Reported {
                source: source
                    .clone()
                    .or_else(|_| reported_source(resource, name, report, alias))?,
            }),
        }
    }
}

// The same preparation seam is used when legacy schema admission supplies
// classifications directly, rather than producer-owned resolved inputs.
pub(super) fn missing_schema_inputs<B: BindingAddress, V>(
    slots: &crate::parser::retained::AuthoredSlots<B>,
    attributes: &IndexMap<String, V>,
    report: &crate::parser::retained::ExpressionReport<B>,
) -> MissingInputs {
    let mut inputs = attributes
        .iter()
        .map(|(name, value)| (name.clone(), PreparedAttribute::WithValue(value)))
        .collect();
    join_slots(slots, &mut inputs);
    prepare_missing::<LiveResource, _, _>(inputs.iter(), report)
}

fn join_slots<B: BindingAddress, V>(
    slots: &crate::parser::retained::AuthoredSlots<B>,
    inputs: &mut IndexMap<String, PreparedAttribute<B, V>>,
) {
    // This ephemeral join includes canonical block aliases. It does not record
    // visits or transfer responsibility to a future walk.
    let paired = inputs
        .keys()
        .filter_map(|name| slots.get(name).ok())
        .map(|slot| (slot.occurrence(), slot.instance()))
        .collect::<std::collections::HashSet<_>>();
    for (name, slot) in slots.iter() {
        if !paired.contains(&(slot.occurrence(), slot.instance())) {
            inputs.insert(
                name.transport_identity().to_owned(),
                PreparedAttribute::WithoutValue {
                    slot: Arc::new(slot.clone()),
                    precheck: SlotPrecheck::NoOrdinaryFinding,
                },
            );
        }
    }
}

pub(super) struct MissingInputs {
    ordinary: Vec<(String, TypeError)>,
    conversions: crate::ConversionChannel,
}
impl MissingInputs {
    pub(super) fn finish(
        self,
        mut findings: crate::schema::ResourceValueValidation,
        coverage: &mut WalkCoverage,
    ) -> crate::schema::ResourceValueValidation {
        coverage.record_conversions(self.conversions);
        for (name, error) in self.ordinary {
            findings.push_attribute_error(&name, error);
        }
        findings
    }
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.ordinary.is_empty() && self.conversions.findings().is_empty()
    }
}
fn prepare_missing<'a, Pass: ResourceAdmissionPass, B: BindingAddress + 'a, V: 'a>(
    inputs: impl Iterator<Item = (&'a String, &'a PreparedAttribute<B, V>)>,
    report: &crate::parser::retained::ExpressionReport<B>,
) -> MissingInputs {
    let mut findings = MissingInputs {
        ordinary: Vec::new(),
        conversions: Default::default(),
    };
    for (name, input) in inputs {
        match input {
            PreparedAttribute::WithValue(_) => {}
            PreparedAttribute::WithoutValue { slot, precheck } => match precheck {
                SlotPrecheck::OrdinaryFinding => {}
                SlotPrecheck::NoOrdinaryFinding => {
                    match slot
                        .source_judgement_in(report)
                        .or_else(|_| slot.source_judgement())
                    {
                        Ok(source) => findings.conversions.append(Pass::missing_source(source)),
                        Err(missing) => findings.ordinary.push((
                            name.clone(),
                            crate::validation::admission::assignment_seam_error(missing),
                        )),
                    }
                }
            },
        }
    }
    findings
}

fn source_judgement<B: BindingAddress, D>(
    resource: &crate::parser::AnalysisResourceRef<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    name: &str,
) -> Result<JudgedExpr, MissingEvidence>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let slot = resource.authored_slot(name)?;
    slot.source_judgement().cloned()
}

// Existing semantic module-output bindings and schema block aliases identify
// the producer. Diagnostic locations never establish conversion authority.
fn reported_source<B: BindingAddress, D>(
    resource: &crate::parser::AnalysisResourceRef<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    name: &str,
    report: &crate::parser::retained::ExpressionReport<B>,
    alias: Option<&str>,
) -> Result<JudgedExpr, MissingEvidence>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    use crate::parser::AnalysisItemKind;
    match resource.kind() {
        AnalysisItemKind::Composition => report
            .module_attribute(
                resource
                    .binding_address()
                    .ok_or_else(|| MissingEvidence::new("composition binding"))?,
                name,
            )
            .cloned(),
        AnalysisItemKind::Resource
        | AnalysisItemKind::DataSource
        | AnalysisItemKind::DeferredResource => {
            let slot = resource
                .authored_slot(name)
                .or_else(|error| alias.map_or(Err(error), |alias| resource.authored_slot(alias)))?;
            slot.source_judgement_in(report)
                .or_else(|_| slot.source_judgement())
                .cloned()
        }
        AnalysisItemKind::Wait => Err(MissingEvidence::new("wait is not an attribute producer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r11_missing_omitted_source_evidence_is_a_failure() {
        let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            "mock.Sink { attr = 'ok' as mock.Name }",
            std::path::Path::new("missing-omitted-source.crn"),
            &catalog,
        )
        .unwrap()
        .resolve_root_for_test();
        let schemas = SchemaRegistry::new();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree.resources().next().unwrap();
        let mut slot = resource.authored_slot("attr").unwrap().clone();
        slot.invalidate_judgement();
        let inputs = [PreparedAttribute::<_, ()>::WithoutValue {
            slot: Arc::new(slot),
            precheck: SlotPrecheck::NoOrdinaryFinding,
        }];
        let empty =
            crate::parser::parse_file("", std::path::Path::new("empty-evidence.crn"), &catalog)
                .unwrap()
                .resolve_root_for_test();
        let name = "attr".to_owned();
        let missing = prepare_missing::<LiveResource, _, _>(
            inputs.iter().map(|input| (&name, input)),
            empty.expression_report(),
        );
        assert!(
            !missing.is_empty(),
            "missing source evidence was silently accepted"
        );
    }

    #[test]
    fn r9_report_only_omitted_input_cannot_gain_authored_authority_by_materializing() {
        let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            "mock.Sink { attr = 'ok' }",
            std::path::Path::new("report-materialization.crn"),
            &catalog,
        )
        .unwrap()
        .resolve_root_for_test();
        let schemas = SchemaRegistry::new();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree.resources().next().unwrap();
        let mut inputs = ResourceAttributeInputs::from_report_for_test(
            resource,
            &IndexMap::new(),
            judged.report(),
        );
        inputs
            .materialize(&IndexMap::from([(
                "attr".into(),
                Value::Concrete(crate::resource::ConcreteValue::String("ok".into())),
            )]))
            .unwrap();
        assert!(matches!(
            inputs
                .input(resource, "attr", judged.report(), None)
                .unwrap(),
            AttributeInput::Reported { .. }
        ));
        assert!(inputs.authored_evidence().is_err());
    }
}

type ValidationDispatch<B> = for<'v> fn(
    &Classification<'v, ParserPlaceholder, B>,
    &dyn ProviderTypeCheckContext,
) -> Option<Value<ParserPlaceholder, B>>;

/// Fixed evaluation policies for validation. Each mode uses admission's session.
#[expect(
    private_interfaces,
    reason = "Only typed constructors may supply an evaluation payload"
)]
pub enum ValidationEvaluation<B: BindingAddress> {
    None,
    Expanded(EvaluationDispatch<B>),
    RelativeStatic(EvaluationDispatch<B>),
}

// The binding type selects the evaluator. Keeping the shared payload private
// prevents callers from constructing or relabeling evaluation modes.
struct EvaluationDispatch<B: BindingAddress> {
    dispatch: ValidationDispatch<B>,
}

impl<B: BindingAddress> Copy for EvaluationDispatch<B> {}
impl<B: BindingAddress> Clone for EvaluationDispatch<B> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<B: BindingAddress> Copy for ValidationEvaluation<B> {}
impl<B: BindingAddress> Clone for ValidationEvaluation<B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl ValidationEvaluation<crate::resource::AbsoluteBindingAddress> {
    pub fn expanded() -> Self {
        Self::Expanded(EvaluationDispatch {
            dispatch: crate::parser::evaluate_expanded_value_for_validation,
        })
    }
}

impl ValidationEvaluation<crate::resource::RelativeBindingName> {
    pub fn relative_static() -> Self {
        Self::RelativeStatic(EvaluationDispatch {
            dispatch: |proof, session| {
                if matches!(proof, Classification::Known(_)) {
                    return None;
                }
                crate::parser::evaluate_static_value_for_validation(proof, session.catalog())
            },
        })
    }
}

/// Classification failures in initial-pass, then changed-pass order.
#[derive(Debug)]
pub struct ClassificationDiagnostics<B: BindingAddress>(pub Vec<(String, KnowledgeError<B>)>);

/// Owned findings and classification failures from the consumed product.
#[derive(Debug)]
pub struct ResourceAdmissionOutcome<B: BindingAddress> {
    pub values: crate::schema::ResourceValueValidation,
    pub classification_diagnostics: ClassificationDiagnostics<B>,
}

/// Run classification and evaluation without admitting values or recording coverage.
pub fn classification_preflight<B: BindingAddress>(
    inputs: &ResourceAttributeInputs<B>,
    evaluation: ValidationEvaluation<B>,
    session: &dyn ProviderTypeCheckContext,
) -> ClassificationDiagnostics<B> {
    with_classified_inputs(inputs, evaluation, session, |_, _| ()).1
}

pub(super) fn with_classified_inputs<B: BindingAddress, R>(
    inputs: &ResourceAttributeInputs<B>,
    evaluation: ValidationEvaluation<B>,
    session: &dyn ProviderTypeCheckContext,
    judge: impl for<'x> FnOnce(
        &'x ResourceAttributeInputs<B>,
        &'x IndexMap<String, Classification<'x, ParserPlaceholder, B>>,
    ) -> R,
) -> (R, ClassificationDiagnostics<B>) {
    let mut diagnostics = ClassificationDiagnostics(Vec::new());
    let mut failed = |name: &str, error| diagnostics.0.push((name.to_owned(), error));
    let context = KnowledgeContext::validation();
    let mut classified = inputs.classify(&context, &mut failed);
    #[cfg(test)]
    classification_tests::collect_injected_errors(&classified, &mut failed);
    // The binding type selects the evaluator through its typed constructor.
    let dispatch = match evaluation {
        ValidationEvaluation::None => return (judge(inputs, &classified), diagnostics),
        ValidationEvaluation::Expanded(mode) | ValidationEvaluation::RelativeStatic(mode) => {
            mode.dispatch
        }
    };
    let evaluated = classified
        .iter()
        .filter_map(|(name, proof)| {
            dispatch(proof, session)
                .filter(|value| inputs.value(name).is_some_and(|source| source != value))
                .map(|value| (name.clone(), value))
        })
        .collect();
    // Initial proofs borrow the original owner. Only changed projections get
    // new allocations; unchanged proofs also belong to this shared fork.
    let mut evaluated_inputs = inputs.fork();
    let (evaluated_inputs, updated) =
        evaluated_inputs.materialize_and_classify(&evaluated, &context, &mut failed);
    #[cfg(test)]
    classification_tests::collect_injected_errors(&updated, &mut failed);
    classified.extend(updated);
    (judge(evaluated_inputs, &classified), diagnostics)
}
