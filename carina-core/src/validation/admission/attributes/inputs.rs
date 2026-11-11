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

/// Producer-owned attribute projections and their authored authority.
///
/// Classify `values()` after producing/evaluating these inputs. Admission only
/// borrows the resulting pairing; it never replaces a slot's projection.
#[derive(Clone)]
pub struct ResourceAttributeInputs<B: BindingAddress> {
    owner: ResourceId,
    inputs: IndexMap<String, StoredInput<B>>,
}

pub(super) enum AttributeInput<'a, B: BindingAddress> {
    Authored { slot: &'a JudgedSlot<B> },
    Projected { phase: ProjectedPhase },
    Composition,
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
    // Composition outputs are outside the resource-slot inventory in 1b.
    // They retain the report-based computed-expression checks from main.
    Composition(Arc<Value<ParserPlaceholder, B>>),
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
            | Self::Composition(value)
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
    ) -> Self
    where
        D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        Self::prepare(resource, values.iter(), true)
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
                    if resource.kind() == crate::parser::AnalysisItemKind::Composition {
                        return Ok(StoredInput::Composition(Arc::new(value.clone())));
                    }
                    if let Some(phase) = resource.projected_phase(name) {
                        return Ok(StoredInput::Projected {
                            value: Arc::new(value.clone()),
                            phase,
                        });
                    }
                    let slot = resource.authored_slot(name)?;
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
            inputs,
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
        // Evaluation normally returns a subset of this owner's names. Retain
        // a typed per-attribute outcome if that producer contract is broken;
        // classifications and sibling admission must still run.
        for (name, error) in self.unmatched_materializations(values) {
            self.inputs.insert(
                name.clone(),
                StoredInput::Unavailable {
                    value: Arc::new(values[&name].clone()),
                    error,
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
    pub fn materialize(
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
                | StoredInput::Composition(projection)
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
            StoredInput::Composition(_) => Ok(AttributeInput::Composition),
        }
    }
}
