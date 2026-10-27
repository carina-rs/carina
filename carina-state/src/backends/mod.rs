//! Backend implementations for state storage

mod local;
mod s3;
#[cfg(test)]
mod s3_winterbaume_tests;
mod url;

pub use local::LocalBackend;
pub use s3::S3Backend;
pub use url::{StateUrl, load_published_exports_from_url, load_state_from_url};

use std::path::Path;

use crate::backend::{BackendResult, StateBackend};
use crate::backend_config::{BackendConfig, LocalBackendSettings};
use crate::backend_selection::{BackendSelection, BackendTarget, BucketDeleteBackendSelection};

/// Create the backend named by an opaque, operation-specific selection token.
///
/// Raw backend configuration is deliberately not accepted. Callers must first
/// choose one of the named selection paths on [`BackendSelection`].
pub async fn create_backend(selection: &BackendSelection) -> BackendResult<Box<dyn StateBackend>> {
    match selection.target() {
        BackendTarget::Configured { config, base_dir } => match config {
            Some(config) => create_configured_backend(config, base_dir).await,
            None => Ok(Box::new(LocalBackend::with_path(
                base_dir.join(LocalBackend::DEFAULT_STATE_FILE),
            ))),
        },
        BackendTarget::ExplicitUrl(StateUrl::S3 { bucket, key }) => Ok(Box::new(
            S3Backend::from_url_parts(bucket.clone(), key.clone()).await?,
        )),
        BackendTarget::ExplicitUrl(StateUrl::File { path }) => {
            Ok(Box::new(LocalBackend::with_path(path.clone())))
        }
    }
}

async fn create_configured_backend(
    config: &BackendConfig,
    base_dir: &std::path::Path,
) -> BackendResult<Box<dyn StateBackend>> {
    match config {
        BackendConfig::S3(settings) => {
            let backend = S3Backend::from_config(settings).await?;
            Ok(Box::new(backend))
        }
        BackendConfig::Local(settings) => {
            let backend = LocalBackend::from_config(settings, base_dir)?;
            Ok(Box::new(backend))
        }
    }
}

/// Create the S3 backend authorized by a bucket-delete selection.
///
/// The narrow token proves that the requested bucket matched the configured
/// bucket before backend construction became possible.
pub async fn create_remote_backend(
    selection: &BucketDeleteBackendSelection,
) -> BackendResult<Box<dyn StateBackend>> {
    Ok(Box::new(
        S3Backend::from_config(selection.s3_settings()).await?,
    ))
}

/// Resolve a local backend's state file path, anchoring a relative
/// `path` (or the unset default) at `base_dir`.
pub fn anchored_local_path(config: &LocalBackendSettings, base_dir: &Path) -> std::path::PathBuf {
    let path = config
        .path()
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
    use crate::BackendError;
    use crate::backend_config::BackendConfigError;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_unsupported_backend() {
        let config = carina_core::parser::BackendConfig {
            backend_type: "unsupported".to_string(),
            attributes: HashMap::new(),
            attribute_spans: HashMap::new(),
        };

        let error = BackendConfig::try_from(&config).unwrap_err();
        let error = BackendError::from(error);
        assert_eq!(error.to_string(), "Unsupported backend type: unsupported");
        assert!(matches!(
            error,
            BackendError::InvalidConfig(BackendConfigError::UnsupportedBackend(name))
                if name == "unsupported"
        ));
    }
}
