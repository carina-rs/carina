//! Backend implementations for state storage

mod local;
mod s3;
#[cfg(test)]
mod s3_winterbaume_tests;
mod url;

pub use local::LocalBackend;
pub use s3::{S3AutoCreate, S3AutoCreateDisabled, S3Backend, S3Bootstrap, S3BootstrapInfo};
pub use url::{
    LocalStateFile, StateUrl, load_encoded_published_exports_from_url, load_local_state_file,
    load_state_from_url,
};

use carina_core::paths::ResolvedPath;

use crate::backend::{BackendResult, StateBackend};
use crate::backend_config::LocalBackendSettings;
use crate::backend_selection::ConfiguredBackend;

/// Provider name reported for the storage resource (the state bucket).
pub(crate) const S3_BACKEND_PROVIDER_NAME: &str = "aws";

/// Resource type used to surface the state bucket in Carina state.
pub(crate) const S3_BACKEND_RESOURCE_TYPE: &str = "s3.Bucket";

/// Create the backend named by an opaque, operation-specific selection token.
///
/// Raw backend configuration is deliberately not accepted. Callers must first
/// choose one of the named selection paths on [`crate::BackendSelection`].
pub(crate) async fn create_backend(
    selection: &ConfiguredBackend,
) -> BackendResult<Box<dyn StateBackend>> {
    selection.open().await
}

pub(crate) async fn create_explicit_backend(
    url: &StateUrl,
) -> BackendResult<Box<dyn StateBackend>> {
    match url {
        StateUrl::S3 { bucket, key } => Ok(Box::new(
            S3Backend::from_url_parts(bucket.clone(), key.clone()).await?,
        )),
        StateUrl::File { path } => Ok(Box::new(LocalBackend::from_location(
            crate::backend_config::LocalBackendLocation::for_address(path)?,
        )?)),
    }
}

/// Construct a local backend for tests that need to exercise the full backend
/// contract without weakening production selection capabilities.
#[cfg(any(test, feature = "test-utils"))]
pub fn local_backend_for_test(state_path: impl Into<std::path::PathBuf>) -> Box<dyn StateBackend> {
    Box::new(LocalBackend::with_path(state_path.into()))
}

/// Inject an in-process S3 client while retaining the production readiness
/// and provisioning path. No provisioning capability is constructed by tests.
#[cfg(any(test, feature = "test-utils"))]
pub fn s3_backend_for_test(
    client: aws_sdk_s3::Client,
    settings: &crate::S3BackendSettings,
    region: String,
) -> Box<dyn StateBackend> {
    Box::new(S3Backend::from_client(client, settings, region))
}

/// Resolve a local backend's state file path, anchoring a relative
/// `path` (or the unset default) at `base_dir`.
pub fn anchored_local_path(
    config: &LocalBackendSettings,
    base_dir: &ResolvedPath,
) -> std::io::Result<ResolvedPath> {
    base_dir.join(config.path().unwrap_or(LocalBackend::DEFAULT_STATE_FILE))
}
