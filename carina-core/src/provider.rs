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
            write!(f, "[{}.{}] {}", id.resource_type, id.name, self.message)?;
        } else {
            write!(f, "{}", self.message)?;
        }
        if let Some(ref cause) = self.cause {
            write!(f, ": {}", cause)?;
        }
        Ok(())
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

/// Runtime CRUD operations for a provider.
///
/// Each infrastructure provider (AWS, GCP, etc.) implements this trait
/// to define how resources are read, created, updated, and deleted.
pub trait ProviderRuntime: Send + Sync {
    /// Name of this Provider (e.g., "aws")
    fn name(&self) -> &'static str;

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
}

/// Plan-time schema extensions for a provider.
///
/// Handles normalization of desired state and hydration of read state.
/// These operations are synchronous and run before/after CRUD operations.
pub trait ProviderSchemaExt: Send + Sync {
    /// Normalize desired resources before diffing.
    ///
    /// For example, resolves bare enum identifiers like `advanced` or `Tier.advanced`
    /// into fully-qualified DSL format like `awscc.ec2_ipam.Tier.advanced`.
    /// Default implementation is a no-op for providers without enum types.
    fn normalize_desired(&self, _resources: &mut [Resource]) {}

    /// Hydrate read state with saved attributes that the API did not return.
    ///
    /// Some APIs (e.g., CloudControl) don't return certain properties in read responses
    /// (create-only properties, or normal properties like `description` on some resources).
    /// This method carries them forward from previously saved attribute values.
    /// Default implementation is a no-op.
    fn hydrate_read_state(
        &self,
        _current_states: &mut HashMap<ResourceId, State>,
        _saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    ) {
    }
}

/// Main Provider trait combining runtime operations and schema extensions.
///
/// This is a supertrait of `ProviderRuntime` and `ProviderSchemaExt`.
/// Providers that implement both traits automatically implement `Provider`.
pub trait Provider: ProviderRuntime + ProviderSchemaExt {}

/// Blanket implementation: any type implementing both sub-traits is a Provider.
impl<T: ProviderRuntime + ProviderSchemaExt> Provider for T {}

/// A provider that routes operations to the correct sub-provider
/// based on the resource's provider name (`ResourceId.provider`).
pub struct ProviderRouter {
    providers: HashMap<String, Box<dyn Provider>>,
}

