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
    /// Returns `State::not_found()` if no identifier is provided or the resource does not exist.
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

    /// Restore unreturned attributes from saved state into current read states.
    ///
    /// Some APIs (e.g., CloudControl) don't return certain properties in read responses
    /// (create-only properties, or normal properties like `description` on some resources).
    /// This method carries them forward from previously saved attribute values.
    /// Default implementation is a no-op.
    fn restore_unreturned_attrs(
        &self,
        _current_states: &mut HashMap<ResourceId, State>,
        _saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    ) {
    }
}

/// A provider that dispatches operations to the correct sub-provider
/// based on the resource's provider name (`ResourceId.provider`).
pub struct MultiProvider {
    providers: HashMap<String, Box<dyn Provider>>,
}

impl Default for MultiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiProvider {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn add_provider(&mut self, name: String, provider: Box<dyn Provider>) {
        self.providers.insert(name, provider);
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    fn get_provider_or_error(&self, provider_name: &str) -> ProviderResult<&dyn Provider> {
        self.providers
            .get(provider_name)
            .map(|p| p.as_ref())
            .ok_or_else(|| ProviderError::new(format!("Unknown provider: {}", provider_name)))
    }
}

impl Provider for MultiProvider {
    fn name(&self) -> &'static str {
        "multi"
    }

    fn resource_types(&self) -> Vec<Box<dyn ResourceType>> {
        self.providers
            .values()
            .flat_map(|p| p.resource_types())
            .collect()
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.map(|s| s.to_string());
        Box::pin(async move {
            let provider = self.get_provider_or_error(&id.provider)?;
            provider.read(&id, identifier.as_deref()).await
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        Box::pin(async move {
            let provider = self.get_provider_or_error(&resource.id.provider)?;
            provider.create(&resource).await
        })
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
        Box::pin(async move {
            let provider = self.get_provider_or_error(&id.provider)?;
            provider.update(&id, &identifier, &from, &to).await
        })
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
        Box::pin(async move {
            let provider = self.get_provider_or_error(&id.provider)?;
            provider.delete(&id, &identifier, &lifecycle).await
        })
    }

    fn resolve_enum_identifiers(&self, resources: &mut [Resource]) {
        for provider in self.providers.values() {
            provider.resolve_enum_identifiers(resources);
        }
    }

    fn restore_unreturned_attrs(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    ) {
        for provider in self.providers.values() {
            provider.restore_unreturned_attrs(current_states, saved_attrs);
        }
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

    /// Region completions for this provider.
    /// Returns a list of `CompletionValue` with DSL-format region values
    /// (e.g., "aws.Region.ap_northeast_1") and display names.
    fn region_completions(&self) -> Vec<crate::schema::CompletionValue> {
        vec![]
    }
}

/// Find a factory by provider name.
pub fn find_factory<'a>(
    factories: &'a [Box<dyn ProviderFactory>],
    name: &str,
) -> Option<&'a dyn ProviderFactory> {
    factories
        .iter()
        .find(|f| f.name() == name)
        .map(|f| f.as_ref())
}

/// Collect all resource schemas from the given factories into a single map.
pub fn collect_schemas(
    factories: &[Box<dyn ProviderFactory>],
) -> HashMap<String, crate::schema::ResourceSchema> {
    let mut all_schemas = HashMap::new();
    for factory in factories {
        for schema in factory.schemas() {
            all_schemas.insert(schema.resource_type.clone(), schema);
        }
    }
    all_schemas
}

/// Determine the schema lookup key for a resource based on its provider.
pub fn schema_key_for_resource(
    factories: &[Box<dyn ProviderFactory>],
    resource: &Resource,
) -> String {
    match resource.attributes.get("_provider") {
        Some(Value::String(provider)) => {
            if let Some(factory) = find_factory(factories, provider) {
                factory.format_schema_key(&resource.id.resource_type)
            } else {
                resource.id.resource_type.clone()
            }
        }
        _ => resource.id.resource_type.clone(),
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

    fn restore_unreturned_attrs(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    ) {
        (**self).restore_unreturned_attrs(current_states, saved_attrs)
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

    #[tokio::test]
    async fn multi_provider_dispatches_read_by_provider_name() {
        let mut multi = MultiProvider::new();
        multi.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider("mock", "test", "example");
        let state = multi.read(&id, None).await.unwrap();
        assert!(!state.exists);
    }

    #[tokio::test]
    async fn multi_provider_dispatches_create_by_provider_name() {
        let mut multi = MultiProvider::new();
        multi.add_provider("mock".to_string(), Box::new(MockProvider));

        let resource = Resource::with_provider("mock", "test", "example");
        let state = multi.create(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("mock-id-123".to_string()));
    }

    #[tokio::test]
    async fn multi_provider_returns_error_for_unknown_provider() {
        let multi = MultiProvider::new();
        let id = ResourceId::with_provider("nonexistent", "test", "example");
        let result = multi.read(&id, None).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("Unknown provider: nonexistent")
        );
    }
}
