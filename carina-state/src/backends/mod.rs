//! Backend implementations for state storage

mod local;
mod s3;
mod url;

pub use local::LocalBackend;
pub use s3::S3Backend;
pub use url::{StateUrl, load_state_from_url};

use std::path::Path;

use crate::backend::{BackendConfig, BackendError, BackendResult, StateBackend};

/// Create a backend from configuration, anchoring local state paths at `base_dir`.
///
/// This function dispatches to the appropriate backend implementation
/// based on the backend_type in the configuration. Relative local backend
/// paths, including the default when no backend is configured, are resolved
/// relative to `base_dir`.
pub async fn create_backend(
    config: Option<&BackendConfig>,
    base_dir: &Path,
) -> BackendResult<Box<dyn StateBackend>> {
    match config {
        Some(config) => create_configured_backend(config, base_dir).await,
        None => Ok(Box::new(LocalBackend::with_path(
            base_dir.join(LocalBackend::DEFAULT_STATE_FILE),
        ))),
    }
}

async fn create_configured_backend(
    config: &BackendConfig,
    base_dir: &Path,
) -> BackendResult<Box<dyn StateBackend>> {
    match config.backend_type.as_str() {
        "s3" => {
            let backend = S3Backend::from_config(config).await?;
            Ok(Box::new(backend))
        }
        "local" => {
            let backend = LocalBackend::from_config(config, base_dir)?;
            Ok(Box::new(backend))
        }
        // Future backends:
        // "gcs" => Ok(Box::new(GcsBackend::from_config(config)?)),
        // "azure" => Ok(Box::new(AzureBackend::from_config(config)?)),
        other => Err(BackendError::unsupported_backend(other)),
    }
}

/// Create a non-local backend from configuration.
///
/// Use this for metadata-only paths, such as backend bucket management, where
/// the operation is meaningful only for remote backends and should reject a
/// local backend instead of constructing one.
pub async fn create_remote_backend(config: &BackendConfig) -> BackendResult<Box<dyn StateBackend>> {
    if config.is_local() {
        return Err(BackendError::Configuration(
            "Local backend does not manage a remote state bucket".to_string(),
        ));
    }
    create_configured_backend(config, Path::new(".")).await
}

/// Resolve a backend from an optional parser BackendConfig for read-only use.
///
/// If a config is provided, converts it to a StateBackendConfig and creates the
/// appropriate backend. If no config is provided, falls back to a local backend.
pub async fn resolve_backend_for_read(
    backend_config: Option<&carina_core::parser::BackendConfig>,
    base_dir: &Path,
) -> BackendResult<Box<dyn StateBackend>> {
    if let Some(config) = backend_config {
        let state_config = BackendConfig::from(config);
        create_backend(Some(&state_config), base_dir).await
    } else {
        create_backend(None, base_dir).await
    }
}

/// Resolve a local backend's state file path, anchoring a relative
/// `path` (or the unset default) at `base_dir`.
pub fn anchored_local_path(config: &BackendConfig, base_dir: &Path) -> std::path::PathBuf {
    let path = config
        .get_string("path")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(LocalBackend::DEFAULT_STATE_FILE));
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
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

        let result = create_backend(Some(&config), Path::new(".")).await;
        assert!(result.is_err());

        if let Err(BackendError::UnsupportedBackend(name)) = result {
            assert_eq!(name, "unsupported");
        } else {
            panic!("Expected UnsupportedBackend error");
        }
    }

    #[tokio::test]
    async fn test_resolve_backend_for_read_none_returns_local() {
        let base_dir = tempfile::tempdir().unwrap();
        let backend = resolve_backend_for_read(None, base_dir.path()).await;
        assert!(backend.is_ok(), "None config should return a local backend");
        // Local backend read_state returns Ok(None) when no state file exists
        let state = backend.unwrap().read_state().await;
        assert!(state.is_ok());
    }

    #[tokio::test]
    async fn test_resolve_backend_for_read_invalid_config_returns_error() {
        use carina_core::parser::BackendConfig as ParserBackendConfig;

        let config = ParserBackendConfig {
            backend_type: "unsupported".to_string(),
            attributes: HashMap::new(),
        };

        let result = resolve_backend_for_read(Some(&config), Path::new(".")).await;
        assert!(result.is_err());

        if let Err(BackendError::UnsupportedBackend(name)) = result {
            assert_eq!(name, "unsupported");
        } else {
            panic!("Expected UnsupportedBackend error");
        }
    }
}
