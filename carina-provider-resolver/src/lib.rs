//! Provider resolution: download, cache, and verify provider WASM components.

mod github;
mod github_location;
mod install_lock;
pub mod provider_resolver;
mod provider_trust;
pub mod revision_resolver;
mod signing;
mod trusted_local;
mod user_trust;
mod verified_artifact;
mod version_resolver;

#[cfg(any(test, feature = "test-utils"))]
pub use github::FixtureGithubHttp;
pub use github_location::{ArtifactId, AssetId, CommitSha, ReleaseTag};
pub use install_lock::{GuardedLockFile, ProviderInstallLock};
pub use provider_resolver::*;
pub use provider_trust::{
    ProviderLoadIdentity, ProviderLoadIdentityError, ProviderTrust, ProviderTrustClass,
    UnverifiedOfficialProviderError, UnverifiedOfficialProviderReason,
};
pub use trusted_local::{GrantParseError, TrustedLocalGrants};
pub use user_trust::UserTrustRecord;
pub use verified_artifact::{
    LoadedArtifactIdentity, LoadedArtifactOrigin, LockedArtifactVersion, ProviderArtifactBytes,
    VerifiedArtifact, VerifiedLoad,
};

#[cfg(test)]
pub(crate) fn test_env_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(crate) struct ScopedEnvVar {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl ScopedEnvVar {
    pub(crate) fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(name);
        // SAFETY: callers hold test_env_guard() while this process-wide override is active.
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }
}

#[cfg(test)]
impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        // SAFETY: the test still holds test_env_guard() while this guard is dropped.
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.name, value) },
            None => unsafe { std::env::remove_var(self.name) },
        }
    }
}
