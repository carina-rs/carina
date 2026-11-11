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
        let inputs = resource
            .attributes()
            .iter()
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
                    if slot.projection() != Some(value) {
                        return Err(MissingEvidence::new("resource slot/projection pairing"));
                    }
                    slot.cursor()?;
                    Ok(StoredInput::Authored {
                        slot: Arc::new(slot.clone()),
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
        self.materialize(values);
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
    pub fn materialize(&mut self, values: &HashMap<String, Value<ParserPlaceholder, B>>) {
        for (name, value) in values {
            let Some(input) = self.inputs.get_mut(name) else {
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
