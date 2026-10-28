use std::collections::BTreeMap;
use std::io::Read;
use std::sync::OnceLock;

use carina_core::provider_source::GithubSource;
use serde::Deserialize;
use serde::de::DeserializeOwned;

const GITHUB_JSON_ACCEPT: &str = "application/vnd.github+json";
const OCTET_STREAM_ACCEPT: &str = "application/octet-stream";

#[derive(Clone)]
pub(crate) struct GithubRequest {
    url: String,
    accept: &'static str,
    authorization: Option<String>,
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

pub(crate) trait GithubHttp {
    fn get(&self, request: GithubRequest) -> Result<GithubResponse, String>;
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
    fn get(&self, request: GithubRequest) -> Result<GithubResponse, String> {
        let mut call = self
            .agent()
            .get(request.url())
            .header("Accept", request.accept())
            .header("User-Agent", "carina");
        if let Some(authorization) = request.authorization() {
            call = call.header("Authorization", authorization);
        }
        let response = call
            .call()
            .map_err(|error| format!("Failed to fetch {} from GitHub: {error}", request.url()))?;
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
        let mut body = Vec::new();
        response
            .into_body()
            .into_reader()
            .read_to_end(&mut body)
            .map_err(|error| {
                format!(
                    "Failed to read GitHub response body from {}: {error}",
                    request.url()
                )
            })?;
        Ok(GithubResponse {
            status,
            headers,
            body,
        })
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

enum GithubTokenSource<'token> {
    Fixed(Option<&'token str>),
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
            token_source: GithubTokenSource::Fixed(token),
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
        source: &GithubSource,
    ) -> Result<Vec<GithubReleaseResponse>, String> {
        let mut url = github_repo_url(source, &["releases"])?;
        url.query_pairs_mut().append_pair("per_page", "100");
        let mut next = Some(url.to_string());
        let mut releases = Vec::new();
        while let Some(url) = next {
            let response = self.get(&url, GITHUB_JSON_ACCEPT, GithubAuthentication::Optional)?;
            let page: Vec<GithubReleaseResponse> = parse_json(&response.body, &url)?;
            releases.extend(page);
            next = response
                .header("link")
                .and_then(next_link)
                .map(str::to_string);
            if let Some(next_url) = &next {
                validate_github_api_url(next_url)?;
            }
        }
        Ok(releases)
    }

    pub(crate) fn latest_release(
        &self,
        source: &GithubSource,
    ) -> Result<GithubReleaseResponse, String> {
        let url = github_repo_url(source, &["releases", "latest"])?;
        let response = self.get(
            url.as_str(),
            GITHUB_JSON_ACCEPT,
            GithubAuthentication::Optional,
        )?;
        parse_json(&response.body, url.as_str())
    }

    pub(crate) fn download_release_asset(
        &self,
        source: &GithubSource,
        asset_id: u64,
    ) -> Result<Vec<u8>, String> {
        let id = asset_id.to_string();
        let url = github_repo_url(source, &["releases", "assets", &id])?;
        self.get(
            url.as_str(),
            OCTET_STREAM_ACCEPT,
            GithubAuthentication::Optional,
        )
        .map(|response| response.body)
    }

    pub(crate) fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let response = self.get(url, GITHUB_JSON_ACCEPT, GithubAuthentication::Required)?;
        parse_json(&response.body, url)
    }

    pub(crate) fn get_bytes(&self, url: &str, accept: &'static str) -> Result<Vec<u8>, String> {
        self.get(url, accept, GithubAuthentication::Required)
            .map(|response| response.body)
    }

    fn get(
        &self,
        url: &str,
        accept: &'static str,
        authentication: GithubAuthentication,
    ) -> Result<GithubResponse, String> {
        validate_github_api_url(url)?;
        let authorization = self.authorization(authentication)?;
        let anonymous = authorization.is_none();
        let response = self.http.get(GithubRequest {
            url: url.to_string(),
            accept,
            authorization,
        })?;
        if !(200..300).contains(&response.status) {
            return Err(github_status_error(&response, url, anonymous));
        }
        Ok(response)
    }

