//! Phase-independent, owned declaration snapshots. No expression inference.

use std::marker::PhantomData;

use super::*;
use crate::{
    binding_index::WaitAliasSpec,
    effect::deps::UnresolvedResource,
    resource::{AbsoluteBindingAddress, DataSource},
};

#[derive(Default)]
pub(super) struct TypeFacts {
    pub authored: HashMap<ResourceId, IndexMap<String, Value>>,
    pub bindings: HashMap<AbsoluteBindingAddress, Declaration>,
    pub aliases: HashMap<AbsoluteBindingAddress, AbsoluteBindingAddress>,
    pub default_keys: HashMap<ResourceId, Vec<(String, bool)>>,
}

pub(super) enum Declaration {
    Schema(Arc<ValidatedResourceSchema>),
    Outputs(IndexMap<String, Value>),
}

/// Composition expressions retained by the live or saved planning phase.
/// Both forms use the resource module's common execution-value projection.
pub enum InputCompositions<'a> {
    Live(&'a [crate::resource::LiveComposition]),
    Saved(&'a [crate::resource::SavedComposition<crate::resource::NoPlaceholder>]),
}

impl InputCompositions<'_> {
    fn outputs(&self) -> impl Iterator<Item = (AbsoluteBindingAddress, IndexMap<String, Value>)> {
        use crate::resource::CompositionRuntimeRef;
        let (live, saved) = match self {
            Self::Live(compositions) => (*compositions, &[][..]),
            Self::Saved(compositions) => (&[][..], *compositions),
        };
        live.iter()
            .map(CompositionRuntimeRef::Live)
            .chain(saved.iter().map(CompositionRuntimeRef::Saved))
            .filter_map(|composition| {
                Some((
                    composition.binding_address()?.clone(),
                    composition
                        .attributes()
                        .map(|(name, value)| (name.clone(), value))
                        .collect(),
                ))
            })
    }
}

/// Immutable types and authored expressions for one plan. Both live
/// `ModuleSource` and `SavedModuleSource` resources can build this context.
/// Composition outputs are expressions, so forwarded declarations keep their
/// original refinements rather than becoming inferred plain Strings.
///
/// This context does not provide an arbitrary-value estimator:
///
/// ```compile_fail
/// use carina_core::{input_rules::InputTypeContext, resource::Value, schema::AttributeType};
/// let types = InputTypeContext::<carina_core::resource::ModuleSource>::empty();
/// let _ = types.estimate(Value::Concrete(carina_core::resource::ConcreteValue::String("x".into())), AttributeType::string());
/// ```
pub struct InputTypeContext<S = ModuleSource> {
    pub(super) facts: TypeFacts,
    source: PhantomData<fn() -> S>,
}

impl<S> InputTypeContext<S> {
    /// An empty catalog preserves fail-closed admission for callers that have
    /// no retained source expressions (including bootstrap provider requests).
    pub fn empty() -> Self {
        Self {
            facts: TypeFacts::default(),
            source: PhantomData,
        }
    }

    /// Build live and saved contexts through the same composition projection.
    pub fn from_compositions(
        resources: &[UnresolvedResource<S>],
        data_sources: &[DataSource<S>],
        compositions: InputCompositions<'_>,
        wait_aliases: &[WaitAliasSpec],
        schemas: &SchemaRegistry,
    ) -> Self {
        Self::from_sources(
            resources,
            data_sources,
            compositions.outputs(),
            wait_aliases,
            schemas,
        )
    }

    pub fn from_sources(
        resources: &[UnresolvedResource<S>],
        data_sources: &[DataSource<S>],
        composition_outputs: impl IntoIterator<Item = (AbsoluteBindingAddress, IndexMap<String, Value>)>,
        wait_aliases: &[WaitAliasSpec],
        schemas: &SchemaRegistry,
    ) -> Self {
        let mut context = Self::empty();
        for source in resources {
            let resource = source.as_resource();
            context
                .facts
                .authored
                .insert(resource.id.clone(), resource.attributes.clone());
            if let Some((binding, schema)) =
                resource.binding.as_ref().zip(schemas.get_for(resource))
            {
                context.facts.bindings.insert(
                    binding.clone(),
                    Declaration::Schema(Arc::new(schema.schema().clone())),
                );
            }
        }
        context = context.with_data_sources(data_sources, schemas);
        for (binding, outputs) in composition_outputs {
            context
                .facts
                .bindings
                .insert(binding, Declaration::Outputs(outputs));
        }
        context.facts.aliases.extend(
            wait_aliases
                .iter()
                .map(|alias| (alias.binding.clone(), alias.target.clone())),
        );
        context
    }

    /// Declaration lookup is independent of data-source dependency phase;
    /// saved origins retain the same schema types with erased dependencies.
    pub fn with_data_sources<D>(
        mut self,
        data_sources: &[DataSource<S, crate::resource::NoPlaceholder, AbsoluteBindingAddress, D>],
        schemas: &SchemaRegistry,
    ) -> Self {
        for source in data_sources {
            if let Some((binding, schema)) = source
                .binding
                .as_ref()
                .zip(schemas.get_for_data_source(source))
            {
                self.facts.bindings.insert(
                    binding.clone(),
                    Declaration::Schema(Arc::new(schema.schema().clone())),
                );
            }
        }
        self
    }

    /// Project mandatory and possible default tag keys through the same host
    /// merge as concrete desired inputs. An unresolved tag value may be skipped
    /// now but merge later; its key can block proof, never force a violation.
    pub fn with_default_tags<'a>(
        mut self,
        configs: &[crate::parser::ExpandedProviderConfig],
        schemas: &SchemaRegistry,
        write_ids: impl IntoIterator<Item = &'a ResourceId>,
    ) -> crate::provider::ProviderResult<Self> {
        let mut mandatory = write_ids
            .into_iter()
            .map(|id| {
                crate::resource::Resource::from_id(id.clone())
                    .with_attribute("tags", Value::Concrete(ConcreteValue::Map(IndexMap::new())))
            })
            .collect::<Vec<_>>();
        let mut possible = mandatory.clone();
        for config in configs {
            crate::provider::merge_default_tags(config, &mut mandatory, schemas)?;
            let mut all_keys = config.clone();
            for value in all_keys.default_tags.values_mut() {
                *value = Value::Concrete(ConcreteValue::String("".into()));
            }
            crate::provider::merge_default_tags(&all_keys, &mut possible, schemas)?;
        }
        for (possible, mandatory) in possible.iter().zip(&mandatory) {
            if let Some(Value::Concrete(ConcreteValue::Map(keys))) = possible.attributes.get("tags")
            {
                self.facts.default_keys.insert(
                    possible.id.clone(),
                    keys.keys()
                        .map(|key| (key.clone(), mandatory.default_tag_keys.contains(key)))
                        .collect(),
                );
            }
        }
        Ok(self)
    }
}
