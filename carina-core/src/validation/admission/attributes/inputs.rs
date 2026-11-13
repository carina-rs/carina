use super::*;
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

/// Producer-owned attribute projections and their authored authority.
///
/// Classify `values()` after producing/evaluating these inputs. Admission only
/// borrows the resulting pairing; it never replaces a slot's projection.
#[derive(Clone)]
pub struct ResourceAttributeInputs<B: BindingAddress> {
    owner: ResourceId,
    pub(super) scope: crate::schema::value_validation::AssignmentScope,
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
    Projected { phase: ValidationProjection },
    Reported { source: JudgedExpr },
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
        values: &HashMap<String, Value<ParserPlaceholder, B>>,
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
        values: &HashMap<String, Value<ParserPlaceholder, B>>,
        report: &crate::parser::retained::ExpressionReport<B>,
        unchecked: crate::parser::retained::NoAuthoredEvidence,
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self {
            owner: resource.id().clone(),
            scope: crate::schema::value_validation::AssignmentScope::Full,
            evidence: ResourceEvidence::NoAuthoredEvidence(Arc::new(unchecked)),
            report: Some(Arc::new(report.clone())),
            inputs: values
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        StoredInput::Reported {
                            value: Arc::new(value.clone()),
                            source: source_judgement(resource, name),
                        },
                    )
                })
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
        matches!(self.inputs.get(name), Some(StoredInput::Reported { .. }))
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
        values: &HashMap<String, Value<ParserPlaceholder, B>>,
        report: &crate::parser::retained::ExpressionReport<B>,
    ) -> Result<Self, MissingEvidence>
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let report = report.for_resource(resource.id())?;
        let mut inputs = Self::prepare(resource, values.iter(), true);
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
                    prepare().unwrap_or_else(|error| StoredInput::Unavailable {
                        value: Arc::new(value.clone()),
                        error,
                        source: source_judgement(resource, name),
                    }),
                )
            })
            .collect();
        Self {
            owner: resource.id().clone(),
            scope: crate::schema::value_validation::AssignmentScope::Full,
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
    ) -> HashMap<String, Classification<'a, ParserPlaceholder, B>> {
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
        values: &HashMap<String, Value<ParserPlaceholder, B>>,
        context: &KnowledgeContext<'_, ParserPlaceholder, B, S>,
        failed: impl FnMut(&str, KnowledgeError<B>),
    ) -> (
        &'a Self,
        HashMap<String, Classification<'a, ParserPlaceholder, B>>,
    ) {
        // Additional resolved names have no authored authority. Preserve
        // main's classified/report path instead of inventing a seam finding.
        for (name, _) in self.unmatched_materializations(values) {
            self.inputs.insert(
                name.clone(),
                StoredInput::Reported {
                    value: Arc::new(values[&name].clone()),
                    source: Err(MissingEvidence::new(
                        "new materialized attribute has no source judgement",
                    )),
                },
            );
        }
        self.apply_materializations(values);
        let inputs: &Self = self;
        let classified = collect_classified_attributes(
            values
                .keys()
                .filter_map(|name| inputs.value(name).map(|value| (name.clone(), value))),
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
        values: &HashMap<String, Value<ParserPlaceholder, B>>,
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
        values: &HashMap<String, Value<ParserPlaceholder, B>>,
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

    fn apply_materializations(&mut self, values: &HashMap<String, Value<ParserPlaceholder, B>>) {
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
