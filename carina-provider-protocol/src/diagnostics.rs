//! Untrusted transport records. None grants presentation authority.
use crate::types::{ProviderError, ResourceId, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueOrigin {
    Desired,
    Observed,
    Unclassified,
}

pub enum DiagnosticValue {
    Typed(Value),
    Json(String),
    Masked,
}
pub struct ValueDiagnostic {
    pub id: Option<ResourceId>,
    pub path: Vec<String>,
    pub origin: ValueOrigin,
    pub input: DiagnosticValue,
}
pub struct PanicDiagnostic {
    pub message: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpTransportCode {
    Dns,
    Connect,
    Tls,
    Timeout,
    Protocol,
    Body,
}
/// HTTP evidence carries lengths, never response bodies.
///
/// ```compile_fail,E0560
/// use carina_provider_protocol::diagnostics::HttpDiagnostic;
/// let _ = HttpDiagnostic { method: "GET".into(), uri: "/".into(), headers: vec![],
///     request_body_bytes: 0, response_body_bytes: Some(0), status: Some(200),
///     transport_code: None, phases_ms: [0; 7], response_body: "raw".into() };
/// ```
pub struct HttpDiagnostic {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub request_body_bytes: u64,
    pub response_body_bytes: Option<u64>,
    pub status: Option<u16>,
    pub transport_code: Option<HttpTransportCode>,
    pub phases_ms: [u64; 7],
}
pub enum Payload {
    Failure(ProviderError),
    Diagnostic(String),
    Panic(PanicDiagnostic),
    Value(ValueDiagnostic),
    Http(HttpDiagnostic),
}
pub struct Event {
    pub severity: Level,
    pub context: Option<String>,
    pub body: Payload,
}

#[derive(Clone)]
pub struct AwsSession {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}
pub enum ProtectedMaterial {
    AwsSession(AwsSession),
    GithubToken(String),
}
impl ProtectedMaterial {
    pub fn values(&self) -> impl Iterator<Item = &str> {
        let values = match self {
            Self::AwsSession(AwsSession {
                access_key_id,
                secret_access_key,
                session_token,
            }) => [
                Some(access_key_id.as_str()),
                Some(secret_access_key.as_str()),
                session_token.as_deref(),
            ],
            Self::GithubToken(token) => [Some(token.as_str()), None, None],
        };
        values.into_iter().flatten()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationError {
    LimitExceeded,
    ClosedContext,
}
impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::LimitExceeded => "protected-material registration limit exceeded",
            Self::ClosedContext => "protected-material context is closed",
        })
    }
}
impl std::error::Error for RegistrationError {}
