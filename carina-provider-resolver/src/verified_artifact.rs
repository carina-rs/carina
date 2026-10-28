use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use carina_core::hint::ProjectCommand;
use carina_core::provider_source::{ProviderSource, RemoteSource, RemoteSourceKey};
use sha2::{Digest, Sha256};

use crate::github_location::{ArtifactId, CommitSha};
use crate::install_lock::StagedArtifact;
use crate::provider_resolver::{
    LockEntry, LockEntryKind, LockFile, LockedProviderPin, RegistryLock, WasmArtifactPath,
    cache_path_github_release, cache_path_wasm, sha256_file,
};
use crate::revision_resolver::cache_path_revision;

/// Provider artifact bytes captured by the resolver.
///
/// The private representation prevents callers from attaching resolver-owned
/// provenance to arbitrary bytes. Cloning this value only clones its `Arc`.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderArtifactBytes(Arc<[u8]>);

impl ProviderArtifactBytes {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn read(path: &Path) -> io::Result<Self> {
        std::fs::read(path).map(|bytes| Self(Arc::from(bytes)))
    }
}

impl fmt::Debug for ProviderArtifactBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderArtifactBytes")
            .field("len", &self.0.len())
            .finish()
    }
}

/// A provider artifact that owns the exact bytes matched against an existing
/// lock-file SHA256 pin.
///
/// The fields and constructors are private to this crate, so a raw path cannot
/// claim lock-file provenance without verification:
///
/// ```compile_fail,E0451
/// use std::path::PathBuf;
/// use carina_provider_resolver::{LockedProviderPin, VerifiedArtifact, WasmArtifactPath};
/// # let path: WasmArtifactPath = panic!();
/// let artifact = VerifiedArtifact {
///     path,
///     bytes: panic!(),
///     sha256: String::new(),
///     source: "github.com/carina-rs/carina-provider-aws".into(),
///     project_dir: PathBuf::from("."),
///     pin: LockedProviderPin::Version {
///         version: "1.0.0".into(),
///         constraint: None,
///     },
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedArtifact {
    path: WasmArtifactPath,
    bytes: ProviderArtifactBytes,
    sha256: String,
    source: String,
    project_dir: PathBuf,
    pin: LockedProviderPin,
}

#[derive(Debug)]
pub(crate) struct FirstUse(());

#[derive(Debug)]
pub(crate) enum ArtifactPin<'a, P, R = RegistryLock> {
    Pinned {
        lock_entry: &'a LockEntry<R>,
        provider_name: &'a str,
        pin: P,
    },
    Unpinned(FirstUse),
}

