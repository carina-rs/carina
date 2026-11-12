//! Phase-independent, owned declaration snapshots. No expression inference.

use std::marker::PhantomData;

use super::*;
use crate::{
    binding_index::WaitAliasSpec,
    effect::deps::UnresolvedResource,
    resource::{AbsoluteBindingAddress, DataSource},
};

#[derive(Clone, Default)]
pub(super) struct TypeFacts {
    pub selection: super::union_fits::InputSelectionAuthority,
    pub bindings: HashMap<AbsoluteBindingAddress, Declaration>,
    pub aliases: HashMap<AbsoluteBindingAddress, AbsoluteBindingAddress>,
    pub defaults: Vec<DefaultTagEvidence>,
}

#[derive(Clone)]
pub(super) struct DefaultTagEvidence {
    pub route: crate::provider::ProviderInstanceKey,
    pub values: InputAttributes,
    pub completeness: DefaultTagCompleteness,
}

#[derive(Clone, Copy)]
pub(super) enum DefaultTagCompleteness {
    Complete,
    Unavailable(DefaultTagUnavailable),
}

#[derive(Clone, Copy)]
pub(super) enum DefaultTagUnavailable {
    Unresolved,
    ProviderConfigurationFailed,
}

impl DefaultTagCompleteness {
    pub fn is_complete(self) -> bool {
        match self {
            Self::Complete => true,
            Self::Unavailable(reason) => match reason {
                DefaultTagUnavailable::Unresolved
                | DefaultTagUnavailable::ProviderConfigurationFailed => false,
            },
        }
    }
}

impl DefaultTagEvidence {
    pub fn for_config<P: crate::resource::PlaceholderKind, B: crate::resource::BindingAddress>(
        config: &crate::parser::ProviderConfig<P, B>,
    ) -> Self {
        let (values, public) = match config.default_tags.public_value_map() {
            Ok(values) => (project_attributes(&values), true),
            Err(_) => (Default::default(), false),
        };
        Self {
            route: crate::provider::ProviderInstanceKey::from(config),
            values,
            completeness: if !public {
                DefaultTagCompleteness::Unavailable(
                    DefaultTagUnavailable::ProviderConfigurationFailed,
                )
            } else if config.unresolved_attributes.contains_key("default_tags") {
                DefaultTagCompleteness::Unavailable(DefaultTagUnavailable::Unresolved)
            } else {
                DefaultTagCompleteness::Complete
            },
        }
    }

    pub fn for_preview(preview: &crate::parser::ProviderInputPreview) -> Self {
        let evidence = Self::for_config(preview.config());
        match preview.status() {
            crate::parser::ProviderInputStatus::Complete => evidence,
            crate::parser::ProviderInputStatus::Failed => evidence.after_config_failure(),
        }
    }

    pub fn after_config_failure(mut self) -> Self {
        if let DefaultTagCompleteness::Unavailable(_) = self.completeness {
            self.completeness = DefaultTagCompleteness::Unavailable(
                DefaultTagUnavailable::ProviderConfigurationFailed,
            );
        }
        self
    }
}

#[derive(Clone)]
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
/// ```compile_fail,E0599
/// use carina_core::{input_rules::InputTypeContext, resource::Value, schema::AttributeType};
/// let types = InputTypeContext::<carina_core::resource::ModuleSource>::empty();
/// let _ = types.estimate(Value::Concrete(carina_core::resource::ConcreteValue::String("x".into())), AttributeType::string());
/// ```
pub struct InputTypeContext<S = ModuleSource> {
    pub(super) facts: TypeFacts,
    source: PhantomData<fn() -> S>,
}

impl<S> InputTypeContext<S> {
    pub fn with_provider_context(mut self, context: &crate::parser::LoadedProviderContext) -> Self {
        self.facts.selection = super::union_fits::InputSelectionAuthority::in_catalog(context);
        self
    }
    /// Build the complete input context shared by live, saved, and bootstrap
    /// admission, including declarations and host-merged default tag keys.
    pub fn for_plan<'a, D>(
        resources: &[UnresolvedResource<S>],
        data_sources: &[DataSource<S, crate::resource::NoPlaceholder, AbsoluteBindingAddress, D>],
        compositions: InputCompositions<'_>,
        wait_aliases: &[WaitAliasSpec],
        configs: &[crate::parser::ExpandedProviderConfig],
        schemas: &SchemaRegistry,
        write_ids: impl IntoIterator<Item = &'a ResourceId>,
    ) -> crate::provider::HostResult<Self> {
        Self::from_compositions(resources, &[], compositions, wait_aliases, schemas)
            .with_data_sources(data_sources, schemas)
            .with_default_tags(configs, schemas, write_ids)
    }

    /// An empty catalog preserves fail-closed admission for callers that have
    /// no retained source expressions.
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
        self,
        configs: &[crate::parser::ExpandedProviderConfig],
        schemas: &SchemaRegistry,
        write_ids: impl IntoIterator<Item = &'a ResourceId>,
    ) -> crate::provider::HostResult<Self> {
        self.with_default_tag_projection(configs, schemas, write_ids)
    }

    pub(super) fn with_default_tag_projection<'a, P: crate::resource::PlaceholderKind>(
        mut self,
        configs: &[crate::parser::ProviderConfig<P, AbsoluteBindingAddress>],
        schemas: &SchemaRegistry,
        write_ids: impl IntoIterator<Item = &'a ResourceId>,
    ) -> crate::provider::HostResult<Self> {
        let _ = (schemas, write_ids);
        self.facts.defaults = configs.iter().map(DefaultTagEvidence::for_config).collect();
        Ok(self)
    }
}
