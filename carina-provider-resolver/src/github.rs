use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::sync::OnceLock;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::github_location::{
    ArtifactId, AssetId, CommitSha, GitRef, GithubLocationError, ReleaseTag, RepoSlug,
    WorkflowRunId,
};
use crate::verified_artifact::{GithubReleaseDownloadLocation, GithubRevisionDownloadLocation};
use crate::version_resolver::{
    ReleaseAssetVerificationError, VerifiedReleaseAsset,
    verify_locked_release_asset as validate_locked_release_asset,
};

const GITHUB_JSON_ACCEPT: &str = "application/vnd.github+json";
const OCTET_STREAM_ACCEPT: &str = "application/octet-stream";

#[derive(Clone)]
pub(crate) struct GithubRequest {
    url: String,
    accept: &'static str,
    authorization: Option<String>,
    response_body_limit: Option<u64>,
}

impl GithubRequest {
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn accept(&self) -> &str {
        self.accept
    }

    pub(crate) fn authorization(&self) -> Option<&str> {
        self.authorization.as_deref()
    }
}

pub(crate) struct GithubResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl GithubResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// Bytes returned by GitHub together with the typed location used to fetch
/// them. Only this module can mint the accompanying fetched-location token.
#[derive(Debug)]
pub(crate) struct DownloadedFrom<L> {
    bytes: Vec<u8>,
    location: L,
}

impl<L> DownloadedFrom<L> {
    fn new(bytes: Vec<u8>, location: L) -> Self {
        Self { bytes, location }
    }

    pub(crate) fn into_parts(self) -> (Vec<u8>, FetchedLocation<L>) {
        (self.bytes, FetchedLocation(self.location))
    }
}

/// Proof that the typed GitHub client successfully fetched a location.
#[derive(Debug)]
pub(crate) struct FetchedLocation<L>(L);

impl<L> FetchedLocation<L> {
    pub(crate) fn into_inner(self) -> L {
        self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(location: L) -> Self {
        Self(location)
    }

    #[cfg(test)]
    fn as_ref(&self) -> &L {
        &self.0
    }
}

pub(crate) trait GithubHttp {
    fn get(&self, request: GithubRequest) -> Result<GithubResponse, GithubApiError>;
}

pub(crate) trait GithubTokenProvider {
    fn get_github_token(&self) -> Result<String, String>;
}

pub(crate) struct EnvironmentGithubTokenProvider;

impl GithubTokenProvider for EnvironmentGithubTokenProvider {
    fn get_github_token(&self) -> Result<String, String> {
        crate::revision_resolver::get_github_token()
    }
}

pub(crate) struct UreqGithubHttp;

impl UreqGithubHttp {
    fn agent(&self) -> &'static ureq::Agent {
        static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
        AGENT.get_or_init(|| {
            ureq::Agent::config_builder()
                .https_only(true)
                .http_status_as_error(false)
                // Release assets redirect to a signed object-store URL. Never
                // forward the GitHub credential across that redirect boundary.
                .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
                .build()
                .into()
        })
    }
}

impl GithubHttp for UreqGithubHttp {
    fn get(&self, request: GithubRequest) -> Result<GithubResponse, GithubApiError> {
        let mut call = self
            .agent()
            .get(request.url())
            .header("Accept", request.accept())
            .header("User-Agent", "carina");
        if let Some(authorization) = request.authorization() {
            call = call.header("Authorization", authorization);
        }
        let response = call.call().map_err(|error| {
            GithubApiError::Http(format!(
                "Failed to fetch {} from GitHub: {error}",
                request.url()
            ))
        })?;
        let status = response.status().into();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
            })
            .collect();
        let body = read_response_body(
            response.into_body().into_reader(),
            request.url(),
            request.response_body_limit,
        )?;
        Ok(GithubResponse {
            status,
            headers,
            body,
        })
    }
}

fn read_response_body(
    mut reader: impl Read,
    url: &str,
    response_body_limit: Option<u64>,
) -> Result<Vec<u8>, GithubApiError> {
    let mut body = Vec::new();
    match response_body_limit {
        Some(limit) => reader.take(limit.saturating_add(1)).read_to_end(&mut body),
        None => reader.read_to_end(&mut body),
    }
    .map_err(|error| {
        GithubApiError::Http(format!(
            "Failed to read GitHub response body from {url}: {error}"
        ))
    })?;
    if let Some(limit) = response_body_limit
        && body.len() as u64 > limit
    {
        return Err(GithubApiError::ResponseBodyTooLarge {
            url: url.to_string(),
            limit,
        });
    }
    Ok(body)
}

fn response_body_limit_error(url: &str, limit: u64) -> String {
    let limit = byte_size_label(limit);
    format!("GitHub response body from {url} exceeds the {limit} limit")
}