impl<P, R> ArtifactPin<'_, P, R> {
    pub(crate) fn pinned(&self) -> Option<(&LockEntry<R>, &P)> {
        match self {
            Self::Pinned {
                lock_entry,
                provider_name: _,
                pin,
            } => Some((lock_entry, pin)),
            Self::Unpinned(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GithubReleaseArtifactPin {
    pub(crate) constraint: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RevisionArtifactPin {
    pub(crate) revision: String,
    pub(crate) resolved_sha: CommitSha,
    pub(crate) artifact_id: ArtifactId,
}

#[derive(Debug)]
pub(crate) enum DownloadedArtifact<P> {
    Verified {
        artifact: VerifiedArtifact,
        pin: P,
    },
    FirstUse {
        path: WasmArtifactPath,
        sha256: String,
    },
}

#[derive(Debug)]
pub(crate) struct ChecksumMismatch {
    provider_name: String,
    locked_source: String,
    locked_identity: String,
    expected_sha256: String,
    actual_sha256: String,
    project_dir: PathBuf,
}

#[derive(Debug)]
pub(crate) enum VerifiedArtifactError {
    ChecksumMismatch(Box<ChecksumMismatch>),
    Other(String),
}

impl VerifiedArtifactError {
    fn into_download_message(self) -> String {
        match self {
            Self::ChecksumMismatch(mismatch) => mismatch.download_message(),
            Self::Other(message) => message,
        }
    }
}

impl fmt::Display for VerifiedArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChecksumMismatch(mismatch) => formatter.write_str(&mismatch.cache_message()),
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl ChecksumMismatch {
    fn summary(&self) -> String {
        format!(
            "SHA256 mismatch for provider '{}' ({}@{}). Expected: {}, got: {}.",
            self.provider_name,
            self.locked_source,
            self.locked_identity,
            self.expected_sha256,
            self.actual_sha256,
        )
    }

    fn cache_message(&self) -> String {
        format!(
            "installed artifact {}@{} does not match carina-providers.lock (expected {}, got {}). Run `{}` to discard the mismatched copy, then run it again to download a verified one.",
            self.locked_source,
            self.locked_identity,
            self.expected_sha256,
            self.actual_sha256,
            ProjectCommand::new("init", &self.project_dir),
        )
    }

    pub(crate) fn after_cache_removal(self, cached_path: &Path, removal: io::Result<()>) -> String {
        let summary = self.summary();
        let command = ProjectCommand::new("init", &self.project_dir);
        match removal {
            Ok(()) => format!(
                "{summary} The mismatched cached copy was removed; re-run `{command}` to download it again."
            ),
            Err(error) => format!(
                "{summary} Failed to remove the mismatched cached copy at {}: {error}. Remove it manually, then re-run `{command}` to download it again.",
                cached_path.display(),
            ),
        }
    }

    fn download_message(self) -> String {
        format!(
            "Downloaded provider '{}' ({}@{}) does not match the sha256 pinned in carina-providers.lock. Expected: {}, got: {}. The origin artifact changed after it was pinned; if the change is legitimate, remove the '{}' entry from carina-providers.lock and re-run `{}`.",
            self.provider_name,
            self.locked_source,
            self.locked_identity,
            self.expected_sha256,
            self.actual_sha256,
            self.locked_source,
            ProjectCommand::new("init", &self.project_dir),
        )
    }

    fn unchanged_revision_download_message(self) -> String {
        format!(
            "Downloaded provider '{}' ({}@{}) does not match the sha256 pinned in carina-providers.lock. Expected: {}, got: {}. The artifact for an unchanged commit changed after it was pinned; refusing to replace the lock entry.",
            self.provider_name,
            self.locked_source,
            self.locked_identity,
            self.expected_sha256,
            self.actual_sha256,
        )
    }
}

pub(crate) struct LockArtifactSelection<'lock, R = RegistryLock> {
    pub(crate) path: WasmArtifactPath,
    pub(crate) lock_entry: &'lock LockEntry<R>,
    locked_identity: String,
    pub(crate) pin: LockedProviderPin,
}

impl VerifiedArtifact {
    pub fn path(&self) -> &WasmArtifactPath {
        &self.path
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(crate) fn bytes(&self) -> &ProviderArtifactBytes {
        &self.bytes
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    pub(crate) fn pin(&self) -> &LockedProviderPin {
        &self.pin
    }

    pub(crate) fn from_existing<R>(
        lock_entry: &LockEntry<R>,
        provider_name: &str,
        project_dir: &Path,
    ) -> Result<Self, VerifiedArtifactError> {
        let selection = lock_artifact_selection(project_dir, lock_entry, provider_name)
            .map_err(VerifiedArtifactError::Other)?;
        Self::from_selection(selection, provider_name, project_dir)
    }

    pub(crate) fn from_selection<R>(
        selection: LockArtifactSelection<'_, R>,
        provider_name: &str,
        project_dir: &Path,
    ) -> Result<Self, VerifiedArtifactError> {
        let LockArtifactSelection {
            path,
            lock_entry,
            locked_identity,
            pin,
        } = selection;
        Self::verify_bytes_at(
            path.as_path(),
            path.clone(),
            lock_entry,
            &locked_identity,
            provider_name,
            project_dir,
            pin,
        )
    }

    fn verify_bytes_at<R>(
        bytes_path: &Path,
        published_path: WasmArtifactPath,
        lock_entry: &LockEntry<R>,
        locked_identity: &str,
        provider_name: &str,
        project_dir: &Path,
        pin: LockedProviderPin,
    ) -> Result<Self, VerifiedArtifactError> {
        let bytes = ProviderArtifactBytes::read(bytes_path).map_err(|error| {
            VerifiedArtifactError::Other(format!("Failed to hash WASM binary: {error}"))
        })?;
        let mut hasher = Sha256::new();
        hasher.update(bytes.as_bytes());
        let actual_sha256 = format!("{:x}", hasher.finalize());
        if actual_sha256 != lock_entry.sha256 {
            return Err(VerifiedArtifactError::ChecksumMismatch(Box::new(
                ChecksumMismatch {
                    provider_name: provider_name.to_string(),
                    locked_source: lock_entry.source.clone(),
                    locked_identity: locked_identity.to_string(),
                    expected_sha256: lock_entry.sha256.clone(),
                    actual_sha256,
                    project_dir: project_dir.to_path_buf(),
                },
            )));
        }
        Ok(Self {
            path: published_path,
            bytes,
            sha256: actual_sha256,
            source: lock_entry.source.clone(),
            project_dir: project_dir.to_path_buf(),
            pin,
        })
    }
}

pub(crate) fn lookup_github_release_artifact_pin<'a>(
    lock_file: &'a LockFile,
    source: &RemoteSourceKey,
    version: &semver::Version,
    provider_name: &'a str,
) -> ArtifactPin<'a, GithubReleaseArtifactPin> {
    let version_string = version.to_string();
    let pinned = lock_file
        .find_by_source_key_and_version(source, &version_string)
        .and_then(|entry| match &entry.kind {
            LockEntryKind::GithubRelease {
                version: locked_version,
                tag: _,
                asset_id: _,
                constraint,
            } if locked_version == version => Some((
                entry,
                GithubReleaseArtifactPin {
                    constraint: constraint.clone(),
                },
            )),
            LockEntryKind::GithubRelease {
                version: _,
                tag: _,
                asset_id: _,
                constraint: _,
            }
            | LockEntryKind::Version {
                version: _,
                constraint: _,
            }
            | LockEntryKind::Revision {
                revision: _,
                resolved_sha: _,
                artifact_id: _,
            }
            | LockEntryKind::RegistryRevision {
                revision: _,
                version: _,
            }
            | LockEntryKind::File => None,
        });
    match pinned {
        Some((lock_entry, pin)) => ArtifactPin::Pinned {
            lock_entry,
            provider_name,
            pin,
        },
        None => ArtifactPin::Unpinned(FirstUse(())),
    }
}

pub(crate) fn lookup_revision_artifact_pin<'a>(
    lock_file: &'a LockFile,
    source: &RemoteSourceKey,
    revision: &str,
    resolved_sha: &CommitSha,
    provider_name: &'a str,
) -> ArtifactPin<'a, RevisionArtifactPin> {
    let pinned = lock_file
        .find_by_source_key_and_sha(source, resolved_sha)
        .and_then(|entry| match &entry.kind {
            LockEntryKind::Revision {
                revision: _,
                resolved_sha: locked_sha,
                artifact_id,
            } => Some((
                entry,
                RevisionArtifactPin {
                    revision: revision.to_string(),
                    resolved_sha: locked_sha.clone(),
                    artifact_id: *artifact_id,
                },
            )),
            LockEntryKind::Version {
                version: _,
                constraint: _,
            }
            | LockEntryKind::GithubRelease {
                version: _,
                tag: _,
                asset_id: _,
                constraint: _,
            }
            | LockEntryKind::RegistryRevision {
                revision: _,
                version: _,
            }
            | LockEntryKind::File => None,
        });
    match pinned {
        Some((lock_entry, pin)) => ArtifactPin::Pinned {
            lock_entry,
            provider_name,
            pin,
        },
        None => ArtifactPin::Unpinned(FirstUse(())),
    }
}

pub(crate) fn publish_cached<R>(
    staged: StagedArtifact<'_>,
    lock_entry: &LockEntry<R>,
    provider_name: &str,
) -> Result<VerifiedArtifact, VerifiedArtifactError> {
    let project_dir = staged.project_dir().to_path_buf();
    let LockArtifactSelection {
        path,
        lock_entry: _,
        locked_identity,
        pin,
    } = lock_artifact_selection(&project_dir, lock_entry, provider_name)
        .map_err(VerifiedArtifactError::Other)?;
    if staged.destination() != &path {
        return Err(VerifiedArtifactError::Other(format!(
            "Staged provider artifact {} does not match lock-selected path {}",
            staged.destination().as_path().display(),
            path.as_path().display()
        )));
    }
    let verified = VerifiedArtifact::verify_bytes_at(
        staged.path(),
        path,
        lock_entry,
        &locked_identity,
        provider_name,
        &project_dir,
        pin,
    )?;
    let published_path = verified.path.as_path().to_path_buf();
    staged.publish().map_err(|error| {
        VerifiedArtifactError::Other(format!(
            "Failed to publish provider artifact {}: {error}",
            published_path.display()
        ))
    })?;
    Ok(verified)
}

fn publish_download_verified<R, P>(
    staged: StagedArtifact<'_>,
    pin: ArtifactPin<'_, P, R>,
) -> Result<DownloadedArtifact<P>, VerifiedArtifactError> {
    match pin {
        ArtifactPin::Pinned {
            lock_entry,
            provider_name,
            pin,
        } => publish_cached(staged, lock_entry, provider_name)
            .map(|artifact| DownloadedArtifact::Verified { artifact, pin }),
        ArtifactPin::Unpinned(FirstUse(())) => {
            let sha256 = sha256_file(staged.path()).map_err(|error| {
                VerifiedArtifactError::Other(format!("Failed to hash WASM binary: {error}"))
            })?;
            let path = staged.destination().clone();
            let published_path = path.as_path().to_path_buf();
            staged.publish().map_err(|error| {
                VerifiedArtifactError::Other(format!(
                    "Failed to publish provider artifact {}: {error}",
                    published_path.display()
                ))
            })?;
            Ok(DownloadedArtifact::FirstUse { path, sha256 })
        }
    }
}

pub(crate) fn publish_download<R, P>(
    staged: StagedArtifact<'_>,
    pin: ArtifactPin<'_, P, R>,
) -> Result<DownloadedArtifact<P>, String> {
    publish_download_verified(staged, pin).map_err(VerifiedArtifactError::into_download_message)
}

pub(crate) fn publish_unchanged_revision_download<R, P>(
    staged: StagedArtifact<'_>,
    pin: ArtifactPin<'_, P, R>,
) -> Result<DownloadedArtifact<P>, String> {
    publish_download_verified(staged, pin).map_err(|error| match error {
        VerifiedArtifactError::ChecksumMismatch(mismatch) => {
            mismatch.unchanged_revision_download_message()
        }
        VerifiedArtifactError::Other(message) => message,
    })
}

pub(crate) fn lock_artifact_selection<'lock, R>(
    project_dir: &Path,
    lock_entry: &'lock LockEntry<R>,
    provider_name: &str,
) -> Result<LockArtifactSelection<'lock, R>, String> {
    let source = lock_entry
        .source
        .parse::<ProviderSource>()
        .map_err(|error| format!("Invalid provider source in lock entry: {error}"))?;
    let remote_source = match source {
        ProviderSource::Remote(source) => source,
        ProviderSource::File(_) => {
            return Err(format!(
                "Provider '{provider_name}' has file provenance and no remote project-cache path"
            ));
        }
    };

    let (path, locked_identity, pin) = match (&lock_entry.kind, remote_source) {
        (
            LockEntryKind::Version {
                version,
                constraint,
            },
            source,
        ) => (
            cache_path_wasm(project_dir, &source.source_key(), version),
            version.clone(),
            LockedProviderPin::Version {
                version: version.clone(),
                constraint: constraint.clone(),
            },
        ),
        (
            LockEntryKind::GithubRelease {
                version,
                tag,
                asset_id,
                constraint,
            },
            RemoteSource::GithubDirect(source),
        ) => (
            cache_path_github_release(project_dir, &source.source_key(), version),
            version.to_string(),
            LockedProviderPin::GithubRelease {
                version: version.to_string(),
                tag: tag.clone(),
                asset_id: *asset_id,
                constraint: constraint.clone(),
            },
        ),
        (
            LockEntryKind::GithubRelease {
                version: _,
                tag: _,
                asset_id: _,
                constraint: _,
            },
            RemoteSource::Registry(_source),
        ) => {
            return Err(format!(
                "Provider '{provider_name}' has a GitHub release lock pin for a registry source"
            ));
        }
        (LockEntryKind::RegistryRevision { revision, version }, source) => (
            cache_path_wasm(project_dir, &source.source_key(), version),
            version.clone(),
            LockedProviderPin::RegistryRevision {
                revision: revision.clone(),
                version: version.clone(),
            },
        ),
        (
            LockEntryKind::Revision {
                revision,
                resolved_sha,
                artifact_id,
            },
            RemoteSource::GithubDirect(source),
        ) => (
            cache_path_revision(project_dir, &source, resolved_sha),
            resolved_sha.short().to_string(),
            LockedProviderPin::Revision {
                revision: revision.clone(),
                resolved_sha: resolved_sha.clone(),
                artifact_id: *artifact_id,
            },
        ),
        (
            LockEntryKind::Revision {
                revision: _revision,
                resolved_sha: _resolved_sha,
                artifact_id: _artifact_id,
            },
            RemoteSource::Registry(_registry_source),
        ) => {
            return Err(format!(
                "Provider '{provider_name}' has a GitHub revision lock pin for a registry source"
            ));
        }
        (LockEntryKind::File, RemoteSource::GithubDirect(_github_source)) => {
            return Err(format!(
                "Provider '{provider_name}' has file provenance and no remote project-cache path"
            ));
        }
        (LockEntryKind::File, RemoteSource::Registry(_registry_source)) => {
            return Err(format!(
                "Provider '{provider_name}' has file provenance and no remote project-cache path"
            ));
        }
    };

    Ok(LockArtifactSelection {
        path: WasmArtifactPath::new(path, provider_name)?,
        lock_entry,
        locked_identity,
        pin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_removal_failure_is_included_in_checksum_error() {
        let mismatch = ChecksumMismatch {
            provider_name: "aws".into(),
            locked_source: "github.com/carina-rs/carina-provider-aws".into(),
            locked_identity: "1.0.0".into(),
            expected_sha256: "expected".into(),
            actual_sha256: "actual".into(),
            project_dir: PathBuf::from("infra/foo"),
        };
        let cached_path = Path::new("infra/foo/.carina/providers/aws.wasm");

        let message = mismatch.after_cache_removal(
            cached_path,
            Err(io::Error::other("simulated removal failure")),
        );

        assert_eq!(
            message,
            "SHA256 mismatch for provider 'aws' (github.com/carina-rs/carina-provider-aws@1.0.0). Expected: expected, got: actual. Failed to remove the mismatched cached copy at infra/foo/.carina/providers/aws.wasm: simulated removal failure. Remove it manually, then re-run `carina init infra/foo` to download it again."
        );
    }
}
