//! Backend implementations for state storage

mod s3;

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
        // Future backends:
        // "gcs" => Ok(Box::new(GcsBackend::from_config(config)?)),
        // "azure" => Ok(Box::new(AzureBackend::from_config(config)?)),
        // "local" => Ok(Box::new(LocalBackend::from_config(config)?)),
        other => Err(BackendError::unsupported_backend(other)),
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
}
