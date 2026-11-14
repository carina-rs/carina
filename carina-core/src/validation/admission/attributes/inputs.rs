use super::*;
use crate::parser::retained::{JudgedSlot, MissingEvidence, ProjectedPhase, ProjectionGeneration};
use crate::resource::ParserPlaceholder;
use crate::resource::knowledge::{
    Classification, ClassificationStage, KnowledgeContext, KnowledgeError,
    collect_classified_attributes,
};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;

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
/// Classify `values()` after producing/evaluating these inputs. Admission only
/// borrows the resulting pairing; it never replaces a slot's projection.
#[derive(Clone)]
pub struct ResourceAttributeInputs<B: BindingAddress> {
    owner: ResourceId,
    report: Option<Arc<crate::parser::retained::ExpressionReport<B>>>,
    inputs: IndexMap<String, StoredInput<B>>,
    evidence: ResourceEvidence,
}

#[derive(Clone)]
enum ResourceEvidence {
    Slots,
    NoAuthoredEvidence(Arc<crate::parser::retained::NoAuthoredEvidence>),
}

pub(super) enum AttributeInput<'a, B: BindingAddress> {
    Authored { slot: &'a JudgedSlot<B> },
    Projected { phase: ProjectedPhase },
    Reported,
}

#[derive(Clone)]
enum StoredInput<B: BindingAddress> {
    Authored {
        slot: Arc<JudgedSlot<B>>,
        source_generation: ProjectionGeneration,
    },
    Projected {
        value: Arc<Value<ParserPlaceholder, B>>,
        phase: ProjectedPhase,
    },
    // Inputs outside the slot inventory retain main's report-based checks.
    // This mode never grants authored or covered authority.
    Reported(Arc<Value<ParserPlaceholder, B>>),
    Unavailable {
        value: Arc<Value<ParserPlaceholder, B>>,
        error: MissingEvidence,
    },
}

impl<B: BindingAddress> StoredInput<B> {
    fn value(&self) -> &Value<ParserPlaceholder, B> {
        match self {
            Self::Authored { slot, .. } => slot.projection().expect("prepared materialized slot"),
            Self::Projected { value, .. }
            | Self::Reported(value)
            | Self::Unavailable { value, .. } => value,
        }
    }
}

impl<B: BindingAddress> ResourceAttributeInputs<B> {
    pub fn new<D>(
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
    pub fn for_resource<D>(
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
        let mut inputs = Self::new(resource);
        inputs.report = Some(Arc::new(report.clone()));
        Ok(inputs)
    }

    /// Main's buffer path when an authored owner/tree is not available.
    /// Values keep their classifications, and computed checks use this report.
    pub fn from_report<D>(
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
        Self::from_report_with_evidence(
            resource,
            values,
            report,
            crate::parser::retained::NoAuthoredEvidence::for_resource(resource),
        )
    }

    /// Report-based checks keep their checked owner and resolved values.
    /// Unreached source identities are metadata, never a replacement owner.
    pub fn from_report_with_evidence<D>(
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
        Self {
            owner: resource.id().clone(),
            evidence: ResourceEvidence::NoAuthoredEvidence(Arc::new(unchecked)),
            report: Some(Arc::new(report.clone())),
            inputs: in_resource_order(resource, values)
                .map(|(name, value)| (name.clone(), StoredInput::Reported(Arc::new(value.clone()))))
                .collect(),
        }
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
        matches!(self.inputs.get(name), Some(StoredInput::Reported(_)))
    }

    /// Produce inputs for exactly the resolved attribute key set. Evaluation
    /// owns this materialization; admission still borrows each slot's value.
    pub fn from_resolved<D>(
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
                        Err(_) => return Ok(StoredInput::Reported(Arc::new(value.clone()))),
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
                    prepare().unwrap_or_else(|error| StoredInput::Unavailable {
                        value: Arc::new(value.clone()),
                        error,
                    }),
                )
            })
            .collect();
        Self {
            owner: resource.id().clone(),
            report: None,
            inputs,
            evidence: ResourceEvidence::Slots,
        }
    }

    /// Input provenance, distinct from candidate sealing or successful admission.
    /// Report-only inputs never confer authored authority on their source slots.
    pub fn authored_evidence(&self) -> Result<(), &crate::parser::retained::NoAuthoredEvidence> {
        match &self.evidence {
            ResourceEvidence::Slots => Ok(()),
            ResourceEvidence::NoAuthoredEvidence(missing) => Err(missing),
        }
    }

    pub fn values(&self) -> impl Iterator<Item = (&str, &Value<ParserPlaceholder, B>)> {
        self.inputs
            .iter()
            .map(|(name, input)| (name.as_str(), input.value()))
    }

    pub fn value(&self, name: &str) -> Option<&Value<ParserPlaceholder, B>> {
        self.inputs.get(name).map(StoredInput::value)
    }

    pub fn classify<'a, S: ClassificationStage>(
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
    pub fn materialize_and_classify<'a, S: ClassificationStage>(
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
                StoredInput::Reported(Arc::new(values[&name].clone())),
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
    pub(crate) fn materialize(
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
            if input.value() == value {
                continue;
            }
            match input {
                StoredInput::Authored { slot, .. } => *slot = Arc::new(slot.materialized(value)),
                StoredInput::Projected {
                    value: projection, ..
                }
                | StoredInput::Reported(projection)
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
        match input {
            StoredInput::Authored {
                slot,
                source_generation,
            } => {
                let source = resource.authored_slot(name)?;
                if source.instance() != slot.instance() || source.generation() != *source_generation
                {
                    return Err(MissingEvidence::new("resource instance/generation pairing"));
                }
                Ok(AttributeInput::Authored { slot })
            }
            StoredInput::Unavailable { error, .. } => Err(error.clone()),
            StoredInput::Projected { phase, .. } => Ok(AttributeInput::Projected { phase: *phase }),
            StoredInput::Reported(_) => Ok(AttributeInput::Reported),
        }
    }
}
