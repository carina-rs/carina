use super::{GitHubError, GitHubRequest, GitHubResponse, GitHubTransport, MAX_BYTES, Method};
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
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| GitHubError::Credentials("invalid token header"))?;
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
            .map_err(|_| GitHubError::Transport {
                kind: "client construction",
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

pub(super) fn append_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), GitHubError> {
    let size = body.len().saturating_add(chunk.len());
    if size > MAX_BYTES {
        return Err(GitHubError::TooLarge {
            size,
            cap: MAX_BYTES,
        });
    }
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
        let mut response = self
            .request(request, deadline)
            .send()
            .await
            .map_err(transport_error)?;
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
        if let Some(size) = response.content_length()
            && size > MAX_BYTES as u64
        {
            return Err(GitHubError::TooLarge {
                size: usize::try_from(size).unwrap_or(usize::MAX),
                cap: MAX_BYTES,
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            append_chunk(&mut body, &chunk)?;
        }
        body = self.response_body(method, status, body);
        Ok(GitHubResponse {
            status,
            headers,
            body,
        })
    }
}
fn transport_error(error: reqwest::Error) -> GitHubError {
    if error.is_timeout() {
        GitHubError::Deadline
    } else {
        GitHubError::Transport {
            kind: if error.is_connect() {
                "connection"
            } else {
                "request or response"
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut bytes = vec![0; MAX_BYTES - 1];
        append_chunk(&mut bytes, &[0]).unwrap();
        assert!(
            matches!(append_chunk(&mut bytes,&[1]),Err(GitHubError::TooLarge {size,cap:MAX_BYTES}) if size==MAX_BYTES+1)
        );
        assert_eq!(bytes.len(), MAX_BYTES);
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
}
