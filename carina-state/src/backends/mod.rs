//! Backend implementations for state storage

mod local;
mod s3;

pub use local::LocalBackend;
pub use s3::S3Backend;

use crate::backend::{BackendConfig, BackendError, BackendResult, StateBackend};

/// Create a backend from configuration
///
/// This function dispatches to the appropriate backend implementation
/// based on the backend_type in the configuration.
pub async fn create_backend(config: &BackendConfig) -> BackendResult<Box<dyn StateBackend>> {
    match config.backend_type.as_str() {
        "s3" => {
            let backend = S3Backend::from_config(config).await?;
            Ok(Box::new(backend))
        }
        "local" => {
            let backend = LocalBackend::from_config(config)?;
            Ok(Box::new(backend))
        }
        // Future backends:
        // "gcs" => Ok(Box::new(GcsBackend::from_config(config)?)),
        // "azure" => Ok(Box::new(AzureBackend::from_config(config)?)),
        other => Err(BackendError::unsupported_backend(other)),
    }
}

/// Create a default local backend
///
/// This is used when no backend is configured in the .crn file.
pub fn create_local_backend() -> Box<dyn StateBackend> {
    Box::new(LocalBackend::new())
}

/// Resolve a backend from an optional parser BackendConfig.
///
/// If a config is provided, converts it to a StateBackendConfig and creates the
/// appropriate backend. If no config is provided, falls back to a local backend.
pub async fn resolve_backend(
    backend_config: Option<&carina_core::parser::BackendConfig>,
) -> BackendResult<Box<dyn StateBackend>> {
    if let Some(config) = backend_config {
        let state_config = BackendConfig::from(config);
        create_backend(&state_config).await
    } else {
        Ok(create_local_backend())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_unsupported_backend() {
        let config = BackendConfig {
            backend_type: "unsupported".to_string(),
            attributes: HashMap::new(),
        };

        let result = create_backend(&config).await;
        assert!(result.is_err());

        if let Err(BackendError::UnsupportedBackend(name)) = result {
            assert_eq!(name, "unsupported");
        } else {
            panic!("Expected UnsupportedBackend error");
        }
    }

    #[tokio::test]
    async fn test_resolve_backend_none_returns_local() {
        let backend = resolve_backend(None).await;
        assert!(backend.is_ok(), "None config should return a local backend");
        // Local backend read_state returns Ok(None) when no state file exists
        let state = backend.unwrap().read_state().await;
        assert!(state.is_ok());
    }

    #[tokio::test]
    async fn test_resolve_backend_invalid_config_returns_error() {
        use carina_core::parser::BackendConfig as ParserBackendConfig;

        let config = ParserBackendConfig {
            backend_type: "unsupported".to_string(),
            attributes: HashMap::new(),
        };

        let result = resolve_backend(Some(&config)).await;
        assert!(result.is_err());

        if let Err(BackendError::UnsupportedBackend(name)) = result {
            assert_eq!(name, "unsupported");
        } else {
            panic!("Expected UnsupportedBackend error");
        }
    }
}
