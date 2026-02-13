//! Carina AWS Cloud Control Provider
//!
//! AWS Cloud Control API Provider implementation.
//!
//! ## Module Structure
//!
//! - `resources` - Resource type definitions and configurations
//! - `provider` - AwsccProvider implementation
//! - `schemas` - Auto-generated resource schemas
//! - `utils` - Helper functions for value normalization

pub mod provider;
pub mod resources;
pub mod schemas;
pub mod utils;

// Re-export main types
pub use provider::AwsccProvider;
pub use utils::{
    convert_enum_value, normalize_availability_zone, normalize_instance_tenancy, normalize_region,
};

use carina_core::provider::{BoxFuture, Provider, ProviderResult};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State};

use resources::resource_types;

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
}
