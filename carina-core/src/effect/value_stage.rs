//! Structural mapping between snapshot and evaluation plan carriers.
use super::*;
use crate::resource::{ValueKind, plan_directives, template_directives};

pub(crate) fn map_template<S: Clone, P: PlanValueKind, Q: PlanValueKind, E>(
    template: &DeferredForExpression<S, P, P::Template>,
    mut iterable: impl FnMut(&Value<P>) -> Result<Value<Q>, E>,
    mut attribute: impl FnMut(&str, &Value<P::Template>) -> Result<Value<Q::Template>, E>,
) -> Result<DeferredForExpression<S, Q, Q::Template>, E> {
    let resource = template.template_resource().as_resource();
    let attributes = resource.attributes.iter().map(|(key, value)|
        attribute(key, value).map(|value| (key.clone(), value))).collect::<Result<_, _>>()?;
    let resource = resource.clone().with_value_stage(attributes,
        template_directives::<P, Q>(resource.directives.clone()));
    Ok(template.clone().with_value_stages(iterable(&template.iterable)?, resource))
}

impl<S: Clone, P: PlanValueKind> EffectWithSource<S, P> {
    pub(crate) fn try_map_value_stage<Q: PlanValueKind, E>(
        &self,
        mut attribute: impl FnMut(&ResourceId, &str, &Value<P>) -> Result<Value<Q>, E>,
        mut import: impl FnMut(&ResourceId, &Value<P>) -> Result<Value<Q>, E>,
        mut template: impl FnMut(&DeferredForExpression<S, P, P::Template>) -> Result<DeferredForExpression<S, Q, Q::Template>, E>,
    ) -> Result<EffectWithSource<S, Q>, E> {
        let mut map_resource = |resource: &ResolvedResource<S, P>| {
            let attributes = resource.attributes.iter().map(|(key, value)|
                attribute(&resource.id, key, value).map(|value| (key.clone(), value)))
                .collect::<Result<_,_>>()?;
            Ok(resource.replace_inner(resource.as_inner().clone().with_value_stage(attributes,
                plan_directives::<P, Q>(resource.directives.clone()))))
        };
        Ok(match self {
            Effect::Create(resource) => Effect::Create(map_resource(resource)?),
            Effect::ResumeCreate { resource, started_at } => Effect::ResumeCreate {
                resource: map_resource(resource)?, started_at: started_at.clone(),
            },
            Effect::Update { from, identifier, to, changed_attributes } => Effect::Update {
                from: from.clone(), identifier: identifier.clone(), to: map_resource(to)?,
                changed_attributes: changed_attributes.clone(),
            },
            Effect::Read { resource } => {
                let attributes = resource.attributes.iter().map(|(key, value)|
                    attribute(&resource.id, key, value).map(|value| (key.clone(), value)))
                    .collect::<Result<_,_>>()?;
                Effect::Read { resource: resource.replace_inner(resource.as_inner().clone().with_value_stage(attributes,
                    plan_directives::<P, Q>(resource.directives.clone()))) }
            }
            Effect::Import { id, identifier } => Effect::Import { id: id.clone(), identifier: import(id, identifier)? },
            Effect::Delete { id, identifier, generation, directives, binding, dependencies, explicit_dependencies, blocked_by_updates } => Effect::Delete {
                id: id.clone(), identifier: identifier.clone(), generation: generation.clone(), directives: directives.clone(),
                binding: binding.clone(), dependencies: dependencies.clone(), explicit_dependencies: explicit_dependencies.clone(), blocked_by_updates: blocked_by_updates.clone(),
            },
            Effect::Remove { id } => Effect::Remove { id: id.clone() },
            Effect::Move { from, to, origin } => Effect::Move { from: from.clone(), to: to.clone(), origin: origin.clone() },
            Effect::ReleaseListKeys { id, attribute, key_field, keys } => Effect::ReleaseListKeys {
                id: id.clone(), attribute: attribute.clone(), key_field: key_field.clone(), keys: keys.clone(),
            },
            Effect::Wait { id, target_id, until, until_surface, timeout, interval, explicit_dependencies } => Effect::Wait {
                id: id.clone(), target_id: target_id.clone(), until: until.clone(), until_surface: until_surface.clone(),
                timeout: *timeout, interval: *interval, explicit_dependencies: explicit_dependencies.clone(),
            },
            Effect::DeferredCreate(payload) => Effect::DeferredCreate(DeferredCreatePayload::new(
                Box::new(template(&payload.template)?), payload.dependency_template.clone(),
            )),
            Effect::DeferredReplace(payload) => Effect::DeferredReplace(Box::new(DeferredReplacePayload {
                deletes: payload.deletes.clone(), dependency_template: payload.dependency_template.clone(), template: Box::new(template(&payload.template)?),
            })),
        })
    }
}