impl Default for ProviderRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRouter {
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

impl ProviderRuntime for ProviderRouter {
    fn name(&self) -> &'static str {
        "router"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        match self.get_provider_or_error(&id.provider) {
            Ok(provider) => provider.read(id, identifier),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        match self.get_provider_or_error(&resource.id.provider) {
            Ok(provider) => provider.create(resource),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        match self.get_provider_or_error(&id.provider) {
            Ok(provider) => provider.update(id, identifier, from, to),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        match self.get_provider_or_error(&id.provider) {
            Ok(provider) => provider.delete(id, identifier, lifecycle),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
}

impl ProviderSchemaExt for ProviderRouter {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        for provider in self.providers.values() {
            provider.normalize_desired(resources);
        }
    }

    fn hydrate_read_state(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    ) {
        for provider in self.providers.values() {
            provider.hydrate_read_state(current_states, saved_attrs);
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
    if resource.id.provider.is_empty() {
        return resource.id.resource_type.clone();
    }
    if let Some(factory) = find_factory(factories, &resource.id.provider) {
        factory.format_schema_key(&resource.id.resource_type)
    } else {
        resource.id.resource_type.clone()
    }
}

/// ProviderRuntime implementation for Box<dyn Provider>
/// This enables dynamic dispatch for Providers
impl ProviderRuntime for Box<dyn Provider> {
    fn name(&self) -> &'static str {
        (**self).name()
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
}

impl ProviderSchemaExt for Box<dyn Provider> {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        (**self).normalize_desired(resources)
    }

    fn hydrate_read_state(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    ) {
        (**self).hydrate_read_state(current_states, saved_attrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mock Provider for testing
    struct MockProvider;

    impl ProviderRuntime for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
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

    impl ProviderSchemaExt for MockProvider {}

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
    async fn provider_router_dispatches_read_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider("mock", "test", "example");
        let state = router.read(&id, None).await.unwrap();
        assert!(!state.exists);
    }

    #[tokio::test]
    async fn provider_router_dispatches_create_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let resource = Resource::with_provider("mock", "test", "example");
        let state = router.create(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("mock-id-123".to_string()));
    }

    #[test]
    fn provider_error_source_returns_cause() {
        use std::error::Error;
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::new("Failed to create resource").with_cause(cause);
        let source = err.source().expect("source should be Some");
        assert_eq!(source.to_string(), "connection refused");
    }

    #[test]
    fn provider_error_display_includes_cause() {
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::new("Failed to create resource").with_cause(cause);
        let display = format!("{}", err);
        assert!(
            display.contains("connection refused"),
            "Display should include cause message, got: {}",
            display
        );
    }

    #[test]
    fn provider_error_display_without_cause() {
        let err = ProviderError::new("simple error");
        let display = format!("{}", err);
        assert_eq!(display, "simple error");
    }

    #[test]
    fn provider_error_display_with_resource_id_and_cause() {
        let cause = std::io::Error::other("timeout");
        let id = ResourceId::new("s3.bucket", "my-bucket");
        let err = ProviderError::new("Failed to read")
            .with_cause(cause)
            .for_resource(id);
        let display = format!("{}", err);
        assert!(
            display.contains("timeout"),
            "Display should include cause message, got: {}",
            display
        );
        assert!(
            display.contains("s3.bucket"),
            "Display should include resource type, got: {}",
            display
        );
    }

    #[tokio::test]
    async fn provider_router_dispatches_update_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider("mock", "test", "example");
        let from = State::existing(id.clone(), HashMap::new());
        let to = Resource::with_provider("mock", "test", "example");
        let state = router.update(&id, "mock-id-123", &from, &to).await.unwrap();
        assert!(state.exists);
    }

    #[tokio::test]
    async fn provider_router_dispatches_delete_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider("mock", "test", "example");
        let lifecycle = LifecycleConfig::default();
        let result = router.delete(&id, "mock-id-123", &lifecycle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn provider_router_returns_error_for_unknown_provider() {
        let router = ProviderRouter::new();
        let id = ResourceId::with_provider("nonexistent", "test", "example");
        let result = router.read(&id, None).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("Unknown provider: nonexistent")
        );
    }

    // Mock ProviderFactory for testing schema_key_for_resource
    struct MockProviderFactory;

    impl ProviderFactory for MockProviderFactory {
        fn name(&self) -> &str {
            "mock"
        }

        fn display_name(&self) -> &str {
            "Mock provider"
        }

        fn validate_config(&self, _attributes: &HashMap<String, Value>) -> Result<(), String> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &HashMap<String, Value>) -> String {
            "us-east-1".to_string()
        }

        fn create_provider(
            &self,
            _attributes: &HashMap<String, Value>,
        ) -> BoxFuture<'_, Box<dyn Provider>> {
            Box::pin(async { Box::new(MockProvider) as Box<dyn Provider> })
        }

        fn schemas(&self) -> Vec<crate::schema::ResourceSchema> {
            vec![]
        }
    }

    // --- Tests for ProviderRuntime + ProviderSchemaExt trait split ---

    struct SplitMockProvider;

    impl ProviderRuntime for SplitMockProvider {
        fn name(&self) -> &'static str {
            "split_mock"
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
            Box::pin(async move { Ok(State::existing(id, attrs).with_identifier("split-id")) })
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

    impl ProviderSchemaExt for SplitMockProvider {
        fn normalize_desired(&self, resources: &mut [Resource]) {
            // Mock: uppercase all string attribute values
            for r in resources.iter_mut() {
                if r.id.provider != "split_mock" {
                    continue;
                }
                for val in r.attributes.values_mut() {
                    if let Value::String(s) = val {
                        *s = s.to_uppercase();
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn split_traits_can_be_implemented_separately() {
        let provider = SplitMockProvider;
        // ProviderRuntime methods work
        let id = ResourceId::new("test", "example");
        let state = provider.read(&id, None).await.unwrap();
        assert!(!state.exists);

        let resource = Resource::new("test", "example");
        let state = provider.create(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("split-id".to_string()));
    }

    #[test]
    fn split_traits_normalize_desired_works() {
        let provider = SplitMockProvider;
        let mut resources = vec![
            Resource::with_provider("split_mock", "test", "example")
                .with_attribute("key", Value::String("hello".to_string())),
        ];
        provider.normalize_desired(&mut resources);
        assert_eq!(
            resources[0].attributes.get("key"),
            Some(&Value::String("HELLO".to_string()))
        );
    }

    #[test]
    fn split_traits_hydrate_read_state_default_is_noop() {
        let provider = SplitMockProvider;
        let id = ResourceId::new("test", "example");
        let mut states = HashMap::new();
        states.insert(id.clone(), State::existing(id.clone(), HashMap::new()));
        let saved = HashMap::new();
        // Default implementation should not panic
        provider.hydrate_read_state(&mut states, &saved);
    }

    #[test]
    fn provider_supertrait_requires_both() {
        // SplitMockProvider implements both ProviderRuntime and ProviderSchemaExt,
        // so it should satisfy the Provider supertrait bound.
        fn assert_provider<T: Provider>(_p: &T) {}
        let provider = SplitMockProvider;
        assert_provider(&provider);
    }

    #[test]
    fn schema_key_for_resource_uses_id_provider_not_attribute() {
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(MockProviderFactory)];

        // Resource with id.provider set but NO _provider attribute
        let resource = Resource::with_provider("mock", "s3.bucket", "my-bucket");
        assert!(!resource.attributes.contains_key("_provider"));

        let key = schema_key_for_resource(&factories, &resource);
        assert_eq!(key, "mock.s3.bucket");
    }
}
