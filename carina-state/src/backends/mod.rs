//! Backend implementations for state storage

mod local;
mod s3;
#[cfg(test)]
mod s3_winterbaume_tests;
mod url;

pub use local::LocalBackend;
pub use s3::S3Backend;
pub use url::{StateUrl, load_published_exports_from_url, load_state_from_url};

use crate::backend::BackendError;
use carina_core::paths::ResolvedPath;

use crate::backend::{BackendResult, StateBackend};
use crate::backend_config::{BackendConfig, LocalBackendSettings};
use crate::backend_selection::{BackendSelection, BackendTarget};

/// Provider name reported for the storage resource (the state bucket).
pub(crate) const S3_BACKEND_PROVIDER_NAME: &str = "aws";

/// Resource type used to surface the state bucket in Carina state.
pub(crate) const S3_BACKEND_RESOURCE_TYPE: &str = "s3.Bucket";

/// Create the backend named by an opaque, operation-specific selection token.
///
/// Raw backend configuration is deliberately not accepted. Callers must first
/// choose one of the named selection paths on [`BackendSelection`].
pub(crate) async fn create_backend(
    selection: &BackendSelection,
) -> BackendResult<Box<dyn StateBackend>> {
    match selection.target() {
        BackendTarget::Configured { config, base_dir } => match config {
            Some(config) => create_configured_backend(config, base_dir).await,
            None => Ok(Box::new(LocalBackend::with_resolved_path(
                base_dir
                    .join(LocalBackend::DEFAULT_STATE_FILE)
                    .map_err(|error| BackendError::Io(error.to_string()))?,
            )?)),
        },
        BackendTarget::ExplicitUrl(StateUrl::S3 { bucket, key }) => Ok(Box::new(
            S3Backend::from_url_parts(bucket.clone(), key.clone()).await?,
        )),
        BackendTarget::ExplicitUrl(StateUrl::File { path }) => {
            Ok(Box::new(LocalBackend::with_resolved_path(
                ResolvedPath::new(path).map_err(|error| BackendError::Io(error.to_string()))?,
            )?))
        }
    }
}

/// Construct a local backend for tests that need to exercise the full backend
/// contract without weakening production selection capabilities.
#[cfg(any(test, feature = "test-utils"))]
pub fn local_backend_for_test(state_path: impl Into<std::path::PathBuf>) -> Box<dyn StateBackend> {
    Box::new(LocalBackend::with_path(state_path.into()))
}

async fn create_configured_backend(
    config: &BackendConfig,
    base_dir: &ResolvedPath,
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

/// Resolve a local backend's state file path, anchoring a relative
/// `path` (or the unset default) at `base_dir`.
pub fn anchored_local_path(
    config: &LocalBackendSettings,
    base_dir: &ResolvedPath,
) -> std::io::Result<ResolvedPath> {
    base_dir.join(config.path().unwrap_or(LocalBackend::DEFAULT_STATE_FILE))
}
