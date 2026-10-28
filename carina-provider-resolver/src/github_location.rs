//! Validated GitHub coordinates used at every URL-construction boundary.

use std::fmt;

use carina_core::provider_source::GithubSource;
use serde::{Deserialize, Deserializer, Serialize};

/// A validation failure for a GitHub repository, ref, tag, ID, or API URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GithubLocationError {
    InvalidRepoComponent {
        field: &'static str,
        value: String,
    },
    InvalidReleaseTag {
        value: String,
    },
    InvalidCommitSha {
        value: String,
    },
    InvalidGitRef {
        value: String,
    },
    ApiUrlConstruction {
        detail: String,
    },
    InvalidApiUrl {
        detail: String,
    },
    UnexpectedRepositoryPath {
        expected_prefix: String,
        actual_path: String,
    },
}

impl fmt::Display for GithubLocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRepoComponent { field, value } => {
                write!(formatter, "invalid GitHub repository {field} '{value}'")
            }
            Self::InvalidReleaseTag { value } => {
                write!(formatter, "invalid GitHub release tag '{value}'")
            }
            Self::InvalidCommitSha { value } => {
                write!(formatter, "invalid GitHub commit SHA '{value}'")
            }
            Self::InvalidGitRef { value } => write!(formatter, "invalid GitHub git ref '{value}'"),
            Self::ApiUrlConstruction { detail } => {
                write!(formatter, "failed to construct GitHub API URL: {detail}")
            }
            Self::InvalidApiUrl { detail } => write!(
                formatter,
                "refusing to send GitHub API credentials to non-GitHub URL: {detail}"
            ),
            Self::UnexpectedRepositoryPath {
                expected_prefix,
                actual_path,
            } => write!(
                formatter,
                "refusing GitHub API URL outside repository path {expected_prefix}: {actual_path}"
            ),
        }
    }
}

impl std::error::Error for GithubLocationError {}

/// A validated GitHub repository owner/name pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RepoSlug {
    owner: String,
    repo: String,
}

