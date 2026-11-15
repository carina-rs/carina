use super::{BodyLimit, GitHubError, GitHubRequest, GitHubResponse, GitHubTransport, Method};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::collections::BTreeMap;
use tokio::time::Instant;

/// The token lives only in a sensitive Authorization header. This transport
/// deliberately exposes neither Debug nor a configurable API endpoint.
pub(super) struct ReqwestTransport {
    client: reqwest::Client,
    authorization: HeaderValue,
}
impl ReqwestTransport {
    fn response_body(&self, method: Method, status: u16, body: Vec<u8>) -> Vec<u8> {
        if status != 200 && !(method == Method::Put && status == 201) {
            self.failure_body(body)
        } else {
            body
        }
    }
    fn failure_body(&self, body: Vec<u8>) -> Vec<u8> {
        use base64::Engine;
        let Some(token) = self
            .authorization
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|token| !token.is_empty())
        else {
            return body;
        };
        let escaped = serde_json::to_string(token).expect("token string encoding");
        String::from_utf8_lossy(&body)
            .replace(token, "<redacted>")
            .replace(&escaped[1..escaped.len() - 1], "<redacted>")
            .replace(
                &base64::engine::general_purpose::STANDARD.encode(token),
                "<redacted>",
            )
            .into_bytes()
    }
    pub(super) fn new(token: String) -> Result<Self, GitHubError> {
        let token = carina_core::github_auth::normalize_token(&token)?;
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| carina_core::github_auth::GitHubTokenError::InvalidCharacters)?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization.clone());
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static("2022-11-28"),
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(concat!("carina/", env!("CARGO_PKG_VERSION")))
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| GitHubError::Transport {
                kind: "client construction",
                error: error.without_url(),
            })?;
        Ok(Self {
            client,
            authorization,
        })
    }

    fn request(&self, request: GitHubRequest, deadline: Instant) -> reqwest::RequestBuilder {
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut url = format!("https://api.github.com{}", request.path);
        if let Some(reference) = request.reference {
            url.push_str(&format!(
                "?ref={}",
                super::super::url::encode_component(&reference)
            ));
        }
        let mut builder = self
            .client
            .request(method, url)
            .header("accept", request.accept)
            .timeout(deadline.saturating_duration_since(Instant::now()));
        if let Some(body) = request.body {
            builder = builder.json(&body);
        }
        builder
    }
}

fn append_chunk(body: &mut Vec<u8>, chunk: &[u8], limit: BodyLimit) -> Result<(), GitHubError> {
    let size = body.len().saturating_add(chunk.len());
    limit.check(size)?;
    body.extend_from_slice(chunk);
    Ok(())
}

