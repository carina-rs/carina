//! Resolve semver constraints while retaining the exact GitHub release tag.

use std::fmt;

use carina_core::provider_source::GithubSource;
use semver::{Version, VersionReq};

use crate::github::{GithubApi, GithubAssetResponse, GithubHttp, GithubReleaseResponse};
use crate::github_location::{AssetId, ReleaseTag, RepoSlug};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseAsset {
    name: String,
    id: AssetId,
}

impl ReleaseAsset {
    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[cfg(test)]
    pub(crate) fn id(&self) -> AssetId {
        self.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListedReleaseAsset {
    name: String,
    id: u64,
}

/// A semver release paired with its exact GitHub tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubRelease {
    version: Version,
    tag: ReleaseTag,
    draft: bool,
    prerelease: bool,
    assets: Vec<ListedReleaseAsset>,
}

impl GithubRelease {
    pub(crate) fn from_api(response: GithubReleaseResponse) -> Result<Self, String> {
        let GithubReleaseResponse {
            tag_name,
            draft,
            prerelease,
            assets,
        } = response;
        let tag = ReleaseTag::new(tag_name).map_err(|error| error.to_string())?;
        let version = parse_tag(tag.as_str()).ok_or_else(|| {
            format!(
                "GitHub release tag '{}' is not a semantic version",
                tag.as_str()
            )
        })?;
        Ok(Self {
            version,
            tag,
            draft,
            prerelease,
            assets: assets
                .into_iter()
                .map(|GithubAssetResponse { id, name }| ListedReleaseAsset { name, id })
                .collect(),
        })
    }

    pub(crate) fn version(&self) -> &Version {
        &self.version
    }

    pub(crate) fn tag(&self) -> &ReleaseTag {
        &self.tag
    }
}

/// Proof that one release asset belongs to a published release and is the
/// release's single WASM asset. Its fields are private so only the validation
/// functions in this module can mint a value accepted by the downloader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedReleaseAsset {
    repo: RepoSlug,
    version: Version,
    tag: ReleaseTag,
    asset_id: AssetId,
}

impl VerifiedReleaseAsset {
    pub(crate) fn repo(&self) -> &RepoSlug {
        &self.repo
    }

    pub(crate) fn version(&self) -> &Version {
        &self.version
    }

    pub(crate) fn tag(&self) -> &ReleaseTag {
        &self.tag
    }

    pub(crate) fn asset_id(&self) -> AssetId {
        self.asset_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseAssetSelectionError {
    NoWasmAsset {
        tag: ReleaseTag,
        candidates: Vec<String>,
    },
    MultipleWasmAssets {
        tag: ReleaseTag,
        candidates: Vec<String>,
    },
}

impl fmt::Display for ReleaseAssetSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoWasmAsset { tag, candidates } => write!(
                formatter,
                "GitHub release '{tag}' has no .wasm asset; release assets: {}",
                candidates.join(", ")
            ),
            Self::MultipleWasmAssets { tag, candidates } => write!(
                formatter,
                "GitHub release '{tag}' has more than one .wasm asset; release assets: {}",
                candidates.join(", ")
            ),
        }
    }
}

impl std::error::Error for ReleaseAssetSelectionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseAssetVerificationError {
    InvalidReleaseMetadata {
        tag: String,
        detail: String,
    },
    UnexpectedTag {
        expected: ReleaseTag,
        actual: ReleaseTag,
    },
    UnexpectedVersion {
        tag: ReleaseTag,
        expected: Version,
        actual: Version,
    },
    Draft {
        tag: ReleaseTag,
    },
    PrereleaseForStableVersion {
        tag: ReleaseTag,
        version: Version,
    },
    LockedAssetNotInRelease {
        tag: ReleaseTag,
        asset_id: AssetId,
    },
    LockedAssetNotSelected {
        tag: ReleaseTag,
        locked_asset_id: AssetId,
        selected_asset_id: AssetId,
    },
    AssetSelection(ReleaseAssetSelectionError),
}

