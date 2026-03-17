//! Carina AWS Cloud Control Provider
//!
//! AWS Cloud Control API Provider implementation.
//!
//! ## Module Structure
//!
//! - `resources` - Schema configuration tests
//! - `provider` - AwsccProvider implementation
//! - `schemas` - Auto-generated resource schemas

pub mod provider;
pub mod resources;
pub mod schemas;

// Re-export main types
pub use provider::AwsccProvider;

use std::collections::HashMap;

use carina_core::provider::{
    BoxFuture, Provider, ProviderFactory, ProviderResult, ProviderSchemaExt, SavedAttrs,
};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};

/// Schema extension for the AWSCC provider.
///
/// Handles plan-time normalization of enum identifiers and hydration of
/// unreturned attributes from saved state.
pub struct AwsccSchemaExt;

impl ProviderSchemaExt for AwsccSchemaExt {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        crate::provider::resolve_enum_identifiers_impl(resources);
    }

    fn hydrate_read_state(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &SavedAttrs,
    ) {
        crate::provider::restore_unreturned_attrs_impl(current_states, saved_attrs);
    }
}

/// Factory for creating and configuring the AWSCC Provider
pub struct AwsccProviderFactory;

impl ProviderFactory for AwsccProviderFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWS Cloud Control provider"
    }

    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String> {
        let region_type = schemas::awscc_types::awscc_region();
        if let Some(region_value) = attributes.get("region") {
            region_type
                .validate(region_value)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn extract_region(&self, attributes: &HashMap<String, Value>) -> String {
        if let Some(Value::String(region)) = attributes.get("region") {
            if let Some(rest) = region.strip_prefix("awscc.Region.") {
                return rest.replace('_', "-");
            }
            if let Some(rest) = region.strip_prefix("aws.Region.") {
                return rest.replace('_', "-");
            }
            return region.clone();
        }
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn Provider>> {
        let region = self.extract_region(attributes);
        Box::pin(async move { Box::new(AwsccProvider::new(&region).await) as Box<dyn Provider> })
    }

    fn create_schema_ext(
        &self,
        _attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Option<Box<dyn ProviderSchemaExt>>> {
        Box::pin(async { Some(Box::new(AwsccSchemaExt) as Box<dyn ProviderSchemaExt>) })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ResourceSchema> {
        schemas::all_schemas()
    }

    fn identity_attributes(&self) -> Vec<&str> {
        vec!["region"]
    }

    fn region_completions(&self) -> Vec<carina_core::schema::CompletionValue> {
        carina_aws_types::region_completions("awscc")
    }
}

// =============================================================================
// Provider Trait Implementation
// =============================================================================

impl Provider for AwsccProvider {
    fn name(&self) -> &'static str {
        "awscc"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.map(|s| s.to_string());
        Box::pin(async move {
            self.read_resource(&id.resource_type, &id.name, identifier.as_deref())
                .await
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        Box::pin(async move { self.create_resource(resource).await })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let from = from.clone();
        let to = to.clone();
        Box::pin(async move { self.update_resource(id, &identifier, &from, to).await })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let lifecycle = lifecycle.clone();
        Box::pin(async move { self.delete_resource(&id, &identifier, &lifecycle).await })
    }
}

impl ProviderSchemaExt for AwsccProvider {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        crate::provider::resolve_enum_identifiers_impl(resources);
    }

    fn hydrate_read_state(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &SavedAttrs,
    ) {
        crate::provider::restore_unreturned_attrs_impl(current_states, saved_attrs);
    }
}
