use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use carina_core::UserDirs;
use carina_core::hint::ProjectCommand;
use carina_core::provider_source::{
    GithubSource, ProviderSource, ProviderSourceParseError, RemoteSource, RemoteSourceKey,
};
use sha2::{Digest, Sha256};

use crate::github::FetchedLocation;
use crate::github_location::{
    ArtifactId, AssetId, CommitSha, GithubLocationError, ReleaseTag, RepoSlug,
};
use crate::install_lock::StagedArtifact;
use crate::provider_resolver::{
    LockEntry, LockEntryKind, LockFile, LockedProviderPin, RegistryLock,
    RegistrySignatureProtection, WasmArtifactPath, cache_path_github_release, cache_path_wasm,
    sha256_file,
};
use crate::provider_trust::{ProviderLoadIdentity, ProviderTrust};
use crate::revision_resolver::cache_path_revision;
use crate::signing::VerifiedSignerIdentity;
use crate::user_trust::{UserTrustRecord, user_trust_record_exists};
use crate::version_resolver::VerifiedReleaseAsset;

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

/// Identity of the exact provider artifact retained for one verified load.
///
/// Only the resolver can construct this value, so callers cannot attach
/// verified provenance to arbitrary provider bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedArtifactIdentity {
    sha256: String,
    origin: LoadedArtifactOrigin,
}

impl LoadedArtifactIdentity {
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn origin(&self) -> &LoadedArtifactOrigin {
        &self.origin
    }

    fn new(bytes: &ProviderArtifactBytes, origin: LoadedArtifactOrigin) -> Self {
        Self {
            sha256: format!("{:x}", Sha256::digest(bytes.as_bytes())),
            origin,
        }
    }

    pub(crate) fn locked(
        bytes: &ProviderArtifactBytes,
        source: RemoteSourceKey,
        pin: &LockedProviderPin,
    ) -> Self {
        let version = match pin {
            LockedProviderPin::Version {
                version,
                constraint: _constraint,
            } => LockedArtifactVersion::Version {
                version: version.clone(),
            },
            LockedProviderPin::GithubRelease {
                version,
                tag: _tag,
                asset_id: _asset_id,
                constraint: _constraint,
            } => LockedArtifactVersion::Version {
                version: version.clone(),
            },
            LockedProviderPin::Revision {
                revision: _revision,
                resolved_sha,
                artifact_id: _artifact_id,
            } => LockedArtifactVersion::Revision {
                resolved_sha: resolved_sha.to_string(),
            },
            LockedProviderPin::RegistryRevision { revision, version } => {
                LockedArtifactVersion::RegistryRevision {
                    revision: revision.clone(),
                    version: version.clone(),
                }
            }
        };
        Self::new(bytes, LoadedArtifactOrigin::Locked { source, version })
    }

    pub(crate) fn file(bytes: &ProviderArtifactBytes) -> Self {
        Self::new(bytes, LoadedArtifactOrigin::File)
    }
}

/// Resolver provenance for a loaded provider artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadedArtifactOrigin {
    Locked {
        source: RemoteSourceKey,
        version: LockedArtifactVersion,
    },
    File,
}

/// Lock-resolved provider version facts that participate in artifact identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockedArtifactVersion {
    Version { version: String },
    Revision { resolved_sha: String },
    RegistryRevision { revision: String, version: String },
}

/// Resolver-owned provider bytes bound to the load identity derived for those
/// exact bytes.
///
/// The private fields prevent callers from pairing an identity with arbitrary
/// bytes. Production values are constructed only by [`crate::InstalledProvider`].
#[derive(Debug, PartialEq, Eq)]
pub struct VerifiedLoad {
    bytes: ProviderArtifactBytes,
    identity: ProviderLoadIdentity,
    artifact: LoadedArtifactIdentity,
}

impl VerifiedLoad {
    pub fn bytes(&self) -> &ProviderArtifactBytes {
        &self.bytes
    }

    pub fn identity(&self) -> &ProviderLoadIdentity {
        &self.identity
    }

    pub fn artifact(&self) -> &LoadedArtifactIdentity {
        &self.artifact
    }

    pub(crate) fn new(
        bytes: ProviderArtifactBytes,
        identity: ProviderLoadIdentity,
        artifact: LoadedArtifactIdentity,
    ) -> Self {
        Self {
            bytes,
            identity,
            artifact,
        }
    }