impl fmt::Display for ReleaseAssetVerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReleaseMetadata { tag, detail } => {
                write!(
                    formatter,
                    "GitHub release '{tag}' has invalid metadata: {detail}"
                )
            }
            Self::UnexpectedTag { expected, actual } => write!(
                formatter,
                "GitHub release lookup for tag '{expected}' returned tag '{actual}'"
            ),
            Self::UnexpectedVersion {
                tag,
                expected,
                actual,
            } => write!(
                formatter,
                "GitHub release '{tag}' resolves to version {actual}, not locked version {expected}"
            ),
            Self::Draft { tag } => write!(formatter, "GitHub release '{tag}' is a draft"),
            Self::PrereleaseForStableVersion { tag, version } => write!(
                formatter,
                "GitHub release '{tag}' is marked as a prerelease but locked version {version} is not a prerelease"
            ),
            Self::LockedAssetNotInRelease { tag, asset_id } => write!(
                formatter,
                "GitHub release '{tag}' does not contain locked asset id {asset_id}"
            ),
            Self::LockedAssetNotSelected {
                tag,
                locked_asset_id,
                selected_asset_id,
            } => write!(
                formatter,
                "GitHub release '{tag}' locked asset id {locked_asset_id} is not the release's selected .wasm asset id {selected_asset_id}"
            ),
            Self::AssetSelection(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ReleaseAssetVerificationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AssetSelection(error) => Some(error),
            Self::InvalidReleaseMetadata { .. }
            | Self::UnexpectedTag { .. }
            | Self::UnexpectedVersion { .. }
            | Self::Draft { .. }
            | Self::PrereleaseForStableVersion { .. }
            | Self::LockedAssetNotInRelease { .. }
            | Self::LockedAssetNotSelected { .. } => None,
        }
    }
}

impl From<ReleaseAssetSelectionError> for ReleaseAssetVerificationError {
    fn from(error: ReleaseAssetSelectionError) -> Self {
        Self::AssetSelection(error)
    }
}

pub(crate) fn verify_fresh_release_asset(
    repo: RepoSlug,
    release: GithubRelease,
) -> Result<VerifiedReleaseAsset, ReleaseAssetVerificationError> {
    verify_release_asset(repo, release, None)
}

pub(crate) fn verify_locked_release_asset(
    repo: RepoSlug,
    expected_version: &Version,
    expected_tag: &ReleaseTag,
    expected_asset_id: AssetId,
    response: GithubReleaseResponse,
) -> Result<VerifiedReleaseAsset, ReleaseAssetVerificationError> {
    let response_tag = response.tag_name.clone();
    let release = GithubRelease::from_api(response).map_err(|detail| {
        ReleaseAssetVerificationError::InvalidReleaseMetadata {
            tag: response_tag,
            detail,
        }
    })?;
    verify_release_asset(
        repo,
        release,
        Some((expected_version, expected_tag, expected_asset_id)),
    )
}

fn verify_release_asset(
    repo: RepoSlug,
    release: GithubRelease,
    expected: Option<(&Version, &ReleaseTag, AssetId)>,
) -> Result<VerifiedReleaseAsset, ReleaseAssetVerificationError> {
    if let Some((expected_version, expected_tag, _)) = expected {
        if release.tag != *expected_tag {
            return Err(ReleaseAssetVerificationError::UnexpectedTag {
                expected: expected_tag.clone(),
                actual: release.tag,
            });
        }
        if release.version != *expected_version {
            return Err(ReleaseAssetVerificationError::UnexpectedVersion {
                tag: release.tag,
                expected: expected_version.clone(),
                actual: release.version,
            });
        }
    }
    if release.draft {
        return Err(ReleaseAssetVerificationError::Draft { tag: release.tag });
    }
    if release.prerelease && release.version.pre.is_empty() {
        return Err(ReleaseAssetVerificationError::PrereleaseForStableVersion {
            tag: release.tag,
            version: release.version,
        });
    }
    if let Some((_, _, expected_asset_id)) = expected
        && !release
            .assets
            .iter()
            .any(|asset| AssetId::from_api(asset.id) == expected_asset_id)
    {
        return Err(ReleaseAssetVerificationError::LockedAssetNotInRelease {
            tag: release.tag,
            asset_id: expected_asset_id,
        });
    }
    let selected = select_release_asset(&release)?;
    if let Some((_, _, expected_asset_id)) = expected
        && selected.id != expected_asset_id
    {
        return Err(ReleaseAssetVerificationError::LockedAssetNotSelected {
            tag: release.tag,
            locked_asset_id: expected_asset_id,
            selected_asset_id: selected.id,
        });
    }
    Ok(VerifiedReleaseAsset {
        repo,
        version: release.version,
        tag: release.tag,
        asset_id: selected.id,
    })
}