    fn authorization(
        &self,
        authentication: GithubAuthentication,
    ) -> Result<Option<String>, String> {
        let token = match &self.token_source {
            GithubTokenSource::Fixed(token) => token.map(str::to_string),
            GithubTokenSource::Lazy(provider) => self
                .resolved_token
                .get_or_init(|| provider.get_github_token())
                .as_ref()
                .ok()
                .cloned(),
        };
        match (token, authentication) {
            (Some(token), GithubAuthentication::Optional)
            | (Some(token), GithubAuthentication::Required) => Ok(Some(format!("Bearer {token}"))),
            (None, GithubAuthentication::Optional) => Ok(None),
            (None, GithubAuthentication::Required) => match &self.token_source {
                GithubTokenSource::Lazy(_) => Err(self
                    .resolved_token
                    .get()
                    .and_then(|result| result.as_ref().err())
                    .cloned()
                    .unwrap_or_else(github_authentication_error)),
                GithubTokenSource::Fixed(None) => Err(github_authentication_error()),
                GithubTokenSource::Fixed(Some(_)) => unreachable!("fixed token was present"),
            },
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

fn github_status_error(response: &GithubResponse, url: &str, anonymous: bool) -> String {
    let detail = serde_json::from_slice::<GithubErrorResponse>(&response.body)
        .ok()
        .map(|error| error.message)
        .or_else(|| {
            let body = String::from_utf8_lossy(&response.body).trim().to_string();
            (!body.is_empty()).then_some(body)
        });
    let mut message = format!("GitHub API returned status {} for {url}", response.status);
    if let Some(detail) = detail {
        message.push_str(": ");
        message.push_str(&detail);
    }
    if anonymous && matches!(response.status, 401 | 403 | 404) {
        message.push_str(". Set GITHUB_TOKEN or run `gh auth login` to authenticate");
    }
    message
}

fn github_repo_url(source: &GithubSource, suffix: &[&str]) -> Result<url::Url, String> {
    let mut url = url::Url::parse("https://api.github.com/")
        .map_err(|error| format!("Failed to construct GitHub API URL: {error}"))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| "GitHub API base URL cannot contain path segments".to_string())?;
        segments.push("repos");
        segments.push(source.owner());
        segments.push(source.repo());
        for segment in suffix {
            segments.push(segment);
        }
    }
    Ok(url)
}

fn validate_github_api_url(raw: &str) -> Result<(), String> {
    let url =
        url::Url::parse(raw).map_err(|error| format!("Invalid GitHub API URL '{raw}': {error}"))?;
    if url.scheme() != "https"
        || url.host_str() != Some("api.github.com")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(format!(
            "Refusing to send GitHub API credentials to non-GitHub URL: {raw}"
        ));
    }
    Ok(())
}

fn parse_json<T: DeserializeOwned>(body: &[u8], url: &str) -> Result<T, String> {
    serde_json::from_slice(body)
        .map_err(|error| format!("Failed to parse GitHub response from {url}: {error}"))
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

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct FixtureGithubHttp {
    responses: std::sync::Arc<std::sync::Mutex<BTreeMap<String, GithubFixtureResponse>>>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<GithubRequest>>>,
}

#[cfg(test)]
#[derive(Clone)]
struct GithubFixtureResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[cfg(test)]
impl FixtureGithubHttp {
    pub(crate) fn json(self, url: &str, body: impl AsRef<[u8]>) -> Self {
        self.response(url, 200, body, &[])
    }

    pub(crate) fn json_with_headers(
        self,
        url: &str,
        body: impl AsRef<[u8]>,
        headers: &[(&str, String)],
    ) -> Self {
        self.response(url, 200, body, headers)
    }

    pub(crate) fn bytes(self, url: &str, body: impl AsRef<[u8]>) -> Self {
        self.response(url, 200, body, &[])
    }

    fn response(
        self,
        url: &str,
        status: u16,
        body: impl AsRef<[u8]>,
        headers: &[(&str, String)],
    ) -> Self {
        self.responses.lock().unwrap().insert(
            url.to_string(),
            GithubFixtureResponse {
                status,
                headers: headers
                    .iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
                    .collect(),
                body: body.as_ref().to_vec(),
            },
        );
        self
    }

    pub(crate) fn requests(&self) -> Vec<GithubRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl GithubHttp for FixtureGithubHttp {
    fn get(&self, request: GithubRequest) -> Result<GithubResponse, String> {
        self.requests.lock().unwrap().push(request.clone());
        let response = self
            .responses
            .lock()
            .unwrap()
            .get(request.url())
            .cloned()
            .ok_or_else(|| format!("unexpected fixture GitHub URL: {}", request.url()))?;
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

    #[test]
    fn release_listing_sends_bearer_when_token_is_present() {
        let url = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let http = FixtureGithubHttp::default().json(url, r#"[]"#);
        let api = GithubApi::new(&http, Some("test-token"));

        api.list_releases(&github_source()).unwrap();

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

        api.list_releases(&github_source()).unwrap();

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].authorization(), None);
    }

    #[test]
    fn release_listing_follows_next_link_until_exhausted() {
        let first = "https://api.github.com/repos/acme/widget/releases?per_page=100";
        let second = "https://api.github.com/repositories/123/releases?per_page=100&page=2";
        let http = FixtureGithubHttp::default()
            .json_with_headers(
                first,
                r#"[{"tag_name":"v2.0.0","assets":[]}]"#,
                &[("Link", format!("<{second}>; rel=\"next\""))],
            )
            .json(second, r#"[{"tag_name":"1.0.0","assets":[]}]"#);
        let api = GithubApi::new(&http, None);

        let releases = api.list_releases(&github_source()).unwrap();

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

        let bytes = api.download_release_asset(&github_source(), 42).unwrap();

        assert_eq!(bytes, b"wasm");
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url(), url);
        assert_eq!(requests[0].accept(), "application/octet-stream");
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

        let error = api.list_releases(&github_source()).unwrap_err();

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

        api.list_releases(&github_source()).unwrap();
        api.download_release_asset(&github_source(), 42).unwrap();

        assert_eq!(tokens.calls.load(Ordering::SeqCst), 1);
        assert!(
            http.requests()
                .iter()
                .all(|request| { request.authorization() == Some("Bearer lazy-token") })
        );
    }
}
