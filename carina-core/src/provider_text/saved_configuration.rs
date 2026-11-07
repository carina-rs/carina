//! Complete desired inputs decoded from a saved configuration document.
use crate::resource::{
    AbsoluteBindingAddress, DataSource, NoPlaceholder, Resource, ResourceId, SavedModuleSource,
    SnapshotLeaf, ValueKind,
};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;

/// Core-owned desired configuration from a saved document. Its rows cannot be
/// selected or replaced by a caller. Only snapshot inputs deserialize; core
/// rehydrates every row before granting a runtime plan witness.
#[derive(Debug, Clone)]
pub struct SavedInputsPending;
#[derive(Debug, Clone)]
pub struct SavedInputsResolved;

/// A decoded saved document has no plan-minting authority until core resolves
/// its complete desired configuration and rehydrates its secret placeholders.
///
/// ```compile_fail
/// use carina_core::provider_text::SavedProviderConfiguration;
/// let saved: SavedProviderConfiguration = serde_json::from_str(
///     r#"{"sorted_resources":[],"data_sources":[]}"#).unwrap();
/// let _ = saved.provider_plan();
/// ```
///
/// ```compile_fail
/// use carina_core::provider_text::SavedProviderConfiguration;
/// let saved: SavedProviderConfiguration = serde_json::from_str(
///     r#"{"sorted_resources":[],"data_sources":[]}"#).unwrap();
/// let _ = saved.rehydrate(&Default::default(), &Default::default());
/// ```
#[derive(Debug, Clone)]
pub struct SavedProviderConfiguration<V: ValueKind = SnapshotLeaf, P = SavedInputsPending> {
    phase: std::marker::PhantomData<P>,
    resources: Vec<Resource<SavedModuleSource, V, AbsoluteBindingAddress, ()>>,
    data_sources: Vec<DataSource<SavedModuleSource, V, AbsoluteBindingAddress, ()>>,
}
impl<'de> Deserialize<'de> for SavedProviderConfiguration<SnapshotLeaf> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct SavedDocument {
            sorted_resources:
                Vec<Resource<SavedModuleSource, SnapshotLeaf, AbsoluteBindingAddress, ()>>,
            data_sources:
                Vec<DataSource<SavedModuleSource, SnapshotLeaf, AbsoluteBindingAddress, ()>>,
        }
        let document = SavedDocument::deserialize(deserializer)?;
        Ok(Self {
            phase: std::marker::PhantomData,
            resources: document.sorted_resources,
            data_sources: document.data_sources,
        })
    }
}
impl<V: ValueKind, P> SavedProviderConfiguration<V, P> {
    pub fn resources(&self) -> &[Resource<SavedModuleSource, V, AbsoluteBindingAddress, ()>] {
        &self.resources
    }
    pub fn data_sources(&self) -> &[DataSource<SavedModuleSource, V, AbsoluteBindingAddress, ()>] {
        &self.data_sources
    }
}
impl SavedProviderConfiguration<SnapshotLeaf> {
    /// Retain the same resolved desired metadata as saved-plan execution.
    pub fn resolve_with_schemas(
        self,
        schemas: &crate::schema::SchemaRegistry,
    ) -> Result<
        SavedProviderConfiguration<SnapshotLeaf, SavedInputsResolved>,
        crate::resource::ResolvedResourceError,
    > {
        let resources = self
            .resources
            .into_iter()
            .map(|resource| {
                crate::resource::ResolvedResource::resolve_with_schemas(resource, schemas)
                    .map(crate::resource::ResolvedResource::into_inner)
            })
            .collect::<Result<_, _>>()?;
        Ok(SavedProviderConfiguration {
            phase: std::marker::PhantomData,
            resources,
            data_sources: self.data_sources,
        })
    }
}
impl SavedProviderConfiguration<SnapshotLeaf, SavedInputsResolved> {
    /// Verify and rehydrate the complete saved desired set. Candidate maps can
    /// supply plaintext matching a stored hash; they cannot omit saved rows.
    pub fn rehydrate(
        &self,
        resources: &HashMap<ResourceId, Resource>,
        data_sources: &HashMap<ResourceId, DataSource>,
    ) -> Result<
        SavedProviderConfiguration<NoPlaceholder, SavedInputsResolved>,
        crate::value::SecretRehydrationSiteError,
    > {
        let resources = self
            .resources
            .iter()
            .map(|saved| match resources.get(&saved.id) {
                Some(fresh) => crate::value::rehydrate_resource_secrets(saved, fresh),
                None => saved.to_value_resource().map_err(Into::into),
            })
            .collect::<Result<_, _>>()?;
        let data_sources = self
            .data_sources
            .iter()
            .map(|saved| match data_sources.get(&saved.id) {
                Some(fresh) => crate::value::rehydrate_data_source_secrets(saved, fresh),
                None => saved.to_value_data_source().map_err(Into::into),
            })
            .collect::<Result<_, _>>()?;
        Ok(SavedProviderConfiguration {
            phase: std::marker::PhantomData,
            resources,
            data_sources,
        })
    }
}
impl SavedProviderConfiguration<NoPlaceholder, SavedInputsResolved> {
    pub fn provider_plan(&self) -> crate::provider::ProviderPlan {
        let mut plan = crate::provider::ProviderPlan::state_only();
        for resource in &self.resources {
            plan.include(&resource.id, resource.attributes.values());
        }
        for source in &self.data_sources {
            plan.include(&source.id, source.attributes.values());
        }
        plan
    }
}
