//! Provider - Trait abstracting resource operations
//!
//! A Provider defines operations for a specific infrastructure (AWS, GCP, etc.).
//! It is responsible for converting Effects into actual API calls.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use crate::resource::{LifecycleConfig, Resource, ResourceId, State, Value};

/// Error type for Provider operations
#[derive(Debug)]
pub struct ProviderError {
    pub message: String,
    pub resource_id: Option<ResourceId>,
    pub cause: Option<Box<dyn std::error::Error + Send + Sync>>,
    pub is_timeout: bool,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ref id) = self.resource_id {
            write!(f, "[{}.{}] {}", id.resource_type, id.name, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for ProviderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_ref()
            .map(|e| e.as_ref() as &dyn std::error::Error)
    }
}

impl ProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            resource_id: None,
            cause: None,
            is_timeout: false,
        }
    }

    pub fn for_resource(mut self, id: ResourceId) -> Self {
        self.resource_id = Some(id);
        self
    }

    pub fn with_cause(mut self, cause: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    pub fn timeout(mut self) -> Self {
        self.is_timeout = true;
        self
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;

/// Return type for async operations
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Definition of resource types that a Provider can handle
pub trait ResourceType: Send + Sync {
    /// Resource type name (e.g., "s3_bucket")
    fn name(&self) -> &'static str;

    /// Attribute schema for this resource type (for future type validation)
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Resource attribute schema (for type validation, to be extended)
#[derive(Debug, Default)]
pub struct ResourceSchema {
    // Attribute type definitions to be added later
}

/// Main Provider trait
///
/// Each infrastructure provider (AWS, GCP, etc.) implements this trait.
/// All operations are async and involve side effects.
pub trait Provider: Send + Sync {
    /// Name of this Provider (e.g., "aws")
    fn name(&self) -> &'static str;

    /// List of resource types this Provider can handle
    fn resource_types(&self) -> Vec<Box<dyn ResourceType>>;

    /// Get the current state of a resource
    ///
    /// If identifier is provided, use it to read the resource directly.
    /// Otherwise, fall back to name-based lookup (for backwards compatibility).
    /// Returns `State::not_found()` if the resource does not exist.
    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>>;

    /// Create a resource
    ///
    /// Returns State with identifier set to the AWS internal ID (e.g., vpc-xxx)
    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>>;

    /// Update a resource
    ///
    /// The identifier is the AWS internal ID (e.g., vpc-xxx)
    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>>;

    /// Delete a resource
    ///
    /// The identifier is the AWS internal ID (e.g., vpc-xxx)
    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>>;

    /// Resolve enum identifiers in resources to their fully-qualified DSL format.
    ///
    /// For example, resolves bare `advanced` or `Tier.advanced` into
    /// `awscc.ec2_ipam.Tier.advanced` based on schema definitions.
    /// Default implementation is a no-op for providers without enum types.
    fn resolve_enum_identifiers(&self, _resources: &mut [Resource]) {}

    /// Restore create-only attributes from saved state into current read states.
    ///
    /// Some APIs (e.g., CloudControl) don't return create-only properties in read responses.
    /// This method carries them forward from previously saved attribute values.
    /// Default implementation is a no-op.
    fn restore_create_only_attrs(
        &self,
        _current_states: &mut HashMap<ResourceId, State>,
        _saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    ) {
    }
}

/// Factory for creating and configuring a Provider.
///
/// Each provider crate implements this trait to encapsulate provider-specific
/// logic (region validation, region extraction, provider instantiation, schemas).
/// The CLI uses factories instead of hardcoded provider name matching.
pub trait ProviderFactory: Send + Sync {
    /// Provider name (e.g., "aws", "awscc")
    fn name(&self) -> &str;

    /// Display name for user-facing messages (e.g., "AWS provider", "AWS Cloud Control provider")
    fn display_name(&self) -> &str;

    /// Validate provider configuration (e.g., region).
    /// Called before provider instantiation.
    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String>;

    /// Extract region from config in SDK format (e.g., "ap-northeast-1").
    /// Returns a default region if none is configured.
    fn extract_region(&self, attributes: &HashMap<String, Value>) -> String;

    /// Extract raw region from config in DSL format (e.g., "aws.Region.ap_northeast_1").
    /// Returns None if no region is configured.
    fn extract_region_dsl(&self, attributes: &HashMap<String, Value>) -> Option<String>;

    /// Create a provider instance from configuration attributes.
    fn create_provider(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn Provider>>;

    /// Get all resource schemas for this provider.
    fn schemas(&self) -> Vec<crate::schema::ResourceSchema>;

    /// Format a schema lookup key from a resource type.
    /// Default: prepends provider name (e.g., "awscc" + "ec2_vpc" → "awscc.ec2_vpc").
    fn format_schema_key(&self, resource_type: &str) -> String {
        format!("{}.{}", self.name(), resource_type)
    }

    /// Attribute names (beyond schema create-only properties) that contribute
    /// to anonymous resource identity. For example, AWS providers return
    /// `["region"]` because the same resource type in different regions must
    /// produce different identifiers.
    fn identity_attributes(&self) -> Vec<&str> {
        vec![]
    }
}

/// Provider implementation for Box<dyn Provider>
/// This enables dynamic dispatch for Providers
impl Provider for Box<dyn Provider> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn resource_types(&self) -> Vec<Box<dyn ResourceType>> {
        (**self).resource_types()
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).read(id, identifier)
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).create(resource)
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).update(id, identifier, from, to)
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        (**self).delete(id, identifier, lifecycle)
    }

    fn resolve_enum_identifiers(&self, resources: &mut [Resource]) {
        (**self).resolve_enum_identifiers(resources)
    }

    fn restore_create_only_attrs(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    ) {
        (**self).restore_create_only_attrs(current_states, saved_attrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mock Provider for testing
    struct MockProvider;

    impl Provider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn resource_types(&self) -> Vec<Box<dyn ResourceType>> {
            vec![]
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            let attrs = resource.attributes.clone();
            Box::pin(async move { Ok(State::existing(id, attrs).with_identifier("mock-id-123")) })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _from: &State,
            to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            let attrs = to.attributes.clone();
            Box::pin(async move { Ok(State::existing(id, attrs)) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _lifecycle: &LifecycleConfig,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn mock_provider_read_returns_not_found() {
        let provider = MockProvider;
        let id = ResourceId::new("test", "example");
        let state = provider.read(&id, None).await.unwrap();
        assert!(!state.exists);
    }

    #[tokio::test]
    async fn mock_provider_create_returns_existing() {
        let provider = MockProvider;
        let resource = Resource::new("test", "example");
        let state = provider.create(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("mock-id-123".to_string()));
    }
}