#[async_trait]
impl GitHubTransport for ReqwestTransport {
    async fn execute(
        &self,
        request: GitHubRequest,
        deadline: Instant,
    ) -> Result<GitHubResponse, GitHubError> {
        let method = request.method;
        let limit = request.body_limit;
        let mut response = self
            .request(request, deadline)
            .send()
            .await
            .map_err(|error| transport_error(error, method))?;
        let status = response.status().as_u16();
        let mut headers = BTreeMap::new();
        for key in [
            "retry-after",
            "x-ratelimit-remaining",
            "x-ratelimit-reset",
            "x-github-request-id",
        ] {
            if let Some(value) = response.headers().get(key).and_then(|v| v.to_str().ok()) {
                // Even an untrusted response header cannot reflect the token.
                let token = self
                    .authorization
                    .to_str()
                    .ok()
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .unwrap_or("");
                let safe = if !token.is_empty() && value.contains(token) {
                    "<redacted>"
                } else {
                    value
                };
                headers.insert(
                    key.into(),
                    safe.chars()
                        .filter(|ch| ch.is_ascii() && !ch.is_control())
                        .take(256)
                        .collect(),
                );
            }
        }
        if let Some(size) = response.content_length() {
            limit.check(usize::try_from(size).unwrap_or(usize::MAX))?;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| transport_error(error, method))?
        {
            append_chunk(&mut body, &chunk, limit)?;
        }
        body = self.response_body(method, status, body);
        Ok(GitHubResponse {
            status,
            headers,
            body,
        })
    }
}
fn transport_error(error: reqwest::Error, method: Method) -> GitHubError {
    // Raw download URLs may carry GitHub-issued credentials in their query.
    // Strip the URL before retaining the error in either Display or Debug.
    let error = error.without_url();
    if error.is_timeout() {
        match method {
            Method::Get => GitHubError::TransportDeadline { error },
            Method::Put | Method::Delete => GitHubError::MutationTransportDeadline { error },
        }
    } else {
        GitHubError::Transport {
            kind: if error.is_connect() {
                "connection"
            } else {
                "request or response"
            },
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::MAX_BYTES;
    use super::*;

    #[test]
    fn github_transport_error_renders_the_reqwest_detail_once() {
        use std::error::Error;
        let source = reqwest::Client::new()
            .get("http://[invalid")
            .build()
            .unwrap_err();
        let detail = source.to_string();
        let error = transport_error(source, Method::Get);
        assert_eq!(error.to_string().matches(&detail).count(), 1, "{error}");
        assert!(error.source().is_none());
    }

    #[tokio::test]
    async fn github_transport_timeout_renders_the_reqwest_detail_once() {
        use std::error::Error;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/?token=SECRET", listener.local_addr().unwrap());
        // wall-clock-ok: a real loopback socket timeout produces reqwest's timeout error.
        let source = reqwest::Client::new()
            .get(&url)
            .timeout(std::time::Duration::from_millis(20))
            .send()
            .await
            .unwrap_err();
        assert!(source.is_timeout());
        let error = transport_error(source, Method::Get);
        assert_eq!(
            error.to_string().matches("error sending request").count(),
            1,
            "{error}"
        );
        assert!(error.source().is_none());
        assert!(!format!("{error:?} {error}").contains("SECRET"));
    }

    #[test]
    fn github_round10_transport_error_never_retains_a_raw_url_token() {
        let url = reqwest::Url::parse(
            "https://raw.githubusercontent.com/owner/repo/state.json?token=SECRET",
        )
        .unwrap();
        // Invalid headers fail while building the request, without dispatching it.
        let source = reqwest::Client::new()
            .get(url.clone())
            .header("x-test", "invalid\nheader")
            .build()
            .unwrap_err()
            .with_url(url);
        assert!(source.to_string().contains("SECRET"));
        let error = transport_error(source, Method::Get);
        assert!(!error.to_string().contains("SECRET"), "{error}");
        assert!(!format!("{error:?}").contains("SECRET"), "{error:?}");
    }

    #[test]
    fn github_transport_unexpected_status_cannot_expose_credentials() {
        let token = "unexpected-status-credential";
        let transport = ReqwestTransport::new(token.into()).unwrap();
        for status in [201, 302, 503] {
            let body = transport.response_body(Method::Get, status, token.as_bytes().to_vec());
            assert!(
                !String::from_utf8(body).unwrap().contains(token),
                "HTTP {status}"
            );
        }
        assert_eq!(
            transport.response_body(Method::Get, 200, token.as_bytes().to_vec()),
            token.as_bytes()
        );
    }

    #[test]
    fn github_transport_redacts_reflected_credentials_before_diagnostics() {
        let token = "credential-should-never-be-in-diagnostics";
        let transport = ReqwestTransport::new(token.into()).unwrap();
        let response =
            serde_json::to_vec(&serde_json::json!({"message":format!("invalid token {token}")}))
                .unwrap();
        let safe = transport.failure_body(response);
        assert!(!String::from_utf8(safe).unwrap().contains(token));
        assert!(!format!("{:?}", transport.authorization).contains(token));
    }

    #[test]
    fn github_transport_stream_cap_stops_before_appending_overflow() {
        for limit in [BodyLimit::Contents, BodyLimit::Compare] {
            let mut bytes = vec![0; limit.cap() - 1];
            append_chunk(&mut bytes, &[0], limit).unwrap();
            let error = append_chunk(&mut bytes, &[1], limit).unwrap_err();
            match limit {
                BodyLimit::Contents => assert!(
                    matches!(error, GitHubError::TooLarge { size, cap: MAX_BYTES } if size == MAX_BYTES + 1)
                ),
                BodyLimit::Compare => assert!(
                    matches!(error, GitHubError::CompareTooLarge { size, cap: super::super::MAX_COMPARE_BYTES } if size == super::super::MAX_COMPARE_BYTES + 1)
                ),
            }
            assert_eq!(bytes.len(), limit.cap());
        }
    }

    #[test]
    fn github_round3_compare_limit_applies_only_to_the_compare_endpoint() {
        for (request, cap) in [
            (
                GitHubRequest::compare("/repos/owner/repo/compare/sha...branch".into()),
                super::super::MAX_COMPARE_BYTES,
            ),
            (
                GitHubRequest::get(
                    "/repos/owner/repo/contents/compare/state.json".into(),
                    None,
                    super::super::JSON_MEDIA_TYPE,
                ),
                MAX_BYTES,
            ),
            (
                GitHubRequest::get(
                    "/repos/owner/repo/contents/state.json".into(),
                    None,
                    super::super::JSON_MEDIA_TYPE,
                ),
                MAX_BYTES,
            ),
        ] {
            assert_eq!(request.body_limit.cap(), cap, "{}", request.path);
        }
    }

    #[test]
    fn github_transport_builds_only_github_com_urls_with_encoded_refs() {
        let transport = ReqwestTransport::new("offline-token".into()).unwrap();
        let request = transport
            .request(
                GitHubRequest {
                    method: Method::Get,
                    path: "/repos/owner/repo/contents/states/prod.json".into(),
                    reference: Some("states/prod".into()),
                    accept: super::super::RAW_MEDIA_TYPE,
                    body: None,
                    body_limit: BodyLimit::Contents,
                },
                Instant::now() + std::time::Duration::from_secs(1),
            )
            .build()
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://api.github.com/repos/owner/repo/contents/states/prod.json?ref=states%2Fprod"
        );
        assert_eq!(request.headers()["accept"], super::super::RAW_MEDIA_TYPE);
    }

    #[test]
    fn github_round4_transport_normalizes_and_rejects_invalid_tokens() {
        let transport = ReqwestTransport::new(" \tprivate-token\r\n".into()).unwrap();
        assert_eq!(
            transport.authorization.to_str().unwrap(),
            "Bearer private-token"
        );
        for token in ["private\nsecret", "private\tsecret", "private\0secret"] {
            let error = match ReqwestTransport::new(token.into()) {
                Ok(_) => panic!("invalid token accepted"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .contains("token contains invalid characters"),
                "{error}"
            );
            assert!(!format!("{error:?} {error}").contains("private"));
        }
    }

    #[tokio::test]
    async fn github_round4_transport_deadline_wording_describes_the_dispatched_method() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/?token=SECRET", listener.local_addr().unwrap());
        for method in [Method::Get, Method::Put, Method::Delete] {
            // wall-clock-ok: a real loopback socket timeout produces reqwest's timeout error.
            let source = reqwest::Client::new()
                .get(&url)
                .timeout(std::time::Duration::from_millis(20))
                .send()
                .await
                .unwrap_err();
            assert!(source.is_timeout());
            let error = transport_error(source, method);
            let message = error.to_string();
            assert!(!format!("{error:?} {error}").contains("SECRET"));
            assert_eq!(
                message.contains("a dispatched mutation may have completed"),
                method != Method::Get,
                "{message}"
            );
        }
    }

    #[test]
    fn github_round4_compare_cap_survives_path_changes() {
        let mut request = GitHubRequest::compare("/repos/owner/repo/compare/sha...branch".into());
        assert_eq!(request.body_limit.cap(), super::super::MAX_COMPARE_BYTES);
        request.path = "/repos/owner/repo/contents/compare/state.json".into();
        assert_eq!(request.body_limit.cap(), super::super::MAX_COMPARE_BYTES);
    }
}