    /// Construct a bound load for host tests that do not run the resolver.
    #[cfg(feature = "test-utils")]
    pub fn for_test(bytes: impl AsRef<[u8]>, identity: ProviderLoadIdentity) -> Self {
        let bytes = ProviderArtifactBytes(Arc::from(bytes.as_ref()));
        let artifact = LoadedArtifactIdentity::file(&bytes);
        Self {
            bytes,
            identity,
            artifact,
        }
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
    source: RemoteSourceKey,
    project_dir: PathBuf,
    pin: LockedProviderPin,
}

pub(crate) struct OriginDownloadProof {
    origin: OriginUrl,
    sha256: String,
    signer: Option<RegistrySignatureProtection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OriginUrl {
    source: RemoteSourceKey,
    kind: OriginUrlKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OriginUrlKind {
    GithubRelease {
        location: GithubReleaseDownloadLocation,
        requested: url::Url,
    },
    GithubRevision {
        location: GithubRevisionDownloadLocation,
        requested: url::Url,
    },
    VerifiedRegistry(url::Url),
}

/// The typed GitHub API location used to download a release asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubReleaseDownloadLocation {
    repo: RepoSlug,
    tag: ReleaseTag,
    asset_id: AssetId,
}

impl GithubReleaseDownloadLocation {
    pub(crate) fn from_verified_asset(asset: &VerifiedReleaseAsset) -> Self {
        Self {
            repo: asset.repo().clone(),
            tag: asset.tag().clone(),
            asset_id: asset.asset_id(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(repo: RepoSlug, tag: ReleaseTag, asset_id: AssetId) -> Self {
        Self {
            repo,
            tag,
            asset_id,
        }
    }
}

/// The typed GitHub API location used to download an Actions artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubRevisionDownloadLocation {
    repo: RepoSlug,
    artifact_id: ArtifactId,
}

impl GithubRevisionDownloadLocation {
    pub(crate) fn new(repo: RepoSlug, artifact_id: ArtifactId) -> Self {
        Self { repo, artifact_id }
    }

    pub(crate) fn repo(&self) -> &RepoSlug {
        &self.repo
    }

    pub(crate) fn artifact_id(&self) -> ArtifactId {
        self.artifact_id
    }
}

#[derive(Debug)]
pub(crate) enum OriginUrlError {
    InvalidSource {
        source_key: String,
        source: ProviderSourceParseError,
    },
    ExpectedGithubSource {
        source_key: String,
    },
    ExpectedRegistrySource {
        source_key: String,
    },
    InvalidGithubLocation {
        source_key: String,
        source: GithubLocationError,
    },
    WrongRepository {
        source_key: String,
        expected: String,
        actual: String,
    },
    InvalidUrl {
        requested: String,
        source: url::ParseError,
    },
    ContainsUserinfo {
        requested: String,
    },
    MustUseHttps {
        requested: String,
    },
    NonDefaultPort {
        requested: String,
    },
    WrongHost {
        source_key: String,
        expected: &'static str,
        actual: Option<String>,
    },
    WrongPath {
        source_key: String,
        expected_prefix: String,
        actual: String,
    },
}

impl fmt::Display for OriginUrlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSource {
                source_key,
                source: _,
            } => write!(
                formatter,
                "origin source identity {source_key:?} is invalid"
            ),
            Self::ExpectedGithubSource { source_key } => write!(
                formatter,
                "origin source identity {source_key:?} is not a GitHub-direct source"
            ),
            Self::ExpectedRegistrySource { source_key } => write!(
                formatter,
                "origin source identity {source_key:?} is not a registry source"
            ),
            Self::InvalidGithubLocation {
                source_key,
                source: _,
            } => write!(
                formatter,
                "GitHub origin location for {source_key} is invalid"
            ),
            Self::WrongRepository {
                source_key,
                expected,
                actual,
            } => write!(
                formatter,
                "GitHub origin repository for {source_key} must be {expected}, got {actual}"
            ),
            Self::InvalidUrl {
                requested,
                source: _,
            } => write!(formatter, "origin URL {requested:?} is invalid"),
            Self::ContainsUserinfo { requested } => {
                write!(
                    formatter,
                    "origin URL {requested:?} must not contain userinfo"
                )
            }
            Self::MustUseHttps { requested } => {
                write!(formatter, "origin URL {requested:?} must use HTTPS")
            }
            Self::NonDefaultPort { requested } => write!(
                formatter,
                "origin URL {requested:?} must not use a non-default port"
            ),
            Self::WrongHost {
                source_key,
                expected,
                actual,
            } => write!(
                formatter,
                "origin URL for {source_key} must use host {expected:?}, got {:?}",
                actual.as_deref().unwrap_or("<none>")
            ),
            Self::WrongPath {
                source_key,
                expected_prefix,
                actual,
            } => write!(
                formatter,
                "origin URL for {source_key} must stay under path {expected_prefix:?} after URL normalization, got {actual:?}"
            ),
        }
    }
}

impl Error for OriginUrlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSource { source, .. } => Some(source),
            Self::InvalidGithubLocation { source, .. } => Some(source),
            Self::InvalidUrl { source, .. } => Some(source),
            Self::ExpectedGithubSource { .. }
            | Self::ExpectedRegistrySource { .. }
            | Self::WrongRepository { .. }
            | Self::ContainsUserinfo { .. }
            | Self::MustUseHttps { .. }
            | Self::NonDefaultPort { .. }
            | Self::WrongHost { .. }
            | Self::WrongPath { .. } => None,
        }
    }
}

