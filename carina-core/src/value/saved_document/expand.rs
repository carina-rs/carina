use super::*;
use crate::resolver::PlanningEvaluation;

pub trait Expand: DocumentTree {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError>;
}
impl<P: ValueKind> Expand for Value<P> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        *self = evaluation.expression(self)?.into_expression();
        Ok(())
    }
}
impl<T: Expand> Expand for Vec<T> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        for value in self {
            value.expand(evaluation)?;
        }
        Ok(())
    }
}
impl<T: Expand> Expand for Option<T> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        if let Some(value) = self {
            value.expand(evaluation)?;
        }
        Ok(())
    }
}
impl<K, V: Expand> Expand for HashMap<K, V> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        self.values_mut()
            .try_for_each(|value| value.expand(evaluation))
    }
}
impl<K, V: Expand> Expand for IndexMap<K, V> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        for value in self.values_mut() {
            value.expand(evaluation)?;
        }
        Ok(())
    }
}
impl<T: Expand> Expand for Box<T> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        (**self).expand(evaluation)
    }
}
impl<S, P: PlanValueKind, D> Expand for crate::resource::Resource<S, P, AbsoluteBindingAddress, D> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        self.attributes.expand(evaluation)
    }
}
impl<S, P: PlanValueKind, D> Expand
    for crate::resource::DataSource<S, P, AbsoluteBindingAddress, D>
{
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        self.attributes.expand(evaluation)
    }
}
impl<P: StateValueLeaf> Expand for crate::resource::State<P> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        self.attributes.expand(evaluation)
    }
}
impl<P: PlanValueKind> Expand for crate::parser::SavedProviderConfigs<P> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        let configs = self
            .as_provider_configs()
            .iter()
            .map(|config| {
                config.try_map_value_stage(&mut |value| {
                    evaluation
                        .expression(value)
                        .map(|pair| pair.into_expression())
                        .map_err(SerializationError::from)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        *self = Self::from_snapshot_configs(configs);
        Ok(())
    }
}
impl Expand for crate::parser::ExpandedBackendConfig {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        self.attributes.expand(evaluation)
    }
}
impl Expand for crate::data_source_value::DataSourceValue {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        match self {
            Self::Read(state) => state.expand(evaluation),
            Self::DeferredToApply(_inputs) => Ok(()),
            Self::BlockedByPlanError => Ok(()),
        }
    }
}
impl Expand for crate::replacement::ReplacementInputs {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        for old in &mut self.old_instances {
            old.attributes.expand(evaluation)?;
        }
        Ok(())
    }
}
impl<S: Clone, P: PlanValueKind> Expand for crate::effect::Effect<S, P> {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        let mapped = self.try_map_value_stage(
            |_, _, value| {
                evaluation
                    .expression(value)
                    .map(|pair| pair.into_expression())
                    .map_err(SerializationError::from)
            },
            |_, value| {
                evaluation
                    .expression(value)
                    .map(|pair| pair.into_expression())
                    .map_err(SerializationError::from)
            },
            |template| {
                let mut template = template.clone();
                template.expand(evaluation)?;
                Ok::<_, SerializationError>(template)
            },
        )?;
        *self = mapped;
        match self {
            Self::Update {
                from,
                identifier: _,
                to: _,
                changed_attributes: _,
            } => from.expand(evaluation)?,
            Self::DeferredReplace(payload) => payload.expand_saved_values(evaluation)?,
            Self::ReleaseListKeys {
                id: _,
                attribute: _,
                key_field: _,
                keys,
            } => keys.expand_saved_values(evaluation)?,
            Self::Read { resource: _ }
            | Self::Create(_)
            | Self::ResumeCreate {
                resource: _,
                started_at: _,
            }
            | Self::Import {
                id: _,
                identifier: _,
            }
            | Self::DeferredCreate(_)
            | Self::Delete {
                id: _,
                identifier: _,
                generation: _,
                directives: _,
                binding: _,
                dependencies: _,
                explicit_dependencies: _,
                blocked_by_updates: _,
            }
            | Self::Remove { id: _ }
            | Self::Move {
                from: _,
                to: _,
                origin: _,
            }
            | Self::Wait {
                id: _,
                target_id: _,
                until: _,
                until_surface: _,
                timeout: _,
                interval: _,
                explicit_dependencies: _,
            } => {}
        }
        Ok(())
    }
}
// The unresolved replay carrier already has snapshot values. Rebuild through
// its own mutable inventory rather than changing the persisted representation.
impl Expand for crate::effect::UnresolvedSavedEffect {
    fn expand(&mut self, evaluation: &PlanningEvaluation) -> Result<(), SerializationError> {
        use crate::effect::UnresolvedSavedEffect as Node;
        match self {
            Node::Read { resource } => {
                let mut value = resource.as_inner().clone();
                value.expand(evaluation)?;
                *resource = resource.replace_inner(value);
                Ok(())
            }
            Node::Create(resource)
            | Node::ResumeCreate {
                resource,
                started_at: _,
            } => resource.expand_saved_values(evaluation),
            Node::Update {
                from,
                identifier: _,
                to,
                changed_attributes: _,
            } => {
                from.expand(evaluation)?;
                to.expand_saved_values(evaluation)
            }
            Node::Import { id: _, identifier } => identifier.expand(evaluation),
            Node::DeferredCreate(payload) => payload.template.expand(evaluation),
            Node::DeferredReplace(payload) => payload.expand_saved_values(evaluation),
            Node::ReleaseListKeys {
                id: _,
                attribute: _,
                key_field: _,
                keys,
            } => keys.expand_saved_values(evaluation),
            Node::Delete {
                id: _,
                identifier: _,
                generation: _,
                directives: _,
                binding: _,
                dependencies: _,
                explicit_dependencies: _,
                blocked_by_updates: _,
            } => Ok(()),
            Node::Remove { id: _ }
            | Node::Move {
                from: _,
                to: _,
                origin: _,
            } => Ok(()),
            Node::Wait {
                id: _,
                target_id: _,
                until: _,
                until_surface: _,
                timeout: _,
                interval: _,
                explicit_dependencies: _,
            } => Ok(()),
        }
    }
}