impl RepoSlug {
    fn new(owner: String, repo: String) -> Result<Self, GithubLocationError> {
        validate_repo_component("owner", &owner)?;
        validate_repo_component("name", &repo)?;
        Ok(Self { owner, repo })
    }

    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }

    pub(crate) fn repo(&self) -> &str {
        &self.repo
    }

    pub(crate) fn releases_url(&self) -> Result<url::Url, GithubLocationError> {
        let mut url = self.url_with_segments(&["releases"])?;
        url.query_pairs_mut().append_pair("per_page", "100");
        self.validate_url(url.as_str())
    }

    pub(crate) fn latest_release_url(&self) -> Result<url::Url, GithubLocationError> {
        self.url_with_segments(&["releases", "latest"])
    }

    pub(crate) fn release_asset_url(
        &self,
        asset_id: AssetId,
    ) -> Result<url::Url, GithubLocationError> {
        let asset_id = asset_id.to_string();
        self.url_with_segments(&["releases", "assets", &asset_id])
    }

    pub(crate) fn commit_url(&self, git_ref: &GitRef) -> Result<url::Url, GithubLocationError> {
        self.url_with_segments(&["commits", git_ref.as_str()])
    }

    pub(crate) fn workflow_runs_url(
        &self,
        commit_sha: &CommitSha,
    ) -> Result<url::Url, GithubLocationError> {
        let mut url = self.url_with_segments(&["actions", "runs"])?;
        url.query_pairs_mut()
            .append_pair("head_sha", commit_sha.as_str())
            .append_pair("status", "success")
            .append_pair("event", "push")
            .append_pair("per_page", "100");
        self.validate_url(url.as_str())
    }

    pub(crate) fn run_artifacts_url(
        &self,
        run_id: WorkflowRunId,
    ) -> Result<url::Url, GithubLocationError> {
        let run_id = run_id.get().to_string();
        let mut url = self.url_with_segments(&["actions", "runs", &run_id, "artifacts"])?;
        url.query_pairs_mut().append_pair("per_page", "100");
        self.validate_url(url.as_str())
    }

    pub(crate) fn artifact_zip_url(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<url::Url, GithubLocationError> {
        let artifact_id = artifact_id.to_string();
        self.url_with_segments(&["actions", "artifacts", &artifact_id, "zip"])
    }

    pub(crate) fn artifact_url(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<url::Url, GithubLocationError> {
        let artifact_id = artifact_id.to_string();
        self.url_with_segments(&["actions", "artifacts", &artifact_id])
    }

    pub(crate) fn validate_url(&self, raw: &str) -> Result<url::Url, GithubLocationError> {
        let url = url::Url::parse(raw).map_err(|error| GithubLocationError::InvalidApiUrl {
            detail: error.to_string(),
        })?;
        if url.scheme() != "https"
            || url.host_str() != Some("api.github.com")
            || url.port().is_some()
            || has_explicit_port(raw)
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(GithubLocationError::InvalidApiUrl {
                detail: "the URL must use https://api.github.com with no port or userinfo"
                    .to_string(),
            });
        }

        let expected_prefix = format!("/repos/{}/{}/", self.owner, self.repo);
        if !url.path().starts_with(&expected_prefix) {
            return Err(GithubLocationError::UnexpectedRepositoryPath {
                expected_prefix,
                actual_path: url.path().to_string(),
            });
        }
        Ok(url)
    }

    fn url_with_segments(&self, suffix: &[&str]) -> Result<url::Url, GithubLocationError> {
        let mut url = url::Url::parse("https://api.github.com/").map_err(|error| {
            GithubLocationError::ApiUrlConstruction {
                detail: error.to_string(),
            }
        })?;
        {
            let mut segments =
                url.path_segments_mut()
                    .map_err(|()| GithubLocationError::ApiUrlConstruction {
                        detail: "the GitHub API base URL cannot contain path segments".to_string(),
                    })?;
            segments.push("repos");
            segments.push(self.owner());
            segments.push(self.repo());
            for segment in suffix {
                segments.push(segment);
            }
        }
        self.validate_url(url.as_str())
    }
}

impl TryFrom<&GithubSource> for RepoSlug {
    type Error = GithubLocationError;

    fn try_from(source: &GithubSource) -> Result<Self, Self::Error> {
        Self::new(source.owner().to_string(), source.repo().to_string())
    }
}

fn validate_repo_component(field: &'static str, value: &str) -> Result<(), GithubLocationError> {
    let valid = !value.is_empty()
        && value != "."
        && value != ".."
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(GithubLocationError::InvalidRepoComponent {
            field,
            value: value.to_string(),
        })
    }
}

/// An exact, validated GitHub release tag.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct ReleaseTag(String);