/// Select the release's only `.wasm` asset.
pub(crate) fn select_release_asset(
    release: &GithubRelease,
) -> Result<ReleaseAsset, ReleaseAssetSelectionError> {
    let candidates = release
        .assets
        .iter()
        .map(|asset| asset.name.clone())
        .collect::<Vec<_>>();
    let wasm_assets = release
        .assets
        .iter()
        .filter(|asset| asset.name.ends_with(".wasm"))
        .collect::<Vec<_>>();
    match wasm_assets.as_slice() {
        [asset] => Ok(ReleaseAsset {
            name: asset.name.clone(),
            id: AssetId::from_api(asset.id),
        }),
        [] => Err(ReleaseAssetSelectionError::NoWasmAsset {
            tag: release.tag.clone(),
            candidates,
        }),
        [_, _, ..] => Err(ReleaseAssetSelectionError::MultipleWasmAssets {
            tag: release.tag.clone(),
            candidates,
        }),
    }
}

fn parse_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseResolutionError {
    AmbiguousVersion {
        version: Version,
        tags: Vec<ReleaseTag>,
    },
}

impl fmt::Display for ReleaseResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AmbiguousVersion { version, tags } => write!(
                formatter,
                "GitHub releases are ambiguous for semantic version {version}: {}",
                tags.iter()
                    .map(ReleaseTag::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for ReleaseResolutionError {}

fn resolve_exact_release(
    releases: &[GithubRelease],
    version: &Version,
) -> Result<Option<GithubRelease>, ReleaseResolutionError> {
    let matching = releases
        .iter()
        .filter(|release| release.version() == version)
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [] => Ok(None),
        [release] => Ok(Some((*release).clone())),
        [_, _, ..] => {
            let mut tags = matching
                .into_iter()
                .map(|release| release.tag().clone())
                .collect::<Vec<_>>();
            tags.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            Err(ReleaseResolutionError::AmbiguousVersion {
                version: version.clone(),
                tags,
            })
        }
    }
}

/// Find the highest release matching a semver constraint without changing its tag.
pub(crate) fn resolve_from_releases(
    releases: &[GithubRelease],
    constraint: &VersionReq,
) -> Result<Option<GithubRelease>, ReleaseResolutionError> {
    let version = releases
        .iter()
        .filter(|release| constraint.matches(release.version()))
        .map(GithubRelease::version)
        .max()
        .cloned();
    match version {
        Some(version) => resolve_exact_release(releases, &version),
        None => Ok(None),
    }
}

pub(crate) fn resolve_release_at_version(
    releases: &[GithubRelease],
    version: &Version,
) -> Result<Option<GithubRelease>, ReleaseResolutionError> {
    resolve_exact_release(releases, version)
}

pub(crate) fn fetch_releases_with_api<H: GithubHttp + ?Sized>(
    source: &GithubSource,
    api: &GithubApi<'_, H>,
) -> Result<Vec<GithubRelease>, String> {
    let repo = RepoSlug::try_from(source).map_err(|error| error.to_string())?;
    Ok(api
        .list_releases(&repo)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|response| !response.draft)
        .filter_map(|response| GithubRelease::from_api(response).ok())
        .collect())
}

