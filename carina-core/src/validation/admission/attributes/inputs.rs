use super::*;
use crate::parser::retained::{JudgedSlot, MissingEvidence, ProjectedPhase, ProjectionGeneration};
use crate::resource::ParserPlaceholder;
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
    inputs: IndexMap<String, AttributeInput<B>>,
}

#[derive(Clone)]
pub(super) enum AttributeInput<B: BindingAddress> {
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

impl<B: BindingAddress> AttributeInput<B> {
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
                        return Ok(AttributeInput::Composition(Arc::new(value.clone())));
                    }
                    if let Some(phase) = resource.projected_phase(name) {
                        return Ok(AttributeInput::Projected {
                            value: Arc::new(value.clone()),
                            phase,
                        });
                    }
                    let slot = resource.authored_slot(name)?;
                    if slot.projection() != Some(value) {
                        return Err(MissingEvidence::new("resource slot/projection pairing"));
                    }
                    slot.cursor()?;
                    Ok(AttributeInput::Authored {
                        slot: Arc::new(slot.clone()),
                        source_generation: slot.generation(),
                    })
                };
                (
                    name.clone(),
                    prepare().unwrap_or_else(|error| AttributeInput::Unavailable {
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
        self.inputs.get(name).map(AttributeInput::value)
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
                AttributeInput::Authored { slot, .. } => *slot = Arc::new(slot.materialized(value)),
                AttributeInput::Projected {
                    value: projection, ..
                }
                | AttributeInput::Composition(projection)
                | AttributeInput::Unavailable {
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
    ) -> Result<&AttributeInput<B>, MissingEvidence>
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
            AttributeInput::Authored {
                slot,
                source_generation,
            } => {
                let source = resource.authored_slot(name)?;
                if source.instance() != slot.instance() || source.generation() != *source_generation
                {
                    return Err(MissingEvidence::new("resource instance/generation pairing"));
                }
            }
            AttributeInput::Unavailable { error, .. } => return Err(error.clone()),
            AttributeInput::Projected { .. } | AttributeInput::Composition(_) => {}
        }
        Ok(input)
    }
}