impl OriginUrl {
    fn github_release(
        source: &RemoteSourceKey,
        location: GithubReleaseDownloadLocation,
    ) -> Result<Self, OriginUrlError> {
        require_matching_github_repo(source, &location.repo)?;
        let requested = location
            .repo
            .release_asset_url(location.asset_id)
            .map_err(|error| invalid_github_location(source, error))?;
        let requested = validate_github_api_origin_url(source, &location.repo, requested.as_str())?;
        Ok(Self {
            source: source.clone(),
            kind: OriginUrlKind::GithubRelease {
                location,
                requested,
            },
        })
    }

    fn github_revision(
        source: &RemoteSourceKey,
        location: GithubRevisionDownloadLocation,
    ) -> Result<Self, OriginUrlError> {
        require_matching_github_repo(source, &location.repo)?;
        let requested = location
            .repo
            .artifact_zip_url(location.artifact_id)
            .map_err(|error| invalid_github_location(source, error))?;
        let requested = validate_github_api_origin_url(source, &location.repo, requested.as_str())?;
        Ok(Self {
            source: source.clone(),
            kind: OriginUrlKind::GithubRevision {
                location,
                requested,
            },
        })
    }

    fn verified_registry(
        source: &RemoteSourceKey,
        requested: &str,
        _verified_signer: &VerifiedSignerIdentity,
    ) -> Result<Self, OriginUrlError> {
        require_registry_source(source)?;
        let parsed = parse_secure_origin_url(requested)?;
        Ok(Self {
            source: source.clone(),
            kind: OriginUrlKind::VerifiedRegistry(parsed),
        })
    }

    fn source(&self) -> &RemoteSourceKey {
        &self.source
    }

    fn into_source(self) -> RemoteSourceKey {
        let Self { source, kind } = self;
        match kind {
            OriginUrlKind::GithubRelease {
                location,
                requested,
            } => drop((location, requested)),
            OriginUrlKind::GithubRevision {
                location,
                requested,
            } => drop((location, requested)),
            OriginUrlKind::VerifiedRegistry(requested) => drop(requested),
        }
        source
    }
}

fn invalid_github_location(source: &RemoteSourceKey, error: GithubLocationError) -> OriginUrlError {
    OriginUrlError::InvalidGithubLocation {
        source_key: source.to_string(),
        source: error,
    }
}

fn validate_github_api_origin_url(
    source: &RemoteSourceKey,
    repo: &RepoSlug,
    requested: &str,
) -> Result<url::Url, OriginUrlError> {
    // Parse again at the trust boundary. `url::Url` applies WHATWG path
    // normalization, so traversal segments cannot hide outside the repo path.
    let parsed = parse_secure_origin_url(requested)?;
    if parsed.host_str() != Some("api.github.com") {
        return Err(OriginUrlError::WrongHost {
            source_key: source.to_string(),
            expected: "api.github.com",
            actual: parsed.host_str().map(str::to_owned),
        });
    }
    let expected_prefix = format!("/repos/{}/{}/", repo.owner(), repo.repo());
    if !parsed.path().starts_with(&expected_prefix) {
        return Err(OriginUrlError::WrongPath {
            source_key: source.to_string(),
            expected_prefix,
            actual: parsed.path().to_string(),
        });
    }
    repo.validate_url(requested)
        .map_err(|error| invalid_github_location(source, error))
}

