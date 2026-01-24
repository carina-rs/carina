//! Provider - Trait abstracting resource operations
//!
//! A Provider defines operations for a specific infrastructure (AWS, GCP, etc.).
//! It is responsible for converting Effects into actual API calls.

use std::future::Future;
use std::pin::Pin;

use crate::resource::{Resource, ResourceId, State};

/// Error type for Provider operations
#[derive(Debug)]
pub struct ProviderError {
    pub message: String,
    pub resource_id: Option<ResourceId>,
    pub cause: Option<Box<dyn std::error::Error + Send + Sync>>,
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
        self.cause.as_ref().map(|e| e.as_ref() as &dyn std::error::Error)
    }
}

impl ProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            resource_id: None,
            cause: None,
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
    /// Returns `State::not_found()` if the resource does not exist
    fn read(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<State>>;

    /// Create a resource
    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>>;

    /// Update a resource
    fn update(&self, id: &ResourceId, from: &State, to: &Resource)
        -> BoxFuture<'_, ProviderResult<State>>;

    /// Delete a resource
    fn delete(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<()>>;
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

    fn read(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).read(id)
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).create(resource)
    }

    fn update(
        &self,
        id: &ResourceId,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).update(id, from, to)
    }

    fn delete(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<()>> {
        (**self).delete(id)
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

        fn read(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            let attrs = resource.attributes.clone();
            Box::pin(async move { Ok(State::existing(id, attrs)) })
        }

        fn update(
            &self,
            id: &ResourceId,
            _from: &State,
            to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            let attrs = to.attributes.clone();
            Box::pin(async move { Ok(State::existing(id, attrs)) })
        }

        fn delete(&self, _id: &ResourceId) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn mock_provider_read_returns_not_found() {
        let provider = MockProvider;
        let id = ResourceId::new("test", "example");
        let state = provider.read(&id).await.unwrap();
        assert!(!state.exists);
    }

    #[tokio::test]
    async fn mock_provider_create_returns_existing() {
        let provider = MockProvider;
        let resource = Resource::new("test", "example");
        let state = provider.create(&resource).await.unwrap();
        assert!(state.exists);
    }
}