pub(crate) fn byte_size_label(size: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if size >= MIB && size.is_multiple_of(MIB) {
        format!("{} MiB", size / MIB)
    } else {
        format!("{size} byte{}", if size == 1 { "" } else { "s" })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct GithubAssetResponse {
    pub(crate) id: u64,
    pub(crate) name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct GithubReleaseResponse {
    pub(crate) tag_name: String,
    #[serde(default)]
    pub(crate) draft: bool,
    #[serde(default)]
    pub(crate) prerelease: bool,
    #[serde(default)]
    pub(crate) assets: Vec<GithubAssetResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubArtifactResponse {
    pub(crate) id: ArtifactId,
    pub(crate) name: String,
    pub(crate) expired: bool,
    pub(crate) size_in_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct GithubArtifactWireResponse {
    id: u64,
    name: String,
    #[serde(default)]
    expired: bool,
    #[serde(default)]
    size_in_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct GithubArtifactMetadataResponse {
    #[serde(default)]
    pub(crate) workflow_run: Option<GithubArtifactWorkflowRunResponse>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct GithubArtifactWorkflowRunResponse {
    #[serde(default)]
    pub(crate) head_sha: Option<CommitSha>,
}

#[derive(Deserialize)]
struct GithubCommitResponse {
    sha: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct GithubWorkflowRunResponse {
    pub(crate) id: WorkflowRunId,
    #[serde(default)]
    pub(crate) event: Option<String>,
    #[serde(default)]
    pub(crate) head_sha: Option<CommitSha>,
    #[serde(default)]
    pub(crate) head_repository: Option<GithubWorkflowRunHeadRepository>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct GithubWorkflowRunHeadRepository {
    #[serde(default)]
    pub(crate) full_name: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct GithubWorkflowRunsResponse {
    #[serde(default)]
    pub(crate) workflow_runs: Vec<GithubWorkflowRunResponse>,
}

#[derive(Deserialize)]
struct GithubArtifactsResponse {
    #[serde(default)]
    artifacts: Vec<GithubArtifactWireResponse>,
}

#[derive(Debug)]
pub(crate) enum GithubApiError {
    Location(GithubLocationError),
    Http(String),
    Authentication(String),
    ResponseBodyTooLarge {
        url: String,
        limit: u64,
    },
    RateLimited {
        status: u16,
        endpoint: String,
        message: Option<String>,
        retry_after: Option<String>,
    },
    Status {
        status: u16,
        endpoint: String,
        message: Option<String>,
        anonymous: bool,
    },
    Decode {
        endpoint: String,
        source: serde_json::Error,
    },
    PaginationCycle {
        endpoint: String,
    },
    ReleaseAssetVerification(ReleaseAssetVerificationError),
}

impl std::fmt::Display for GithubApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Location(error) => error.fmt(formatter),
            Self::Http(error) | Self::Authentication(error) => formatter.write_str(error),
            Self::ResponseBodyTooLarge { url, limit } => {
                formatter.write_str(&response_body_limit_error(url, *limit))
            }
            Self::RateLimited {
                status,
                endpoint,
                message,
                retry_after,
            } => {
                write!(
                    formatter,
                    "GitHub API rate limit returned status {status} for {endpoint}"
                )?;
                if let Some(message) = message {
                    write!(formatter, ": {message}")?;
                }
                if let Some(retry_after) = retry_after {
                    write!(formatter, ". Retry-After: {retry_after}")?;
                }
                Ok(())
            }
            Self::Status {
                status,
                endpoint,
                message,
                anonymous,
            } => {
                write!(
                    formatter,
                    "GitHub API returned status {status} for {endpoint}"
                )?;
                if let Some(message) = message {
                    write!(formatter, ": {message}")?;
                }
                if *anonymous && matches!(status, 401 | 403 | 404) {
                    formatter
                        .write_str(". Set GITHUB_TOKEN or run `gh auth login` to authenticate")?;
                }
                Ok(())
            }
            Self::Decode { endpoint, source } => {
                write!(
                    formatter,
                    "Failed to parse GitHub response from {endpoint}: {source}"
                )
            }
            Self::PaginationCycle { endpoint } => {
                write!(formatter, "GitHub pagination cycle revisited {endpoint}")
            }
            Self::ReleaseAssetVerification(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GithubApiError {}

impl From<GithubLocationError> for GithubApiError {
    fn from(error: GithubLocationError) -> Self {
        Self::Location(error)
    }
}

impl From<ReleaseAssetVerificationError> for GithubApiError {
    fn from(error: ReleaseAssetVerificationError) -> Self {
        Self::ReleaseAssetVerification(error)
    }
}

enum GithubTokenSource<'token> {
    Fixed(&'token str),
    Anonymous,
    Lazy(&'token dyn GithubTokenProvider),
}

#[derive(Clone, Copy)]
enum GithubAuthentication {
    Optional,
    Required,
}

pub(crate) struct GithubApi<'http, H: ?Sized> {
    http: &'http H,
    token_source: GithubTokenSource<'http>,
    resolved_token: OnceLock<Result<String, String>>,
}

impl<'http, H: GithubHttp + ?Sized> GithubApi<'http, H> {
    pub(crate) fn new(http: &'http H, token: Option<&'http str>) -> Self {
        Self {
            http,
            token_source: match token {
                Some(token) => GithubTokenSource::Fixed(token),
                None => GithubTokenSource::Anonymous,
            },
            resolved_token: OnceLock::new(),
        }
    }

    pub(crate) fn with_token_provider(
        http: &'http H,
        token_provider: &'http dyn GithubTokenProvider,
    ) -> Self {
        Self {
            http,
            token_source: GithubTokenSource::Lazy(token_provider),
            resolved_token: OnceLock::new(),
        }
    }

    pub(crate) fn list_releases(
        &self,
        repo: &RepoSlug,
    ) -> Result<Vec<GithubReleaseResponse>, GithubApiError> {
        self.paginate(
            repo,
            repo.releases_url()?,
            GithubAuthentication::Optional,
            parse_json,
        )
    }

    pub(crate) fn latest_release(
        &self,
        repo: &RepoSlug,
    ) -> Result<GithubReleaseResponse, GithubApiError> {
        let url = repo.latest_release_url()?;
        let response = self.get(&url, GITHUB_JSON_ACCEPT, GithubAuthentication::Optional)?;
        parse_json(&response.body, &url)
    }

    pub(crate) fn release_by_tag(
        &self,
        repo: &RepoSlug,
        tag: &ReleaseTag,
    ) -> Result<GithubReleaseResponse, GithubApiError> {
        let url = repo.release_by_tag_url(tag)?;
        let response = self.get(&url, GITHUB_JSON_ACCEPT, GithubAuthentication::Optional)?;
        parse_json(&response.body, &url)
    }

    pub(crate) fn verify_locked_release_asset(
        &self,
        repo: &RepoSlug,
        version: &semver::Version,
        tag: &ReleaseTag,
        asset_id: AssetId,
    ) -> Result<VerifiedReleaseAsset, GithubApiError> {
        let release = self.release_by_tag(repo, tag)?;
        validate_locked_release_asset(repo.clone(), version, tag, asset_id, release)
            .map_err(Into::into)
    }

    pub(crate) fn download_release_asset(
        &self,
        asset: VerifiedReleaseAsset,
    ) -> Result<DownloadedFrom<GithubReleaseDownloadLocation>, GithubApiError> {
        let url = asset.repo().release_asset_url(asset.asset_id())?;
        let location = GithubReleaseDownloadLocation::from_verified_asset(&asset);
        self.get(&url, OCTET_STREAM_ACCEPT, GithubAuthentication::Optional)
            .map(|response| DownloadedFrom::new(response.body, location))
    }

    pub(crate) fn commit(
        &self,
        repo: &RepoSlug,
        git_ref: &GitRef,
    ) -> Result<CommitSha, GithubApiError> {
        let url = repo.commit_url(git_ref)?;
        let response = self.get(&url, GITHUB_JSON_ACCEPT, GithubAuthentication::Required)?;
        let commit: GithubCommitResponse = parse_json(&response.body, &url)?;
        CommitSha::new(commit.sha).map_err(Into::into)
    }

    pub(crate) fn workflow_runs(
        &self,
        repo: &RepoSlug,
        commit_sha: &CommitSha,
    ) -> Result<Vec<GithubWorkflowRunResponse>, GithubApiError> {
        self.paginate(
            repo,
            repo.workflow_runs_url(commit_sha)?,
            GithubAuthentication::Required,
            |body, url| {
                let page: GithubWorkflowRunsResponse = parse_json(body, url)?;
                Ok(page.workflow_runs)
            },
        )
    }

    pub(crate) fn run_artifacts(
        &self,
        repo: &RepoSlug,
        run_id: WorkflowRunId,
    ) -> Result<Vec<GithubArtifactResponse>, GithubApiError> {
        self.paginate(
            repo,
            repo.run_artifacts_url(run_id)?,
            GithubAuthentication::Required,
            |body, url| {
                let artifacts: GithubArtifactsResponse = parse_json(body, url)?;
                Ok(artifacts
                    .artifacts
                    .into_iter()
                    .map(|artifact| GithubArtifactResponse {
                        id: ArtifactId::from_api(artifact.id),
                        name: artifact.name,
                        expired: artifact.expired,
                        size_in_bytes: artifact.size_in_bytes,
                    })
                    .collect())
            },
        )
    }

    pub(crate) fn artifact(
        &self,
        repo: &RepoSlug,
        artifact_id: ArtifactId,
    ) -> Result<GithubArtifactMetadataResponse, GithubApiError> {
        let url = repo.artifact_url(artifact_id)?;
        let response = self.get(&url, GITHUB_JSON_ACCEPT, GithubAuthentication::Required)?;
        parse_json(&response.body, &url)
    }

    pub(crate) fn download_artifact_zip(
        &self,
        repo: &RepoSlug,
        artifact_id: ArtifactId,
        response_body_limit: u64,
    ) -> Result<DownloadedFrom<GithubRevisionDownloadLocation>, GithubApiError> {
        let url = repo.artifact_zip_url(artifact_id)?;
        let location = GithubRevisionDownloadLocation::new(repo.clone(), artifact_id);
        self.get_with_body_limit(
            &url,
            GITHUB_JSON_ACCEPT,
            GithubAuthentication::Required,
            Some(response_body_limit),
        )
        .map(|response| DownloadedFrom::new(response.body, location))
    }

    fn paginate<T>(
        &self,
        repo: &RepoSlug,
        first_url: url::Url,
        authentication: GithubAuthentication,
        mut parse_page: impl FnMut(&[u8], &url::Url) -> Result<Vec<T>, GithubApiError>,
    ) -> Result<Vec<T>, GithubApiError> {
        let mut next = Some(first_url);
        let mut items = Vec::new();
        let mut visited = BTreeSet::new();
        while let Some(url) = next {
            if !visited.insert(url.as_str().to_string()) {
                return Err(GithubApiError::PaginationCycle {
                    endpoint: endpoint_label(&url),
                });
            }
            let response = self.get(&url, GITHUB_JSON_ACCEPT, authentication)?;
            items.extend(parse_page(&response.body, &url)?);
            next = response
                .header("link")
                .and_then(next_link)
                .map(|next_url| repo.validate_url(next_url))
                .transpose()?;
        }
        Ok(items)
    }

    fn get(
        &self,
        url: &url::Url,
        accept: &'static str,
        authentication: GithubAuthentication,
    ) -> Result<GithubResponse, GithubApiError> {
        self.get_with_body_limit(url, accept, authentication, None)
    }

    fn get_with_body_limit(
        &self,
        url: &url::Url,
        accept: &'static str,
        authentication: GithubAuthentication,
        response_body_limit: Option<u64>,
    ) -> Result<GithubResponse, GithubApiError> {
        let authorization = self.authorization(authentication)?;
        let authenticated = authorization.is_some();
        let mut response = self.http.get(GithubRequest {
            url: url.to_string(),
            accept,
            authorization,
            response_body_limit,
        })?;
        let mut anonymous = !authenticated;
        if authenticated
            && response.status == 401
            && matches!(authentication, GithubAuthentication::Optional)
        {
            response = self.http.get(GithubRequest {
                url: url.to_string(),
                accept,
                authorization: None,
                response_body_limit,
            })?;
            anonymous = true;
        }
        if !(200..300).contains(&response.status) {
            return Err(github_status_error(&response, url, anonymous));
        }
        Ok(response)
    }

    fn authorization(
        &self,
        authentication: GithubAuthentication,
    ) -> Result<Option<String>, GithubApiError> {
        match (&self.token_source, authentication) {
            (GithubTokenSource::Fixed(token), GithubAuthentication::Optional)
            | (GithubTokenSource::Fixed(token), GithubAuthentication::Required) => {
                Ok(Some(format!("Bearer {token}")))
            }
            (GithubTokenSource::Anonymous, GithubAuthentication::Optional) => Ok(None),
            (GithubTokenSource::Anonymous, GithubAuthentication::Required) => {
                Err(GithubApiError::Authentication(github_authentication_error()))
            }
            (GithubTokenSource::Lazy(provider), GithubAuthentication::Optional) => Ok(self
                .resolved_token
                .get_or_init(|| provider.get_github_token())
                .as_ref()
                .ok()
                .map(|token| format!("Bearer {token}"))),
            (GithubTokenSource::Lazy(provider), GithubAuthentication::Required) => self
                .resolved_token
                .get_or_init(|| provider.get_github_token())
                .as_ref()
                .map(|token| Some(format!("Bearer {token}")))
                .map_err(|error| GithubApiError::Authentication(error.clone())),
        }
    }
}

fn github_authentication_error() -> String {
    "GitHub revision providers require authentication. Set GITHUB_TOKEN or run `gh auth login`."
        .to_string()
}

#[derive(Deserialize)]
struct GithubErrorResponse {
    message: String,
}

fn github_status_error(
    response: &GithubResponse,
    url: &url::Url,
    anonymous: bool,
) -> GithubApiError {
    let message = serde_json::from_slice::<GithubErrorResponse>(&response.body)
        .ok()
        .map(|error| error.message);
    let retry_after = response.header("retry-after").map(str::to_string);
    let primary_rate_limited = response.status == 403
        && response
            .header("x-ratelimit-remaining")
            .is_some_and(|remaining| remaining.trim() == "0");
    let secondary_rate_limited = matches!(response.status, 403 | 429) && retry_after.is_some();
    if response.status == 429 || primary_rate_limited || secondary_rate_limited {
        return GithubApiError::RateLimited {
            status: response.status,
            endpoint: endpoint_label(url),
            message,
            retry_after,
        };
    }
    GithubApiError::Status {
        status: response.status,
        endpoint: endpoint_label(url),
        message,
        anonymous,
    }
}

fn parse_json<T: DeserializeOwned>(body: &[u8], url: &url::Url) -> Result<T, GithubApiError> {
    serde_json::from_slice(body).map_err(|source| GithubApiError::Decode {
        endpoint: endpoint_label(url),
        source,
    })
}

fn endpoint_label(url: &url::Url) -> String {
    url.path().to_string()
}

fn next_link(header: &str) -> Option<&str> {
    header.split(',').find_map(|part| {
        let mut fields = part.trim().split(';');
        let target = fields.next()?.trim();
        let is_next = fields.any(|field| field.trim() == "rel=\"next\"");
        is_next
            .then(|| target.strip_prefix('<')?.strip_suffix('>'))
            .flatten()
    })
}

#[cfg(any(test, feature = "test-utils"))]
#[derive(Clone, Default)]
pub struct FixtureGithubHttp {
    responses: std::sync::Arc<
        std::sync::Mutex<BTreeMap<String, std::collections::VecDeque<GithubFixtureResponse>>>,
    >,
    requests: std::sync::Arc<std::sync::Mutex<Vec<GithubRequest>>>,
}

#[cfg(any(test, feature = "test-utils"))]
#[derive(Clone)]
struct GithubFixtureResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[cfg(any(test, feature = "test-utils"))]
impl FixtureGithubHttp {
    pub fn json(self, url: &str, body: impl AsRef<[u8]>) -> Self {
        self.response(url, 200, body, &[])
    }

    #[cfg(test)]
    pub(crate) fn json_with_headers(
        self,
        url: &str,
        body: impl AsRef<[u8]>,
        headers: &[(&str, String)],
    ) -> Self {
        self.response(url, 200, body, headers)
    }

    pub fn bytes(self, url: &str, body: impl AsRef<[u8]>) -> Self {
        self.response(url, 200, body, &[])
    }

    pub(crate) fn response(
        self,
        url: &str,
        status: u16,
        body: impl AsRef<[u8]>,
        headers: &[(&str, String)],
    ) -> Self {
        self.responses
            .lock()
            .unwrap()
            .entry(url.to_string())
            .or_default()
            .push_back(GithubFixtureResponse {
                status,
                headers: headers
                    .iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
                    .collect(),
                body: body.as_ref().to_vec(),
            });
        self
    }

    #[cfg(test)]
    pub(crate) fn requests(&self) -> Vec<GithubRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn requested_urls(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.url.clone())
            .collect()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl GithubHttp for FixtureGithubHttp {
    fn get(&self, request: GithubRequest) -> Result<GithubResponse, GithubApiError> {
        self.requests.lock().unwrap().push(request.clone());
        let response = {
            let mut responses = self.responses.lock().unwrap();
            let responses = responses.get_mut(request.url()).ok_or_else(|| {
                GithubApiError::Http(format!("unexpected fixture GitHub URL: {}", request.url()))
            })?;
            if responses.len() > 1 {
                responses.pop_front()
            } else {
                responses.front().cloned()
            }
            .ok_or_else(|| {
                GithubApiError::Http(format!("exhausted fixture GitHub URL: {}", request.url()))
            })?
        };
        if let Some(limit) = request.response_body_limit
            && response.body.len() as u64 > limit
        {
            return Err(GithubApiError::ResponseBodyTooLarge {
                url: request.url().to_string(),
                limit,
            });
        }
        Ok(GithubResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::provider_source::{GithubSource, ProviderSource, RemoteSource};
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingTokenProvider {
        calls: AtomicUsize,
    }

    impl GithubTokenProvider for CountingTokenProvider {
        fn get_github_token(&self) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("lazy-token".into())
        }
    }

    fn github_source() -> GithubSource {
        match "github.com/acme/widget".parse::<ProviderSource>().unwrap() {
            ProviderSource::Remote(RemoteSource::GithubDirect(source)) => source,
            source => panic!("expected GitHub source: {source}"),
        }
    }

    fn repo_slug() -> RepoSlug {
        RepoSlug::try_from(&github_source()).unwrap()
    }

    fn verified_release_asset(tag: &str, asset_id: u64) -> VerifiedReleaseAsset {
        let release = crate::version_resolver::GithubRelease::from_api(GithubReleaseResponse {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: vec![GithubAssetResponse {
                id: asset_id,
                name: "provider.wasm".into(),
            }],
        })
        .unwrap();
        crate::version_resolver::verify_fresh_release_asset(repo_slug(), release).unwrap()
    }

    #[test]
    fn github_api_error_does_not_expose_displayed_child_as_source() {
        let error = GithubApiError::Decode {
            endpoint: "https://api.github.com/repos/acme/widget/releases".into(),
            source: serde_json::from_slice::<serde_json::Value>(b"{").unwrap_err(),
        };

        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn release_listing_sends_bearer_when_token_is_present() {
        let url = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let http = FixtureGithubHttp::default().json(url, r#"[]"#);
        let api = GithubApi::new(&http, Some("test-token"));

        api.list_releases(&repo_slug()).unwrap();

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].accept(), "application/vnd.github+json");
        assert_eq!(requests[0].authorization(), Some("Bearer test-token"));
    }

    #[test]
    fn release_listing_is_anonymous_when_token_is_absent() {
        let url = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let http = FixtureGithubHttp::default().json(url, r#"[]"#);
        let api = GithubApi::new(&http, None);

        api.list_releases(&repo_slug()).unwrap();

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].authorization(), None);
    }

    #[test]
    fn release_by_tag_uses_the_typed_repository_endpoint() {
        let url = "https://api.github.com/repos/acme/widget/releases/tags/v1.2.3+build_1";
        let http =
            FixtureGithubHttp::default().json(url, r#"{"tag_name":"v1.2.3+build_1","assets":[]}"#);
        let api = GithubApi::new(&http, Some("test-token"));
        let tag = ReleaseTag::from_lock("v1.2.3+build_1".into());

        let release = api.release_by_tag(&repo_slug(), &tag).unwrap();

        assert_eq!(release.tag_name, tag.as_str());
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url(), url);
        assert_eq!(requests[0].accept(), "application/vnd.github+json");
        assert_eq!(requests[0].authorization(), Some("Bearer test-token"));
    }

    #[test]
    fn release_listing_follows_next_link_until_exhausted() {
        let first = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let second = "https://api.github.com/repos/acme/widget/releases?per_page=100&page=2";
        let http = FixtureGithubHttp::default()
            .json_with_headers(
                first,
                r#"[{"tag_name":"v2.0.0","assets":[]}]"#,
                &[("Link", format!("<{second}>; rel=\"next\""))],
            )
            .json(second, r#"[{"tag_name":"1.0.0","assets":[]}]"#);
        let api = GithubApi::new(&http, None);

        let releases = api.list_releases(&repo_slug()).unwrap();

        assert_eq!(
            releases
                .iter()
                .map(|release| release.tag_name.as_str())
                .collect::<Vec<_>>(),
            ["v2.0.0", "1.0.0"]
        );
        assert_eq!(
            http.requests()
                .iter()
                .map(GithubRequest::url)
                .collect::<Vec<_>>(),
            [first, second]
        );
    }

    #[test]
    fn authenticated_release_asset_download_uses_api_endpoint_and_octet_stream() {
        let url = "https://api.github.com/repos/acme/widget/releases/assets/42";
        let http = FixtureGithubHttp::default().bytes(url, b"wasm");
        let api = GithubApi::new(&http, Some("test-token"));
        let repo = repo_slug();
        let tag = ReleaseTag::from_lock("v1.2.3".into());
        let asset_id = AssetId::from_lock(42);
        let verified = verified_release_asset(tag.as_str(), asset_id.get());

        let (bytes, location) = api.download_release_asset(verified).unwrap().into_parts();

        assert_eq!(bytes, b"wasm");
        assert_eq!(
            location.as_ref(),
            &GithubReleaseDownloadLocation::for_test(repo, tag, asset_id)
        );
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url(), url);
        assert_eq!(requests[0].accept(), "application/octet-stream");
        assert_eq!(requests[0].authorization(), Some("Bearer test-token"));
    }

    #[test]
    fn authenticated_actions_artifact_download_returns_typed_location() {
        let url = "https://api.github.com/repos/acme/widget/actions/artifacts/42/zip";
        let http = FixtureGithubHttp::default().bytes(url, b"zip");
        let api = GithubApi::new(&http, Some("test-token"));
        let repo = repo_slug();
        let artifact_id = ArtifactId::from_lock(42);

        let (bytes, location) = api
            .download_artifact_zip(&repo, artifact_id, 1024)
            .unwrap()
            .into_parts();

        assert_eq!(bytes, b"zip");
        assert_eq!(
            location.as_ref(),
            &GithubRevisionDownloadLocation::new(repo, artifact_id)
        );
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url(), url);
        assert_eq!(requests[0].authorization(), Some("Bearer test-token"));
    }

    #[test]
    fn anonymous_not_found_includes_github_message_and_authentication_hint() {
        let url = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let http = FixtureGithubHttp::default().response(
            url,
            404,
            r#"{"message":"Repository access denied"}"#,
            &[],
        );
        let api = GithubApi::new(&http, None);

        let error = api.list_releases(&repo_slug()).unwrap_err().to_string();

        assert!(error.contains("Repository access denied"), "{error}");
        assert!(error.contains("GITHUB_TOKEN"), "{error}");
        assert!(error.contains("gh auth login"), "{error}");
    }

    #[test]
    fn lazy_token_is_resolved_once_across_multiple_api_requests() {
        let releases_url = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let asset_url = "https://api.github.com/repos/acme/widget/releases/assets/42";
        let http = FixtureGithubHttp::default()
            .json(releases_url, r#"[]"#)
            .bytes(asset_url, b"wasm");
        let tokens = CountingTokenProvider {
            calls: AtomicUsize::new(0),
        };
        let api = GithubApi::with_token_provider(&http, &tokens);

        api.list_releases(&repo_slug()).unwrap();
        api.download_release_asset(verified_release_asset("v1.2.3", 42))
            .unwrap();

        assert_eq!(tokens.calls.load(Ordering::SeqCst), 1);
        assert!(
            http.requests()
                .iter()
                .all(|request| { request.authorization() == Some("Bearer lazy-token") })
        );
    }

    #[test]
    fn optional_authentication_retries_unauthorized_request_anonymously() {
        let url = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let http = FixtureGithubHttp::default()
            .response(url, 401, r#"{"message":"Bad credentials"}"#, &[])
            .response(url, 200, r#"[]"#, &[]);
        let api = GithubApi::new(&http, Some("expired-token"));

        assert!(api.list_releases(&repo_slug()).is_ok());

        let requests = http.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].authorization(), Some("Bearer expired-token"));
        assert_eq!(requests[1].authorization(), None);
    }

    #[test]
    fn required_authentication_does_not_retry_unauthorized_request_anonymously() {
        let url = "https://api.github.com/repos/acme/widget/commits/heads%2Fmain";
        let http = FixtureGithubHttp::default()
            .response(url, 401, r#"{"message":"Bad credentials"}"#, &[])
            .response(url, 200, r#"{}"#, &[]);
        let api = GithubApi::new(&http, Some("expired-token"));

        let error = api
            .commit(&repo_slug(), &GitRef::new("heads/main".into()).unwrap())
            .unwrap_err()
            .to_string();

        assert!(error.contains("Bad credentials"), "{error}");
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].authorization(), Some("Bearer expired-token"));
    }

    #[test]
    fn commit_response_rejects_non_canonical_sha() {
        let url = "https://api.github.com/repos/acme/widget/commits/main";
        let http = FixtureGithubHttp::default()
            .json(url, r#"{"sha":"ABCDEF0123456789abcdef0123456789abcdef01"}"#);
        let api = GithubApi::new(&http, Some("token"));

        let error = api
            .commit(&repo_slug(), &GitRef::new("main".into()).unwrap())
            .unwrap_err();

        assert!(matches!(
            error,
            GithubApiError::Location(GithubLocationError::InvalidCommitSha { .. })
        ));
    }

    #[test]
    fn workflow_runs_builds_query_from_typed_commit_sha() {
        let sha = CommitSha::new("abcdef0123456789abcdef0123456789abcdef01".into()).unwrap();
        let url = "https://api.github.com/repos/acme/widget/actions/runs?head_sha=abcdef0123456789abcdef0123456789abcdef01&status=success&event=push&per_page=100";
        let http = FixtureGithubHttp::default().json(
            url,
            r#"{"workflow_runs":[{"id":7,"event":"push","head_sha":"abcdef0123456789abcdef0123456789abcdef01","head_repository":{"full_name":"acme/widget"}}]}"#,
        );
        let api = GithubApi::new(&http, Some("token"));

        let runs = api.workflow_runs(&repo_slug(), &sha).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id.get(), 7);
        assert_eq!(runs[0].event.as_deref(), Some("push"));
        assert_eq!(runs[0].head_sha.as_ref(), Some(&sha));
        assert_eq!(
            runs[0]
                .head_repository
                .as_ref()
                .and_then(|repository| repository.full_name.as_deref()),
            Some("acme/widget")
        );
        assert_eq!(http.requests()[0].url(), url);
    }

    #[test]
    fn workflow_runs_follows_next_link_until_exhausted() {
        let sha = CommitSha::new("abcdef0123456789abcdef0123456789abcdef01".into()).unwrap();
        let first = "https://api.github.com/repos/acme/widget/actions/runs?head_sha=abcdef0123456789abcdef0123456789abcdef01&status=success&event=push&per_page=100";
        let second = "https://api.github.com/repos/acme/widget/actions/runs?head_sha=abcdef0123456789abcdef0123456789abcdef01&status=success&event=push&per_page=100&page=2";
        let http = FixtureGithubHttp::default()
            .json_with_headers(
                first,
                r#"{"workflow_runs":[{"id":8,"event":"push"}]}"#,
                &[("Link", format!("<{second}>; rel=\"next\""))],
            )
            .json(second, r#"{"workflow_runs":[{"id":7,"event":"push"}]}"#);
        let api = GithubApi::new(&http, Some("token"));

        let runs = api.workflow_runs(&repo_slug(), &sha).unwrap();

        assert_eq!(
            runs.iter().map(|run| run.id.get()).collect::<Vec<_>>(),
            [8, 7]
        );
        assert_eq!(
            http.requests()
                .iter()
                .map(GithubRequest::url)
                .collect::<Vec<_>>(),
            [first, second]
        );
    }

    #[test]
    fn run_artifacts_takes_a_deserialized_workflow_run_id() {
        let run_id: WorkflowRunId = serde_json::from_str("7").unwrap();
        let url = "https://api.github.com/repos/acme/widget/actions/runs/7/artifacts?per_page=100";
        let http = FixtureGithubHttp::default().json(
            url,
            r#"{"artifacts":[{"id":42,"name":"provider","expired":false}]}"#,
        );
        let api = GithubApi::new(&http, Some("token"));

        let artifacts = api.run_artifacts(&repo_slug(), run_id).unwrap();

        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id.get(), 42);
        assert_eq!(artifacts[0].size_in_bytes, None);
        assert_eq!(http.requests()[0].url(), url);
    }

    #[test]
    fn artifact_metadata_uses_typed_url_and_deserializes_workflow_head_sha() {
        let sha = CommitSha::new("abcdef0123456789abcdef0123456789abcdef01".into()).unwrap();
        let url = "https://api.github.com/repos/acme/widget/actions/artifacts/42";
        let http = FixtureGithubHttp::default().json(
            url,
            r#"{"id":42,"workflow_run":{"head_repository_id":1,"repository_id":1,"head_sha":"abcdef0123456789abcdef0123456789abcdef01"}}"#,
        );
        let api = GithubApi::new(&http, Some("token"));

        let artifact = api
            .artifact(&repo_slug(), ArtifactId::from_lock(42))
            .unwrap();

        assert_eq!(
            artifact.workflow_run.and_then(|run| run.head_sha),
            Some(sha)
        );
        let requests = http.requests();
        assert_eq!(requests[0].url(), url);
        assert_eq!(requests[0].accept(), "application/vnd.github+json");
        assert_eq!(requests[0].authorization(), Some("Bearer token"));
    }

    #[test]
    fn artifact_zip_download_rejects_body_over_limit() {
        let url = "https://api.github.com/repos/acme/widget/actions/artifacts/42/zip";
        let http = FixtureGithubHttp::default().bytes(url, b"12345");
        let api = GithubApi::new(&http, Some("token"));

        let error = api
            .download_artifact_zip(&repo_slug(), ArtifactId::from_lock(42), 4)
            .unwrap_err()
            .to_string();

        assert!(error.contains("exceeds"), "{error}");
        assert!(error.contains("4 byte"), "{error}");
        assert_eq!(http.requests()[0].response_body_limit, Some(4));
    }

    #[test]
    fn limited_response_reader_stops_after_limit_plus_one_bytes() {
        struct CountingReader {
            inner: std::io::Cursor<Vec<u8>>,
            bytes_read: Rc<Cell<usize>>,
        }

        impl Read for CountingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let count = self.inner.read(buffer)?;
                self.bytes_read.set(self.bytes_read.get() + count);
                Ok(count)
            }
        }

        let bytes_read = Rc::new(Cell::new(0));
        let reader = CountingReader {
            inner: std::io::Cursor::new(vec![0; 32]),
            bytes_read: Rc::clone(&bytes_read),
        };

        let error = read_response_body(reader, "https://api.github.com/artifact", Some(4))
            .expect_err("the fifth byte must prove the body exceeds the limit")
            .to_string();

        assert!(error.contains("exceeds"), "{error}");
        assert_eq!(bytes_read.get(), 5);
    }

    #[test]
    fn non_json_error_body_is_not_included_in_error() {
        let url = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let secret_body = "upstream redirect failed for https://signed.example.test/?token=secret";
        let http = FixtureGithubHttp::default().response(url, 500, secret_body, &[]);
        let api = GithubApi::new(&http, None);

        let error = api.list_releases(&repo_slug()).unwrap_err().to_string();

        assert!(error.contains("status 500"), "{error}");
        assert!(!error.contains(secret_body), "{error}");
        assert!(!error.contains("token=secret"), "{error}");
    }

    #[test]
    fn github_api_url_rejects_explicit_port() {
        let error = repo_slug()
            .validate_url("https://api.github.com:8443/repos/acme/widget/releases")
            .expect_err("an explicit port must not be accepted for GitHub API requests");

        assert!(error.to_string().contains("non-GitHub URL"), "{error}");
    }

    struct CycleGithubHttp {
        requests: Mutex<Vec<GithubRequest>>,
        responses: Mutex<VecDeque<GithubResponse>>,
    }

    impl GithubHttp for CycleGithubHttp {
        fn get(&self, request: GithubRequest) -> Result<GithubResponse, GithubApiError> {
            self.requests.lock().unwrap().push(request);
            self.responses.lock().unwrap().pop_front().ok_or_else(|| {
                GithubApiError::Http("pagination made an unexpected repeated request".to_string())
            })
        }
    }

    #[test]
    fn release_pagination_rejects_cycles_before_repeating_request() {
        let first = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let http = CycleGithubHttp {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([GithubResponse {
                status: 200,
                headers: BTreeMap::from([("link".to_string(), format!("<{first}>; rel=\"next\""))]),
                body: br#"[]"#.to_vec(),
            }])),
        };
        let api = GithubApi::new(&http, None);

        let error = api.list_releases(&repo_slug()).unwrap_err().to_string();

        assert!(error.contains("pagination cycle"), "{error}");
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn release_pagination_rejects_off_host_next_link_before_requesting_it() {
        let first = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let off_host = "https://objects.example.test/releases?page=2";
        let http = FixtureGithubHttp::default().json_with_headers(
            first,
            r#"[]"#,
            &[("Link", format!("<{off_host}>; rel=\"next\""))],
        );
        let api = GithubApi::new(&http, None);

        let error = api.list_releases(&repo_slug()).unwrap_err().to_string();

        assert!(error.contains("non-GitHub URL"), "{error}");
        assert_eq!(http.requests().len(), 1);
        assert_eq!(http.requests()[0].url(), first);
    }

    #[test]
    fn release_pagination_rejects_next_link_outside_typed_repo_before_requesting_it() {
        let first = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let other_repo = "https://api.github.com/repos/attacker/other/releases?page=2";
        let http = FixtureGithubHttp::default().json_with_headers(
            first,
            r#"[]"#,
            &[("Link", format!("<{other_repo}>; rel=\"next\""))],
        );
        let api = GithubApi::new(&http, None);

        let error = api.list_releases(&repo_slug()).unwrap_err();

        assert!(
            matches!(
                &error,
                GithubApiError::Location(GithubLocationError::UnexpectedRepositoryPath {
                    expected_prefix,
                    actual_path,
                }) if expected_prefix == "/repos/acme/widget/"
                    && actual_path == "/repos/attacker/other/releases"
            ),
            "{error}"
        );
        assert_eq!(http.requests().len(), 1);
        assert_eq!(http.requests()[0].url(), first);
    }
}