fn parse_secure_origin_url(requested: &str) -> Result<url::Url, OriginUrlError> {
    let parsed = url::Url::parse(requested).map_err(|source| OriginUrlError::InvalidUrl {
        requested: requested.to_string(),
        source,
    })?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(OriginUrlError::ContainsUserinfo {
            requested: requested.to_string(),
        });
    }
    if parsed.scheme() != "https" {
        return Err(OriginUrlError::MustUseHttps {
            requested: requested.to_string(),
        });
    }
    if parsed.port().is_some() {
        return Err(OriginUrlError::NonDefaultPort {
            requested: requested.to_string(),
        });
    }
    Ok(parsed)
}

fn parsed_remote_source(source: &RemoteSourceKey) -> Result<RemoteSource, OriginUrlError> {
    match source.as_str().parse::<ProviderSource>().map_err(|error| {
        OriginUrlError::InvalidSource {
            source_key: source.to_string(),
            source: error,
        }
    })? {
        ProviderSource::Remote(source) => Ok(source),
        ProviderSource::File(_) => unreachable!("a RemoteSourceKey cannot parse as a file source"),
    }
}

fn github_source_from_key(source: &RemoteSourceKey) -> Result<GithubSource, OriginUrlError> {
    match parsed_remote_source(source)? {
        RemoteSource::GithubDirect(github) => Ok(github),
        RemoteSource::Registry(_) => Err(OriginUrlError::ExpectedGithubSource {
            source_key: source.to_string(),
        }),
    }
}

fn github_repo_from_key(source: &RemoteSourceKey) -> Result<RepoSlug, OriginUrlError> {
    let github = github_source_from_key(source)?;
    RepoSlug::try_from(&github).map_err(|error| invalid_github_location(source, error))
}

fn require_matching_github_repo(
    source: &RemoteSourceKey,
    actual: &RepoSlug,
) -> Result<(), OriginUrlError> {
    let expected = github_repo_from_key(source)?;
    if expected == *actual {
        return Ok(());
    }
    Err(OriginUrlError::WrongRepository {
        source_key: source.to_string(),
        expected: format!("{}/{}", expected.owner(), expected.repo()),
        actual: format!("{}/{}", actual.owner(), actual.repo()),
    })
}

fn require_registry_source(source: &RemoteSourceKey) -> Result<(), OriginUrlError> {
    match parsed_remote_source(source)? {
        RemoteSource::Registry(_) => Ok(()),
        RemoteSource::GithubDirect(_) => Err(OriginUrlError::ExpectedRegistrySource {
            source_key: source.to_string(),
        }),
    }
}

#[derive(Debug)]
pub(crate) struct RecordVerifiedOriginDownloadError {
    source_key: RemoteSourceKey,
    sha256: String,
    source: Box<dyn Error + Send + Sync>,
}

impl RecordVerifiedOriginDownloadError {
    fn new(
        source_key: &RemoteSourceKey,
        sha256: &str,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            source_key: source_key.clone(),
            sha256: sha256.to_string(),
            source: Box::new(source),
        }
    }

    fn display_chain(&self) -> String {
        let mut rendered = self.to_string();
        let mut source = Error::source(self);
        while let Some(error) = source {
            rendered.push_str(": ");
            rendered.push_str(&error.to_string());
            source = error.source();
        }
        rendered
    }
}

impl fmt::Display for RecordVerifiedOriginDownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Failed to record user trust for {}@{}",
            self.source_key, self.sha256
        )
    }
}

impl Error for RecordVerifiedOriginDownloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

impl From<RecordVerifiedOriginDownloadError> for String {
    fn from(error: RecordVerifiedOriginDownloadError) -> Self {
        error.display_chain()
    }
}

#[derive(Debug)]
pub(crate) struct FirstUse {
    source: RemoteSourceKey,
}

#[derive(Debug)]
pub(crate) enum ArtifactPin<'a, P, R = RegistryLock> {
    Pinned {
        lock_entry: &'a LockEntry<R>,
        provider_name: &'a str,
        pin: P,
        source: RemoteSourceKey,
    },
    Unpinned {
        first_use: FirstUse,
    },
}

impl<P, R> ArtifactPin<'_, P, R> {
    /// Return the lock pin when the cache is safe to reuse. Allow-listed
    /// sources additionally require a valid user-owned trust record for the
    /// same content/source pair; other sources cannot gain trust from a record.
    pub(crate) fn pinned(&self, user_dirs: &UserDirs) -> Option<(&LockEntry<R>, &P)> {
        match self {
            Self::Pinned {
                lock_entry,
                provider_name: _,
                pin,
                source,
            } if !ProviderTrust::remote_source_is_allowlisted(source)
                || user_trust_record_exists(user_dirs, &lock_entry.sha256, source) =>
            {
                Some((lock_entry, pin))
            }
            Self::Pinned { .. } => None,
            Self::Unpinned { .. } => None,
        }
    }

    fn source(&self) -> &RemoteSourceKey {
        match self {
            Self::Pinned { source, .. } => source,
            Self::Unpinned {
                first_use: FirstUse { source },
            } => source,
        }
    }
}