pub(crate) fn fetch_latest_release_with_api<H: GithubHttp + ?Sized>(
    source: &GithubSource,
    api: &GithubApi<'_, H>,
) -> Result<GithubRelease, String> {
    let repo = RepoSlug::try_from(source).map_err(|error| error.to_string())?;
    GithubRelease::from_api(
        api.latest_release(&repo)
            .map_err(|error| error.to_string())?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{FixtureGithubHttp, GithubApi, GithubReleaseResponse};
    use carina_core::provider_source::{ProviderSource, RemoteSource};

    fn github_source() -> GithubSource {
        match "github.com/acme/widget".parse::<ProviderSource>().unwrap() {
            ProviderSource::Remote(RemoteSource::GithubDirect(source)) => source,
            source => panic!("expected GitHub source: {source}"),
        }
    }

    fn release_with_assets(tag: &str, assets: &[(&str, u64)]) -> GithubRelease {
        GithubRelease::from_api(GithubReleaseResponse {
            tag_name: tag.to_string(),
            draft: false,
            prerelease: !parse_tag(tag).is_none_or(|version| version.pre.is_empty()),
            assets: assets
                .iter()
                .map(|(name, id)| GithubAssetResponse {
                    id: *id,
                    name: (*name).to_string(),
                })
                .collect(),
        })
        .unwrap()
    }

    fn releases_from_tags(tags: &[&str]) -> Vec<GithubRelease> {
        tags.iter()
            .map(|tag| release_with_assets(tag, &[]))
            .collect()
    }

    #[test]
    fn resolve_tilde_constraint() {
        let releases = releases_from_tags(&["v0.4.0", "v0.5.0", "v0.5.1", "v0.5.2", "v0.6.0"]);
        let req = VersionReq::parse("~0.5.0").unwrap();
        let resolved = resolve_from_releases(&releases, &req).unwrap().unwrap();
        assert_eq!(resolved.version, Version::new(0, 5, 2));
        assert_eq!(resolved.tag.as_str(), "v0.5.2");
    }

    #[test]
    fn resolve_caret_constraint() {
        let releases = releases_from_tags(&["v1.0.0", "v1.2.0", "v1.9.0", "v2.0.0"]);
        let req = VersionReq::parse("^1.2.0").unwrap();
        let resolved = resolve_from_releases(&releases, &req).unwrap().unwrap();
        assert_eq!(resolved.version, Version::new(1, 9, 0));
    }

    #[test]
    fn resolve_no_match() {
        let releases = releases_from_tags(&["v0.1.0"]);
        let req = VersionReq::parse("~0.5.0").unwrap();
        assert!(resolve_from_releases(&releases, &req).unwrap().is_none());
    }

    #[test]
    fn resolve_tags_without_v_prefix() {
        let releases = releases_from_tags(&["0.5.0", "0.5.1"]);
        let req = VersionReq::parse("~0.5.0").unwrap();
        let resolved = resolve_from_releases(&releases, &req).unwrap().unwrap();
        assert_eq!(resolved.version, Version::new(0, 5, 1));
    }

    #[test]
    fn explicit_constraint_preserves_v_prefixed_tag_and_asset_id() {
        let releases = [release_with_assets("v0.1.0", &[("provider.wasm", 41)])];
        let req = VersionReq::parse("=0.1.0").unwrap();

        let resolved = resolve_from_releases(&releases, &req).unwrap().unwrap();
        let asset = select_release_asset(&resolved).unwrap();

        assert_eq!(resolved.tag().as_str(), "v0.1.0");
        assert_eq!(asset.id().get(), 41);
    }

    #[test]
    fn explicit_constraint_preserves_bare_tag_and_asset_id() {
        let releases = [release_with_assets("0.1.0", &[("provider.wasm", 42)])];
        let req = VersionReq::parse("=0.1.0").unwrap();

        let resolved = resolve_from_releases(&releases, &req).unwrap().unwrap();
        let asset = select_release_asset(&resolved).unwrap();

        assert_eq!(resolved.tag().as_str(), "0.1.0");
        assert_eq!(asset.id().get(), 42);
    }

    #[test]
    fn latest_release_preserves_v_prefixed_tag_and_asset_id() {
        let url = "https://api.github.com/repos/acme/widget/releases/latest";
        let http = FixtureGithubHttp::default().json(
            url,
            r#"{"tag_name":"v0.1.0","assets":[{"name":"provider.wasm","id":43}]}"#,
        );
        let api = GithubApi::new(&http, None);

        let resolved = fetch_latest_release_with_api(&github_source(), &api).unwrap();
        let asset = select_release_asset(&resolved).unwrap();

        assert_eq!(resolved.tag().as_str(), "v0.1.0");
        assert_eq!(asset.id().get(), 43);
    }

    #[test]
    fn latest_release_preserves_bare_tag_and_asset_id() {
        let url = "https://api.github.com/repos/acme/widget/releases/latest";
        let http = FixtureGithubHttp::default().json(
            url,
            r#"{"tag_name":"0.1.0","assets":[{"name":"provider.wasm","id":44}]}"#,
        );
        let api = GithubApi::new(&http, None);

        let resolved = fetch_latest_release_with_api(&github_source(), &api).unwrap();
        let asset = select_release_asset(&resolved).unwrap();

        assert_eq!(resolved.tag().as_str(), "0.1.0");
        assert_eq!(asset.id().get(), 44);
    }

    #[test]
    fn draft_release_with_higher_matching_version_is_not_selected() {
        let url = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let http = FixtureGithubHttp::default().json(
            url,
            r#"[
                {"tag_name":"v0.2.0","draft":true,"prerelease":false,"assets":[{"name":"draft.wasm","id":52}]},
                {"tag_name":"v0.1.0","draft":false,"prerelease":false,"assets":[{"name":"published.wasm","id":51}]}
            ]"#,
        );
        let api = GithubApi::new(&http, None);
        let req = VersionReq::parse("^0").unwrap();

        let releases = fetch_releases_with_api(&github_source(), &api).unwrap();
        let resolved = resolve_from_releases(&releases, &req).unwrap().unwrap();

        assert_eq!(resolved.tag().as_str(), "v0.1.0");
    }

    #[test]
    fn github_prerelease_flag_keeps_semver_prerelease_matching() {
        let url = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let http = FixtureGithubHttp::default().json(
            url,
            r#"[{"tag_name":"v1.0.0-beta.1","draft":false,"prerelease":true,"assets":[{"name":"beta.wasm","id":53}]}]"#,
        );
        let api = GithubApi::new(&http, None);
        let req = VersionReq::parse("=1.0.0-beta.1").unwrap();

        let releases = fetch_releases_with_api(&github_source(), &api).unwrap();
        let resolved = resolve_from_releases(&releases, &req).unwrap().unwrap();

        assert_eq!(resolved.tag().as_str(), "v1.0.0-beta.1");
    }

    #[test]
    fn fresh_release_verification_rejects_stable_version_marked_prerelease() {
        let release = GithubRelease::from_api(GithubReleaseResponse {
            tag_name: "v1.0.0".into(),
            draft: false,
            prerelease: true,
            assets: vec![GithubAssetResponse {
                id: 42,
                name: "provider.wasm".into(),
            }],
        })
        .unwrap();

        let error =
            verify_fresh_release_asset(RepoSlug::try_from(&github_source()).unwrap(), release)
                .unwrap_err();

        assert!(matches!(
            error,
            ReleaseAssetVerificationError::PrereleaseForStableVersion { tag, version }
                if tag.as_str() == "v1.0.0" && version == Version::new(1, 0, 0)
        ));
    }

    #[test]
    fn locked_non_wasm_asset_is_a_typed_verification_error() {
        let expected_tag = ReleaseTag::from_lock("v1.0.0".into());
        let error = verify_locked_release_asset(
            RepoSlug::try_from(&github_source()).unwrap(),
            &Version::new(1, 0, 0),
            &expected_tag,
            AssetId::from_lock(42),
            GithubReleaseResponse {
                tag_name: "v1.0.0".into(),
                draft: false,
                prerelease: false,
                assets: vec![
                    GithubAssetResponse {
                        id: 7,
                        name: "provider.wasm".into(),
                    },
                    GithubAssetResponse {
                        id: 42,
                        name: "checksums.txt".into(),
                    },
                ],
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ReleaseAssetVerificationError::LockedAssetNotSelected {
                tag,
                locked_asset_id,
                selected_asset_id,
            } if tag.as_str() == "v1.0.0"
                && locked_asset_id.get() == 42
                && selected_asset_id.get() == 7
        ));
    }

    #[test]
    fn locked_prerelease_is_allowed_for_prerelease_semver() {
        let version = Version::parse("1.0.0-beta.1").unwrap();
        let tag = ReleaseTag::from_lock("v1.0.0-beta.1".into());

        let verified = verify_locked_release_asset(
            RepoSlug::try_from(&github_source()).unwrap(),
            &version,
            &tag,
            AssetId::from_lock(42),
            GithubReleaseResponse {
                tag_name: tag.as_str().into(),
                draft: false,
                prerelease: true,
                assets: vec![GithubAssetResponse {
                    id: 42,
                    name: "provider.wasm".into(),
                }],
            },
        )
        .unwrap();

        assert_eq!(verified.version(), &version);
        assert_eq!(verified.tag(), &tag);
        assert_eq!(verified.asset_id().get(), 42);
    }

    #[test]
    fn equal_semver_tags_are_a_typed_ambiguity_error() {
        let releases = [
            release_with_assets("v1.0.0", &[("prefixed.wasm", 61)]),
            release_with_assets("1.0.0", &[("bare.wasm", 62)]),
        ];
        let req = VersionReq::parse("^1").unwrap();

        let error = resolve_from_releases(&releases, &req).unwrap_err();

        assert!(matches!(
            error,
            ReleaseResolutionError::AmbiguousVersion { version, tags }
                if version == Version::new(1, 0, 0)
                    && tags.iter().map(ReleaseTag::as_str).collect::<Vec<_>>()
                        == ["1.0.0", "v1.0.0"]
        ));
    }

    #[test]
    fn single_wasm_asset_is_selected_by_id_regardless_of_name() {
        let release = release_with_assets(
            "v0.1.0",
            &[("checksums.txt", 1), ("arbitrary-provider-name.wasm", 42)],
        );

        let asset = select_release_asset(&release).unwrap();

        assert_eq!(asset.name(), "arbitrary-provider-name.wasm");
        assert_eq!(asset.id().get(), 42);
    }

    #[test]
    fn zero_wasm_assets_is_a_typed_error_listing_all_assets() {
        let release = release_with_assets("0.1.0", &[("checksums.txt", 1), ("provider.tar.gz", 2)]);

        let error = select_release_asset(&release).unwrap_err();

        assert!(matches!(
            &error,
            ReleaseAssetSelectionError::NoWasmAsset { tag, candidates }
                if tag.as_str() == "0.1.0"
                    && candidates == &["checksums.txt", "provider.tar.gz"]
        ));
    }

    #[test]
    fn multiple_wasm_assets_is_a_typed_error_listing_all_assets() {
        let release = release_with_assets(
            "v0.1.0",
            &[("first.wasm", 1), ("notes.txt", 2), ("second.wasm", 3)],
        );

        let error = select_release_asset(&release).unwrap_err();

        assert!(matches!(
            &error,
            ReleaseAssetSelectionError::MultipleWasmAssets { tag, candidates }
                if tag.as_str() == "v0.1.0"
                    && candidates == &["first.wasm", "notes.txt", "second.wasm"]
        ));
    }

    #[test]
    fn resolve_star_picks_highest() {
        let releases = releases_from_tags(&["v0.1.0", "v1.0.0", "v2.0.0"]);
        let req = VersionReq::parse("*").unwrap();
        let resolved = resolve_from_releases(&releases, &req).unwrap().unwrap();
        assert_eq!(resolved.version, Version::new(2, 0, 0));
    }
}