impl ReleaseTag {
    pub(crate) fn new(value: String) -> Result<Self, GithubLocationError> {
        let valid = !value.is_empty()
            && value != "."
            && value != ".."
            && !value.contains("..")
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-')
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(GithubLocationError::InvalidReleaseTag { value })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn from_lock(value: String) -> Self {
        Self::new(value).expect("test release tags must be valid")
    }

    pub(crate) fn semver(&self) -> Option<semver::Version> {
        semver::Version::parse(self.0.strip_prefix('v').unwrap_or(&self.0)).ok()
    }
}

impl<'de> Deserialize<'de> for ReleaseTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ReleaseTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A full, lowercase Git commit SHA returned by GitHub.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct CommitSha(String);

impl CommitSha {
    pub(crate) fn new(value: String) -> Result<Self, GithubLocationError> {
        if value.len() == 40
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            Ok(Self(value))
        } else {
            Err(GithubLocationError::InvalidCommitSha { value })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return the canonical 12-character display and cache-key prefix.
    pub fn short(&self) -> &str {
        &self.0[..12]
    }
}

impl<'de> Deserialize<'de> for CommitSha {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A validated Git ref supplied by a provider configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GitRef(String);

impl GitRef {
    pub(crate) fn new(value: String) -> Result<Self, GithubLocationError> {
        let valid = !value.is_empty()
            && !value.contains("..")
            && value.split('/').all(|segment| {
                !segment.is_empty()
                    && segment != "."
                    && segment != ".."
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                    })
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(GithubLocationError::InvalidGitRef { value })
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! numeric_github_id {
    ($type:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash,
        )]
        #[serde(transparent)]
        pub struct $type(u64);

        impl $type {
            pub(crate) fn from_api(value: u64) -> Self {
                Self(value)
            }

            pub fn get(self) -> u64 {
                self.0
            }

            #[cfg(test)]
            pub(crate) fn from_lock(value: u64) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

numeric_github_id!(
    AssetId,
    "A GitHub release asset ID from the API or lock file."
);
numeric_github_id!(
    ArtifactId,
    "A GitHub Actions artifact ID from the API or lock file."
);

/// A GitHub Actions workflow-run ID read from a GitHub API response.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub(crate) struct WorkflowRunId(u64);

impl WorkflowRunId {
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WorkflowRunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

fn has_explicit_port(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix("https://") else {
        return false;
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    authority.rsplit_once(':').is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INJECTIONS: &[&str] = &["%2e%2e", "..", "/", "?", "#", "%2F", " ", ""];

    #[test]
    fn repo_slug_rejects_injection_components() {
        for value in INJECTIONS.iter().chain([".hidden"].iter()) {
            assert!(
                RepoSlug::new((*value).to_string(), "repo".into()).is_err(),
                "{value}"
            );
            assert!(
                RepoSlug::new("owner".into(), (*value).to_string()).is_err(),
                "{value}"
            );
        }
        assert!(RepoSlug::new("carina-rs".into(), "provider.release_1".into()).is_ok());

        let source = match "github.com/.hidden/provider"
            .parse::<carina_core::provider_source::ProviderSource>()
            .unwrap()
        {
            carina_core::provider_source::ProviderSource::Remote(
                carina_core::provider_source::RemoteSource::GithubDirect(source),
            ) => source,
            source => panic!("expected GitHub source: {source}"),
        };
        assert!(RepoSlug::try_from(&source).is_err());
    }

    #[test]
    fn release_tag_rejects_injections_in_constructor_and_deserializer() {
        for value in INJECTIONS {
            assert!(ReleaseTag::new((*value).to_string()).is_err(), "{value}");
            let json = serde_json::to_string(value).unwrap();
            assert!(
                serde_json::from_str::<ReleaseTag>(&json).is_err(),
                "{value}"
            );
        }
        assert!(ReleaseTag::new("v1.2.3+build_1".into()).is_ok());
    }

    #[test]
    fn git_ref_rejects_injection_segments() {
        for value in INJECTIONS
            .iter()
            .chain(["main//next", "refs/./main"].iter())
        {
            assert!(GitRef::new((*value).to_string()).is_err(), "{value}");
        }
        assert!(GitRef::new("refs/heads/release-1.2_3".into()).is_ok());
    }

    #[test]
    fn commit_sha_rejects_uppercase_short_and_non_hex_values() {
        for value in [
            "ABCDEF0123456789abcdef0123456789abcdef01",
            "abc123",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ] {
            assert!(CommitSha::new(value.to_string()).is_err(), "{value}");
            let json = serde_json::to_string(value).unwrap();
            assert!(serde_json::from_str::<CommitSha>(&json).is_err(), "{value}");
        }
        assert!(CommitSha::new("abcdef0123456789abcdef0123456789abcdef01".into()).is_ok());
    }

    #[test]
    fn numeric_ids_round_trip_and_reject_string_injections() {
        let asset: AssetId = serde_json::from_str("42").unwrap();
        let artifact: ArtifactId = serde_json::from_str("43").unwrap();
        assert_eq!(asset.get(), 42);
        assert_eq!(artifact.get(), 43);
        for value in INJECTIONS {
            let json = serde_json::to_string(value).unwrap();
            assert!(serde_json::from_str::<AssetId>(&json).is_err(), "{value}");
            assert!(
                serde_json::from_str::<ArtifactId>(&json).is_err(),
                "{value}"
            );
        }
    }
}