impl OriginDownloadProof {
    fn new(origin: OriginUrl, sha256: String, signer: Option<RegistrySignatureProtection>) -> Self {
        Self {
            origin,
            sha256,
            signer,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (RemoteSourceKey, String, Option<RegistrySignatureProtection>) {
        (self.origin.into_source(), self.sha256, self.signer)
    }
}

fn record_verified_origin_download(
    user_dirs: &UserDirs,
    origin: OriginUrl,
    sha256: &str,
    signer: Option<&RegistrySignatureProtection>,
) -> Result<(), RecordVerifiedOriginDownloadError> {
    let source = origin.source().clone();
    if !ProviderTrust::remote_source_is_allowlisted(&source) {
        return Ok(());
    }
    UserTrustRecord::from_origin_download(OriginDownloadProof::new(
        origin,
        sha256.to_string(),
        signer.cloned(),
    ))
    .and_then(|record| record.write(user_dirs))
    .map_err(|error| RecordVerifiedOriginDownloadError::new(&source, sha256, error))
}

pub(crate) fn record_verified_registry_origin_download(
    user_dirs: &UserDirs,
    source: &RemoteSourceKey,
    requested_origin_url: &str,
    sha256: &str,
    signer: &RegistrySignatureProtection,
    verified_signer: &VerifiedSignerIdentity,
) -> Result<(), RecordVerifiedOriginDownloadError> {
    let origin = OriginUrl::verified_registry(source, requested_origin_url, verified_signer)
        .map_err(|error| RecordVerifiedOriginDownloadError::new(source, sha256, error))?;
    record_verified_origin_download(user_dirs, origin, sha256, Some(signer))
}

#[cfg(test)]
pub(crate) fn record_test_github_origin_download(
    user_dirs: &UserDirs,
    source: &RemoteSourceKey,
    sha256: &str,
    signer: Option<&RegistrySignatureProtection>,
) -> Result<(), RecordVerifiedOriginDownloadError> {
    let repo = github_repo_from_key(source)
        .map_err(|error| RecordVerifiedOriginDownloadError::new(source, sha256, error))?;
    let location = GithubReleaseDownloadLocation::for_test(
        repo,
        ReleaseTag::from_lock("test".into()),
        AssetId::from_lock(1),
    );
    let origin = OriginUrl::github_release(source, location)
        .map_err(|error| RecordVerifiedOriginDownloadError::new(source, sha256, error))?;
    record_verified_origin_download(user_dirs, origin, sha256, signer)
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

pub(crate) trait OriginDownloadPin {
    type Location;

    fn validated_origin(
        source: &RemoteSourceKey,
        location: Self::Location,
    ) -> Result<OriginUrl, OriginUrlError>;
}

impl OriginDownloadPin for GithubReleaseArtifactPin {
    type Location = GithubReleaseDownloadLocation;

    fn validated_origin(
        source: &RemoteSourceKey,
        location: Self::Location,
    ) -> Result<OriginUrl, OriginUrlError> {
        OriginUrl::github_release(source, location)
    }
}

fn record_verified_pin_origin<P: OriginDownloadPin>(
    user_dirs: &UserDirs,
    source: &RemoteSourceKey,
    location: P::Location,
    sha256: &str,
) -> Result<(), RecordVerifiedOriginDownloadError> {
    let origin = P::validated_origin(source, location)
        .map_err(|error| RecordVerifiedOriginDownloadError::new(source, sha256, error))?;
    record_verified_origin_download(user_dirs, origin, sha256, None)
}

pub(crate) fn record_verified_revision_origin_download(
    user_dirs: &UserDirs,
    source: &RemoteSourceKey,
    location: GithubRevisionDownloadLocation,
    sha256: &str,
) -> Result<(), RecordVerifiedOriginDownloadError> {
    let origin = OriginUrl::github_revision(source, location)
        .map_err(|error| RecordVerifiedOriginDownloadError::new(source, sha256, error))?;
    record_verified_origin_download(user_dirs, origin, sha256, None)
}

#[derive(Debug)]
pub(crate) enum DownloadedArtifact<P> {
    Verified {
        artifact: Box<VerifiedArtifact>,
        pin: P,
    },
    FirstUse {
        path: WasmArtifactPath,
        sha256: String,
        source: RemoteSourceKey,
    },
}

impl<P> DownloadedArtifact<P> {
    fn sha256(&self) -> &str {
        match self {
            Self::Verified { artifact, .. } => artifact.sha256(),
            Self::FirstUse { sha256, .. } => sha256,
        }
    }
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

impl From<VerifiedArtifactError> for String {
    fn from(error: VerifiedArtifactError) -> Self {
        error.into_download_message()
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
        let remedy = revision_artifact_replacement_remedy(&self.project_dir);
        format!(
            "Downloaded provider '{}' ({}@{}) does not match the sha256 pinned in carina-providers.lock. Expected: {}, got: {}. The artifact for an unchanged commit changed after it was pinned. {remedy}",
            self.provider_name,
            self.locked_source,
            self.locked_identity,
            self.expected_sha256,
            self.actual_sha256,
        )
    }
}

pub(crate) fn revision_artifact_replacement_remedy(project_dir: &Path) -> String {
    format!(
        "Push a new commit (or point `revision` at one), or remove this provider's entry from carina-providers.lock and run `{}` to accept the current build.",
        ProjectCommand::new("init", project_dir)
    )
}

pub(crate) struct LockArtifactSelection<'lock, R = RegistryLock> {
    pub(crate) path: WasmArtifactPath,
    pub(crate) lock_entry: &'lock LockEntry<R>,
    source: RemoteSourceKey,
    locked_identity: String,
    pub(crate) pin: LockedProviderPin,
}

struct LockArtifactIdentity<'lock, R> {
    lock_entry: &'lock LockEntry<R>,
    source: RemoteSourceKey,
    locked_identity: &'lock str,
    pin: LockedProviderPin,
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

    pub(crate) fn source(&self) -> &RemoteSourceKey {
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
            source,
            locked_identity,
            pin,
        } = selection;
        Self::verify_bytes_at(
            path.as_path(),
            path.clone(),
            LockArtifactIdentity {
                lock_entry,
                source,
                locked_identity: &locked_identity,
                pin,
            },
            provider_name,
            project_dir,
        )
    }

    fn verify_bytes_at<R>(
        bytes_path: &Path,
        published_path: WasmArtifactPath,
        identity: LockArtifactIdentity<'_, R>,
        provider_name: &str,
        project_dir: &Path,
    ) -> Result<Self, VerifiedArtifactError> {
        let LockArtifactIdentity {
            lock_entry,
            source,
            locked_identity,
            pin,
        } = identity;
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
            source,
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
            source: source.clone(),
        },
        None => ArtifactPin::Unpinned {
            first_use: FirstUse {
                source: source.clone(),
            },
        },
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
            source: source.clone(),
        },
        None => ArtifactPin::Unpinned {
            first_use: FirstUse {
                source: source.clone(),
            },
        },
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
        source,
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
        LockArtifactIdentity {
            lock_entry,
            source,
            locked_identity: &locked_identity,
            pin,
        },
        provider_name,
        &project_dir,
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
            source: _,
        } => publish_cached(staged, lock_entry, provider_name).map(|artifact| {
            DownloadedArtifact::Verified {
                artifact: Box::new(artifact),
                pin,
            }
        }),
        ArtifactPin::Unpinned {
            first_use: FirstUse { source },
        } => {
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
            Ok(DownloadedArtifact::FirstUse {
                path,
                sha256,
                source,
            })
        }
    }
}

