//! Backend implementations for state storage

mod local;
mod s3;
mod url;

pub use local::LocalBackend;
pub use s3::S3Backend;
pub use url::{StateUrl, load_state_from_url};

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

/// Resolve a backend from an optional parser BackendConfig for read-only use.
///
/// If a config is provided, converts it to a StateBackendConfig and creates the
/// appropriate backend. If no config is provided, falls back to a local backend.
pub async fn resolve_backend_for_read(
    backend_config: Option<&carina_core::parser::BackendConfig>,
) -> BackendResult<Box<dyn StateBackend>> {
    if let Some(config) = backend_config {
        let state_config = BackendConfig::from(config);
        create_backend(&state_config).await
    } else {
        Ok(create_local_backend())
    }
}

/// Resolve a backend, anchoring a local backend's relative state `path`
/// (or the unset default) at `base_dir` instead of the process CWD.
///
/// `resolve_backend_for_read` / `create_backend` treat a local `path` as
/// CWD-relative, which is correct only when the command is run from the
/// project directory. Commands that accept an explicit directory
/// argument (`carina init <dir>`, upstream `remote_state` resolution)
/// must reach the state next to *that* directory's `.crn` files
/// regardless of where the binary was invoked from — they use this.
/// Remote backends carry their own absolute address and are delegated
/// unchanged to `create_backend`.
pub async fn resolve_backend_anchored(
    backend_config: Option<&BackendConfig>,
    base_dir: &std::path::Path,
) -> BackendResult<Box<dyn StateBackend>> {
    match backend_config {
        Some(config) if config.is_local() => Ok(Box::new(LocalBackend::with_path(
            anchored_local_path(config, base_dir),
        ))),
        Some(config) => create_backend(config).await,
        None => Ok(Box::new(LocalBackend::with_path(
            base_dir.join(LocalBackend::DEFAULT_STATE_FILE),
        ))),
    }
}

/// Resolve a local backend's state file path, anchoring a relative
/// `path` (or the unset default) at `base_dir`.
pub fn anchored_local_path(
    config: &BackendConfig,
    base_dir: &std::path::Path,
) -> std::path::PathBuf {
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

        let result = create_backend(&config).await;
        assert!(result.is_err());

        if let Err(BackendError::UnsupportedBackend(name)) = result {
            assert_eq!(name, "unsupported");
        } else {
            panic!("Expected UnsupportedBackend error");
        }
    }

    #[tokio::test]
    async fn test_resolve_backend_for_read_none_returns_local() {
        let backend = resolve_backend_for_read(None).await;
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

        let result = resolve_backend_for_read(Some(&config)).await;
        assert!(result.is_err());

        if let Err(BackendError::UnsupportedBackend(name)) = result {
            assert_eq!(name, "unsupported");
        } else {
            panic!("Expected UnsupportedBackend error");
        }
    }
}
