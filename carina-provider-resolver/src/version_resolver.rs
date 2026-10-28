//! Resolve semver constraints while retaining the exact GitHub release tag.

use std::fmt;

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use carina_core::provider_source::GithubSource;

use crate::github::{GithubApi, GithubAssetResponse, GithubHttp, GithubReleaseResponse};

/// The exact tag string returned by GitHub or persisted in a release lock.
///
/// The inner string is private so a raw semver string cannot accidentally be
/// substituted at a release-download boundary.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ReleaseTag(String);

impl ReleaseTag {
    fn from_api(tag: String) -> Result<Self, String> {
        if tag.is_empty() {
            return Err("GitHub release tag_name must not be empty".to_string());
        }
        Ok(Self(tag))
    }

    #[cfg(test)]
    pub(crate) fn from_lock(tag: String) -> Self {
        Self(tag)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ReleaseTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let tag = String::deserialize(deserializer)?;
        Self::from_api(tag).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ReleaseTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    name: String,
    id: u64,
}

impl ReleaseAsset {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}

/// A semver release paired with its exact GitHub tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRelease {
    version: Version,
    tag: ReleaseTag,
    assets: Vec<ReleaseAsset>,
}

impl GithubRelease {
    pub(crate) fn from_api(response: GithubReleaseResponse) -> Result<Self, String> {
        let GithubReleaseResponse {
            tag_name,
            draft,
            prerelease,
            assets,
        } = response;
        if draft {
            return Err(format!("GitHub release '{tag_name}' is a draft"));
        }
        let tag = ReleaseTag::from_api(tag_name)?;
        let version = parse_tag(tag.as_str()).ok_or_else(|| {
            format!(
                "GitHub release tag '{}' is not a semantic version",
                tag.as_str()
            )
        })?;
        // The parsed semver, not GitHub's advisory flag, controls prerelease
        // matching through `VersionReq`.
        let _ = prerelease;
        Ok(Self {
            version,
            tag,
            assets: assets
                .into_iter()
                .map(|GithubAssetResponse { id, name }| ReleaseAsset { name, id })
                .collect(),
        })
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn tag(&self) -> &ReleaseTag {
        &self.tag
    }

    pub fn assets(&self) -> &[ReleaseAsset] {
        &self.assets
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseAssetSelectionError {
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

/// Select the release's only `.wasm` asset.
pub fn select_release_asset(
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
        [asset] => Ok((*asset).clone()),
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
pub enum ReleaseResolutionError {
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
pub fn resolve_from_releases(
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
    Ok(api
        .list_releases(source)?
        .into_iter()
        .filter(|response| !response.draft)
        .filter_map(|response| GithubRelease::from_api(response).ok())
        .collect())
}

pub(crate) fn fetch_latest_release_with_api<H: GithubHttp + ?Sized>(
    source: &GithubSource,
    api: &GithubApi<'_, H>,
) -> Result<GithubRelease, String> {
    GithubRelease::from_api(api.latest_release(source)?)
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
        assert_eq!(asset.id(), 41);
    }

    #[test]
    fn explicit_constraint_preserves_bare_tag_and_asset_id() {
        let releases = [release_with_assets("0.1.0", &[("provider.wasm", 42)])];
        let req = VersionReq::parse("=0.1.0").unwrap();

        let resolved = resolve_from_releases(&releases, &req).unwrap().unwrap();
        let asset = select_release_asset(&resolved).unwrap();

        assert_eq!(resolved.tag().as_str(), "0.1.0");
        assert_eq!(asset.id(), 42);
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
        assert_eq!(asset.id(), 43);
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
        assert_eq!(asset.id(), 44);
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
        assert_eq!(asset.id(), 42);
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