pub(crate) fn publish_download<R, P: OriginDownloadPin>(
    user_dirs: &UserDirs,
    staged: StagedArtifact<'_>,
    pin: ArtifactPin<'_, P, R>,
    location: FetchedLocation<P::Location>,
) -> Result<DownloadedArtifact<P>, String> {
    let source = pin.source().clone();
    let downloaded = publish_download_verified(staged, pin)
        .map_err(VerifiedArtifactError::into_download_message)?;
    record_verified_pin_origin::<P>(
        user_dirs,
        &source,
        location.into_inner(),
        downloaded.sha256(),
    )
    .map_err(String::from)?;
    Ok(downloaded)
}

pub(crate) fn publish_revision_download<R, P>(
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
    let source_key = remote_source.source_key();

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
        source: source_key,
        locked_identity,
        pin,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn locked_artifact_identity_drops_user_authored_constraint() {
        let retained_bytes = ProviderArtifactBytes(Arc::from(&b"locked retained bytes"[..]));
        let source = match "github.com/carina-rs/carina-provider-aws"
            .parse::<ProviderSource>()
            .unwrap()
        {
            ProviderSource::Remote(source) => source.source_key(),
            ProviderSource::File(_) => unreachable!(),
        };
        let without_constraint = LoadedArtifactIdentity::locked(
            &retained_bytes,
            source.clone(),
            &LockedProviderPin::Version {
                version: "1.2.3".to_string(),
                constraint: None,
            },
        );
        let with_constraint = LoadedArtifactIdentity::locked(
            &retained_bytes,
            source,
            &LockedProviderPin::Version {
                version: "1.2.3".to_string(),
                constraint: Some("^1.0".to_string()),
            },
        );

        assert_eq!(without_constraint, with_constraint);
        assert_eq!(
            without_constraint.sha256(),
            format!("{:x}", Sha256::digest(retained_bytes.as_bytes()))
        );
    }

    fn trust_record_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(root: &Path, current: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in std::fs::read_dir(current).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.push((
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        std::fs::read(path).unwrap(),
                    ));
                }
            }
        }

        let mut files = Vec::new();
        visit(root, root, &mut files);
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    #[test]
    fn user_trust_record_write_is_concurrent_and_idempotent() {
        let trust = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(trust.path());
        let source = match "github.com/carina-rs/carina-provider-aws"
            .parse::<ProviderSource>()
            .unwrap()
        {
            ProviderSource::Remote(source) => source.source_key(),
            ProviderSource::File(_) => unreachable!(),
        };
        let sha256 = "01".repeat(32);

        record_test_github_origin_download(&user_dirs, &source, &sha256, None).unwrap();
        let first = trust_record_files(user_dirs.trust_record_dir());
        record_test_github_origin_download(&user_dirs, &source, &sha256, None).unwrap();
        assert_eq!(trust_record_files(user_dirs.trust_record_dir()), first);

        let threads = (0..8)
            .map(|_| {
                let source = source.clone();
                let sha256 = sha256.clone();
                let user_dirs = user_dirs.clone();
                std::thread::spawn(move || {
                    record_test_github_origin_download(&user_dirs, &source, &sha256, None)
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }

        assert_eq!(trust_record_files(user_dirs.trust_record_dir()), first);
        assert_eq!(first.len(), 1);
    }

    #[test]
    fn user_trust_record_error_keeps_io_cause_at_string_boundary() {
        let trust = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(trust.path());
        std::fs::create_dir_all(user_dirs.trust_record_dir().parent().unwrap()).unwrap();
        let trust_file = user_dirs.trust_record_dir();
        std::fs::write(trust_file, b"occupied").unwrap();
        let source = match "github.com/carina-rs/carina-provider-aws"
            .parse::<ProviderSource>()
            .unwrap()
        {
            ProviderSource::Remote(source) => source.source_key(),
            ProviderSource::File(_) => unreachable!(),
        };
        let sha256 = "01".repeat(32);

        let error =
            record_test_github_origin_download(&user_dirs, &source, &sha256, None).unwrap_err();

        let error = String::from(error);
        assert!(error.contains("Failed to create provider trust-record directory"));
        assert!(
            error.contains("os error"),
            "the final string boundary must include the io::Error cause: {error}"
        );
    }

    #[test]
    fn github_release_origin_uses_typed_api_location_and_rejects_escaped_paths() {
        let github = match "github.com/carina-rs/carina-provider-aws"
            .parse::<ProviderSource>()
            .unwrap()
        {
            ProviderSource::Remote(RemoteSource::GithubDirect(source)) => source,
            source => panic!("expected GitHub source: {source}"),
        };
        let source = github.source_key();
        let repo = RepoSlug::try_from(&github).unwrap();
        let location = GithubReleaseDownloadLocation::for_test(
            repo.clone(),
            ReleaseTag::from_lock("v1.2.3".into()),
            AssetId::from_lock(42),
        );

        let origin = OriginUrl::github_release(&source, location).unwrap();
        assert!(matches!(
            origin.kind,
            OriginUrlKind::GithubRelease { requested, .. }
                if requested.as_str()
                    == "https://api.github.com/repos/carina-rs/carina-provider-aws/releases/assets/42"
        ));

        for rejected in [
            "https://user@api.github.com/repos/carina-rs/carina-provider-aws/releases/assets/42",
            "http://api.github.com/repos/carina-rs/carina-provider-aws/releases/assets/42",
            "https://api.github.com:8443/repos/carina-rs/carina-provider-aws/releases/assets/42",
            "https://example.com/repos/carina-rs/carina-provider-aws/releases/assets/42",
            "https://api.github.com/repos/evil/repo/releases/assets/42",
        ] {
            let error = validate_github_api_origin_url(&source, &repo, rejected).unwrap_err();
            assert!(!error.to_string().is_empty(), "{rejected}");
        }

        let escaped = "https://api.github.com/repos/carina-rs/carina-provider-aws/releases/assets/../../../attacker";
        assert!(matches!(
            validate_github_api_origin_url(&source, &repo, escaped),
            Err(OriginUrlError::WrongPath { actual, .. })
                if actual == "/repos/carina-rs/attacker"
        ));

        let other = match "github.com/evil/repo".parse::<ProviderSource>().unwrap() {
            ProviderSource::Remote(RemoteSource::GithubDirect(source)) => source,
            source => panic!("expected GitHub source: {source}"),
        };
        let wrong_repo = GithubReleaseDownloadLocation::for_test(
            RepoSlug::try_from(&other).unwrap(),
            ReleaseTag::from_lock("v1.2.3".into()),
            AssetId::from_lock(42),
        );
        assert!(matches!(
            OriginUrl::github_release(&source, wrong_repo),
            Err(OriginUrlError::WrongRepository { .. })
        ));
    }

    #[test]
    fn origin_download_checksum_mismatch_preserves_destination_and_user_trust() {
        let project = tempfile::tempdir().unwrap();
        let user_home = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_home.path());
        std::fs::create_dir_all(user_dirs.trust_record_dir()).unwrap();
        let github = match "github.com/carina-rs/carina-provider-aws"
            .parse::<ProviderSource>()
            .unwrap()
        {
            ProviderSource::Remote(RemoteSource::GithubDirect(source)) => source,
            source => panic!("expected GitHub source: {source}"),
        };
        let source = github.source_key();
        let version = semver::Version::parse("1.0.0").unwrap();
        let tag = ReleaseTag::from_lock("v1.0.0".into());
        let asset_id = AssetId::from_lock(42);
        let locked_bytes = b"repository-installed bytes";
        let downloaded_bytes = b"changed origin bytes";
        let expected_sha256 = format!("{:x}", Sha256::digest(locked_bytes));
        let destination = WasmArtifactPath::new(
            cache_path_github_release(project.path(), &source, &version),
            "aws",
        )
        .unwrap();
        std::fs::create_dir_all(destination.as_path().parent().unwrap()).unwrap();
        std::fs::write(destination.as_path(), locked_bytes).unwrap();
        let install_lock = crate::ProviderInstallLock::acquire(project.path()).unwrap();
        let mut staged = StagedArtifact::for_project(&install_lock, &destination).unwrap();
        staged.write_all(downloaded_bytes).unwrap();
        let mut lock_file = LockFile::default();
        lock_file.upsert(LockEntry {
            name: "aws".into(),
            source: source.to_string(),
            kind: LockEntryKind::GithubRelease {
                version: version.clone(),
                tag: tag.clone(),
                asset_id,
                constraint: None,
            },
            sha256: expected_sha256,
            registry: None,
        });
        let pin = lookup_github_release_artifact_pin(&lock_file, &source, &version, "aws");
        let location = GithubReleaseDownloadLocation::for_test(
            RepoSlug::try_from(&github).unwrap(),
            tag,
            asset_id,
        );

        let error = publish_download(&user_dirs, staged, pin, FetchedLocation::for_test(location))
            .expect_err("origin bytes must match the existing lock pin");

        assert!(
            error.contains("does not match the sha256 pinned"),
            "{error}"
        );
        assert_eq!(std::fs::read(destination.as_path()).unwrap(), locked_bytes);
        assert!(trust_record_files(user_dirs.trust_record_dir()).is_empty());
    }

    #[test]
    fn unchanged_revision_download_error_has_actionable_remedy() {
        let mismatch = ChecksumMismatch {
            provider_name: "aws".into(),
            locked_source: "github.com/carina-rs/carina-provider-aws".into(),
            locked_identity: "abcdef012345".into(),
            expected_sha256: "expected".into(),
            actual_sha256: "actual".into(),
            project_dir: PathBuf::from("infra/foo"),
        };

        assert_eq!(
            mismatch.unchanged_revision_download_message(),
            "Downloaded provider 'aws' (github.com/carina-rs/carina-provider-aws@abcdef012345) does not match the sha256 pinned in carina-providers.lock. Expected: expected, got: actual. The artifact for an unchanged commit changed after it was pinned. Push a new commit (or point `revision` at one), or remove this provider's entry from carina-providers.lock and run `carina init infra/foo` to accept the current build."
        );
    }

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
