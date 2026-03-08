//! Carina AWS Cloud Control Provider
//!
//! AWS Cloud Control API Provider implementation.
//!
//! ## Module Structure
//!
//! - `resources` - Resource type definitions and configurations
//! - `provider` - AwsccProvider implementation
//! - `schemas` - Auto-generated resource schemas

pub mod provider;
pub mod resources;
pub mod schemas;

// Re-export main types
pub use provider::AwsccProvider;

use std::collections::HashMap;

use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};

use resources::resource_types;

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

    fn schemas(&self) -> Vec<carina_core::schema::ResourceSchema> {
        schemas::all_schemas()
    }

    fn identity_attributes(&self) -> Vec<&str> {
        vec!["region"]
    }

    fn region_completions(&self) -> Vec<carina_core::schema::CompletionValue> {
        use carina_core::schema::CompletionValue;
        let regions = [
            ("ap_northeast_1", "Asia Pacific (Tokyo)"),
            ("ap_northeast_2", "Asia Pacific (Seoul)"),
            ("ap_northeast_3", "Asia Pacific (Osaka)"),
            ("ap_south_1", "Asia Pacific (Mumbai)"),
            ("ap_southeast_1", "Asia Pacific (Singapore)"),
            ("ap_southeast_2", "Asia Pacific (Sydney)"),
            ("ca_central_1", "Canada (Central)"),
            ("eu_central_1", "Europe (Frankfurt)"),
            ("eu_west_1", "Europe (Ireland)"),
            ("eu_west_2", "Europe (London)"),
            ("eu_west_3", "Europe (Paris)"),
            ("eu_north_1", "Europe (Stockholm)"),
            ("sa_east_1", "South America (Sao Paulo)"),
            ("us_east_1", "US East (N. Virginia)"),
            ("us_east_2", "US East (Ohio)"),
            ("us_west_1", "US West (N. California)"),
            ("us_west_2", "US West (Oregon)"),
        ];
        regions
            .iter()
            .map(|(code, name)| CompletionValue::new(format!("awscc.Region.{}", code), *name))
            .collect()
    }
}

// =============================================================================
// Provider Trait Implementation
// =============================================================================

impl Provider for AwsccProvider {
    fn name(&self) -> &'static str {
        "awscc"
    }

    fn resource_types(&self) -> Vec<Box<dyn carina_core::provider::ResourceType>> {
        resource_types()
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
        _from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let to = to.clone();
        Box::pin(async move { self.update_resource(id, &identifier, to).await })
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

    fn resolve_enum_identifiers(&self, resources: &mut [Resource]) {
        crate::provider::resolve_enum_identifiers_impl(resources);
    }

    fn restore_unreturned_attrs(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    ) {
        crate::provider::restore_unreturned_attrs_impl(current_states, saved_attrs);
    }
}
