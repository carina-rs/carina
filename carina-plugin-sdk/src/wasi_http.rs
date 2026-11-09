//! WASM HTTP transport over `wasi:http/outgoing-handler`.
//!
//! The wasi:http transport core ([`execute`]) is provider-agnostic: it
//! takes a neutral [`WasiRequest`] (`http`-crate-shaped: method, uri,
//! headers, body) and returns a [`WasiResponse`]. Two adapters sit on
//! top:
//!
//! - [`send_request`] — the **generic** entry point any provider can
//!   use to issue a plain HTTP request with only the `http` crate's
//!   types (no AWS SDK dependency). This is how non-AWS providers (e.g.
//!   the GitHub provider) talk to their REST APIs.
//! - `AwsSdkParts` (with feature `aws`) — installs the registered AWS credentials provider and its
//!   `WasiHttpClient` on one config builder. The client implements `HttpClient` by
//!   translating its `HttpRequest`/`SdkBody` into the same neutral core,
//!   so AWS SDK operations work inside a WASM component too.
//!
//! Both adapters send fully buffered bodies with explicit `Content-Length`
//! framing. The synchronous AWS connector rejects streaming or already-taken
//! SDK bodies as user errors with `UnbufferedRequestBody` in the source chain,
//! before sending anything. It does not consume or poll those bodies.
//!
//! Keeping the wasi:http body-framing fixes (carina#3254 Content-Length,
//! carina#3320 handle-before-write ordering, carina#3318 blocking-write
//! chunking) in the single [`execute`] core means every caller — AWS SDK
//! or generic — gets them; a new provider cannot reintroduce the bugs by
//! re-implementing the transport.
//!
//! Production code compiles this module only for `target_arch = "wasm32"`;
//! native test builds compile it as well to exercise connector classification.

use std::fmt;
#[cfg(feature = "aws")]
use std::time::Duration;
use std::time::Instant;

#[cfg(feature = "aws")]
use crate::protected_material::AwsSessionSource;
use crate::protected_material::RegisteredRequest;
#[cfg(feature = "aws")]
use aws_credential_types::provider::{
    ProvideCredentials, SharedCredentialsProvider, error::CredentialsError, future,
};
#[cfg(feature = "aws")]
use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpClient,
    SharedHttpConnector,
};
#[cfg(feature = "aws")]
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
#[cfg(feature = "aws")]
use aws_smithy_runtime_api::client::result::ConnectorError;
#[cfg(feature = "aws")]
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
#[cfg(feature = "aws")]
use aws_smithy_runtime_api::http::Response;
#[cfg(feature = "aws")]
use aws_smithy_types::body::SdkBody;
#[cfg(feature = "aws")]
use std::sync::Arc;

use wasi::http::outgoing_handler;
use wasi::http::types::{
    ErrorCode, Fields, IncomingBody, IncomingResponse, Method, OutgoingBody, OutgoingRequest,
    RequestOptions, Scheme, http_error_code,
};
use wasi::io::streams::StreamError;

#[cfg(feature = "aws")]
pub use crate::wasi_http_body::UnbufferedRequestBody;
use crate::wasi_http_body::{
    BLOCKING_WRITE_AND_FLUSH_MAX_BYTES, RequestBody, chunks_for_blocking_write,
    inject_content_length_header,
};

/// Whether a failed transport operation could have sent request bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SendPhase {
    /// The request did not leave the guest and is safe to resend.
    NotSent,
    /// Some request bytes may have been sent.
    MaybeSent,
}

/// Classify a wasi:http error by whether the request could have been sent.
///
/// Keep this match exhaustive. A newly added wasi:http error must make an
/// explicit resend-safety decision here rather than silently inheriting one.
pub fn send_phase(code: &ErrorCode) -> SendPhase {
    match code {
        // DNS has not established a connection.
        ErrorCode::DnsTimeout => SendPhase::NotSent,
        ErrorCode::DnsError(..) => SendPhase::NotSent,
        // Destination selection failures happen before connection setup.
        ErrorCode::DestinationNotFound => SendPhase::NotSent,
        ErrorCode::DestinationUnavailable => SendPhase::NotSent,
        ErrorCode::DestinationIpProhibited => SendPhase::NotSent,
        ErrorCode::DestinationIpUnroutable => SendPhase::NotSent,
        // Connection establishment failed before HTTP request bytes existed.
        ErrorCode::ConnectionRefused => SendPhase::NotSent,
        // A terminated connection may already have carried request bytes.
        ErrorCode::ConnectionTerminated => SendPhase::MaybeSent,
        // The carina host uses ConnectionTimeout only for DNS, TCP-connect,
        // and HTTP/1-handshake timeouts. First-byte timeouts use
        // ConnectionReadTimeout instead.
        ErrorCode::ConnectionTimeout => SendPhase::NotSent,
        // Read and write timeouts can happen after a partial request write.
        ErrorCode::ConnectionReadTimeout => SendPhase::MaybeSent,
        ErrorCode::ConnectionWriteTimeout => SendPhase::MaybeSent,
        // A connection limit can be reported after the implementation accepted work.
        ErrorCode::ConnectionLimitReached => SendPhase::MaybeSent,
        // This is NotSent only because the carina host emits TlsProtocolError
        // exclusively from connector.connect during its TLS handshake. The
        // matching send_request_handler call-site comment documents the host
        // side of this contract.
        ErrorCode::TlsProtocolError => SendPhase::NotSent,
        // Certificate validation precedes HTTP request transmission.
        ErrorCode::TlsCertificateError => SendPhase::NotSent,
        // An alert can arrive after a TLS stream has been established.
        ErrorCode::TlsAlertReceived(..) => SendPhase::MaybeSent,
        // The carina host never returns HttpRequestDenied: host-policy denials
        // trap the guest (carina#4100). This arm applies only to foreign hosts.
        ErrorCode::HttpRequestDenied => SendPhase::NotSent,
        // Length/body failures can be discovered after a partial body write.
        ErrorCode::HttpRequestLengthRequired => SendPhase::MaybeSent,
        ErrorCode::HttpRequestBodySize(..) => SendPhase::MaybeSent,
        ErrorCode::HttpRequestMethodInvalid => SendPhase::NotSent,
        ErrorCode::HttpRequestUriInvalid => SendPhase::NotSent,
        ErrorCode::HttpRequestUriTooLong => SendPhase::NotSent,
        // Header/trailer size failures are conservatively treated as possibly sent.
        ErrorCode::HttpRequestHeaderSectionSize(..) => SendPhase::MaybeSent,
        ErrorCode::HttpRequestHeaderSize(..) => SendPhase::MaybeSent,
        ErrorCode::HttpRequestTrailerSectionSize(..) => SendPhase::MaybeSent,
        ErrorCode::HttpRequestTrailerSize(..) => SendPhase::MaybeSent,
        // Every response-side failure necessarily follows request handoff.
        ErrorCode::HttpResponseIncomplete => SendPhase::MaybeSent,
        ErrorCode::HttpResponseHeaderSectionSize(..) => SendPhase::MaybeSent,
        ErrorCode::HttpResponseHeaderSize(..) => SendPhase::MaybeSent,
        ErrorCode::HttpResponseBodySize(..) => SendPhase::MaybeSent,
        ErrorCode::HttpResponseTrailerSectionSize(..) => SendPhase::MaybeSent,
        ErrorCode::HttpResponseTrailerSize(..) => SendPhase::MaybeSent,
        ErrorCode::HttpResponseTransferCoding(..) => SendPhase::MaybeSent,
        ErrorCode::HttpResponseContentCoding(..) => SendPhase::MaybeSent,
        ErrorCode::HttpResponseTimeout => SendPhase::MaybeSent,
        ErrorCode::HttpUpgradeFailed => SendPhase::MaybeSent,
        // The host uses this for send/body failures; it is intentionally conservative.
        ErrorCode::HttpProtocolError => SendPhase::MaybeSent,
        // The specification does not define these as pre-connection failures.
        ErrorCode::LoopDetected => SendPhase::MaybeSent,
        ErrorCode::ConfigurationError => SendPhase::MaybeSent,
        ErrorCode::InternalError(..) => SendPhase::MaybeSent,
    }
}

/// An error produced while constructing a request before host handoff.
#[derive(Debug)]
struct PreSendError {
    message: String,
}

impl PreSendError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// An error produced after receiving a response head.
#[derive(Debug)]
struct ResponseBodyError {
    code: Option<ErrorCode>,
    message: String,
}

impl ResponseBodyError {
    fn new(message: impl Into<String>, code: Option<ErrorCode>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A typed wasi:http transport failure retained as an AWS connector source.
#[derive(Debug)]
pub struct TransportFailure {
    phase: SendPhase,
    code: Option<ErrorCode>,
    message: String,
}
impl TransportFailure {
    fn maybe_sent(message: impl Into<String>, code: Option<ErrorCode>) -> Self {
        Self {
            phase: SendPhase::MaybeSent,
            code,
            message: message.into(),
        }
    }
    fn from_code(message: impl Into<String>, code: ErrorCode) -> Self {
        Self {
            phase: send_phase(&code),
            code: Some(code),
            message: message.into(),
        }
    }
    pub fn phase(&self) -> SendPhase {
        self.phase
    }
    pub fn code(&self) -> Option<&ErrorCode> {
        self.code.as_ref()
    }
}
impl From<PreSendError> for TransportFailure {
    fn from(error: PreSendError) -> Self {
        Self {
            phase: SendPhase::NotSent,
            code: None,
            message: error.message,
        }
    }
}
impl From<ResponseBodyError> for TransportFailure {
    fn from(error: ResponseBodyError) -> Self {
        Self {
            phase: SendPhase::MaybeSent,
            code: error.code,
            message: error.message,
        }
    }
}
impl fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for TransportFailure {}

/// Return the send phase carried by a connector error from this transport.
///
/// Only a [`TransportFailure`] that is the direct source of the connector error
/// counts. A transport failure nested deeper under another error returns
/// `None`, which callers must treat as unknown or [`SendPhase::MaybeSent`].
#[cfg(feature = "aws")]
pub fn send_phase_of(error: &ConnectorError) -> Option<SendPhase> {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<TransportFailure>())
        .map(TransportFailure::phase)
}

#[cfg(feature = "aws")]
struct RegisteredAwsCredentials {
    source: Arc<dyn AwsSessionSource>,
    diagnostics: crate::ProviderDiagnostics,
}
#[cfg(feature = "aws")]
impl RegisteredAwsCredentials {
    fn new(source: impl AwsSessionSource, diagnostics: crate::ProviderDiagnostics) -> Self {
        Self {
            source: Arc::new(source),
            diagnostics,
        }
    }
}
#[cfg(feature = "aws")]
impl std::fmt::Debug for RegisteredAwsCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<registered AWS credential source>")
    }
}
#[cfg(feature = "aws")]
impl ProvideCredentials for RegisteredAwsCredentials {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(async move {
            let material = self.source.current_session().await?;
            let registered = self
                .diagnostics
                .register_aws_session(material)
                .map_err(CredentialsError::provider_error)?;
            Ok(registered.into_credentials())
        })
    }
}

/// Installs an HTTP client paired with its registered credentials provider on
/// one AWS config builder. Neither component is exposed independently by this API.
/// Subsequent changes through the AWS builder remain the caller's responsibility.
///
/// A fixed registered snapshot cannot be installed as a credentials provider.
///
/// ```compile_fail,E0277
/// use aws_credential_types::provider::SharedCredentialsProvider;
/// use carina_plugin_sdk::{ProviderDiagnostics, protected_material::AwsSessionMaterial};
/// fn snapshot_provider(diagnostics: &ProviderDiagnostics, material: AwsSessionMaterial) {
///     let snapshot = diagnostics.register_aws_session(material).unwrap();
///     let _ = SharedCredentialsProvider::new(snapshot);
/// }
/// ```
/// AWS registration is internal to the refreshing credentials provider.
///
/// ```compile_fail,E0624
/// use carina_plugin_sdk::{ProviderDiagnostics, protected_material::AwsSessionMaterial};
/// fn register_snapshot(diagnostics: &ProviderDiagnostics, material: AwsSessionMaterial) {
///     let _ = diagnostics.register_aws_session(material);
/// }
/// ```
/// The acknowledged snapshot type is not part of the public API.
///
/// ```compile_fail,E0432
/// use carina_plugin_sdk::protected_material::RegisteredAwsSession;
/// fn snapshot(_: RegisteredAwsSession) {}
/// ```
/// A snapshot cannot prepare an unsigned AWS request.
///
/// ```compile_fail,E0599
/// use carina_plugin_sdk::{ProviderDiagnostics, protected_material::AwsSessionMaterial};
/// fn unsigned_request(diagnostics: &ProviderDiagnostics, material: AwsSessionMaterial) {
///     let snapshot = diagnostics.register_aws_session(material).unwrap();
///     let _ = snapshot.prepare_request(http::Request::new(Vec::new()));
/// }
/// ```
///
/// ```compile_fail,E0616
/// use carina_plugin_sdk::wasi_http::AwsSdkParts;
/// fn extract_credentials(parts: AwsSdkParts) { let _ = parts.credentials; }
/// ```
/// ```compile_fail,E0616
/// use carina_plugin_sdk::wasi_http::AwsSdkParts;
/// use aws_smithy_runtime_api::client::http::SharedHttpClient;
/// fn extract_client(parts: AwsSdkParts) -> SharedHttpClient { parts.http_client }
/// ```
///
/// ```
/// use carina_core::provider::ProviderPlan;
/// use carina_plugin_sdk::{ProviderDiagnostics, protected_material::{AwsSessionFuture, AwsSessionMaterial, AwsSessionSource}};
/// use carina_plugin_sdk::wasi_http::AwsSdkParts;
/// use aws_types::{SdkConfig, region::Region};
/// struct Current;
/// impl AwsSessionSource for Current {
///     fn current_session(&self) -> AwsSessionFuture<'_> {
///         Box::pin(async {
///             let credentials = aws_credential_types::Credentials::new(
///                 "access", "secret", None, None, "example-source",
///             );
///             Ok(AwsSessionMaterial::new(credentials))
///         })
///     }
/// }
/// let session = ProviderPlan::state_only().session("aws", None, None);
/// let loaded = SdkConfig::builder().region(Region::new("us-east-1")).build();
/// let config = AwsSdkParts::new(Current, ProviderDiagnostics::from_session(&session))
///     .apply(loaded.into_builder()).build();
/// assert_eq!(config.region().unwrap().as_ref(), "us-east-1");
/// assert!(config.credentials_provider().is_some());
/// assert!(config.http_client().is_some());
/// ```
#[cfg(feature = "aws")]
pub struct AwsSdkParts {
    credentials: SharedCredentialsProvider,
    http_client: SharedHttpClient,
}
#[cfg(feature = "aws")]
impl AwsSdkParts {
    pub fn new(source: impl AwsSessionSource, diagnostics: crate::ProviderDiagnostics) -> Self {
        let credentials = SharedCredentialsProvider::new(RegisteredAwsCredentials::new(
            source,
            diagnostics.clone(),
        ));
        let http_client = WasiHttpClient::shared(diagnostics);
        Self {
            credentials,
            http_client,
        }
    }

    /// Consume the pair, replacing both components on the same builder.
    pub fn apply(self, builder: aws_types::sdk_config::Builder) -> aws_types::sdk_config::Builder {
        builder
            .credentials_provider(self.credentials)
            .http_client(self.http_client)
    }
}

/// An HTTP client that uses wasi:http/outgoing-handler for making requests.
///
/// This is intended to be used as the HTTP client for the AWS SDK when running
/// inside a WASM component. The host (via wasmtime-wasi-http) provides the
/// actual HTTP implementation.
/// Clients are obtained together with their registered signing credentials.
///
/// ```compile_fail,E0624
/// use carina_plugin_sdk::{ProviderDiagnostics, wasi_http::WasiHttpClient};
/// fn bypass(diagnostics: ProviderDiagnostics) { let _ = WasiHttpClient::new(diagnostics); }
/// ```
/// ```compile_fail,E0624
/// use carina_plugin_sdk::{ProviderDiagnostics, wasi_http::WasiHttpClient};
/// fn bypass(diagnostics: ProviderDiagnostics) { let _ = WasiHttpClient::shared(diagnostics); }
/// ```
#[derive(Clone)]
#[cfg(feature = "aws")]
pub struct WasiHttpClient {
    diagnostics: crate::ProviderDiagnostics,
}

#[cfg(feature = "aws")]
impl fmt::Debug for WasiHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasiHttpClient").finish()
    }
}

#[cfg(feature = "aws")]
impl WasiHttpClient {
    /// Create a new WasiHttpClient.
    fn new(diagnostics: crate::ProviderDiagnostics) -> Self {
        Self { diagnostics }
    }

    /// Create a SharedHttpClient for use with aws-config.
    fn shared(diagnostics: crate::ProviderDiagnostics) -> SharedHttpClient {
        SharedHttpClient::new(Self::new(diagnostics))
    }
}

#[cfg(feature = "aws")]
impl HttpClient for WasiHttpClient {
    fn http_connector(
        &self,
        settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(WasiHttpConnector {
            connect_timeout: settings.connect_timeout(),
            read_timeout: settings.read_timeout(),
            diagnostics: self.diagnostics.clone(),
        })
    }
}

/// An HTTP connector that carries SDK-supplied timeouts into wasi:http requests.
#[derive(Clone)]
#[cfg(feature = "aws")]
struct WasiHttpConnector {
    diagnostics: crate::ProviderDiagnostics,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
}

#[cfg(feature = "aws")]
impl fmt::Debug for WasiHttpConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasiHttpConnector")
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .finish()
    }
}

#[cfg(feature = "aws")]
impl HttpConnector for WasiHttpConnector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let options = build_request_options(self.connect_timeout, self.read_timeout);
        HttpConnectorFuture::ready(make_request(request, options, &self.diagnostics))
    }
}

/// Build wasi:http RequestOptions from SDK-supplied timeouts.
///
/// wasi:http Duration is nanoseconds (u64), so we convert from std::time::Duration.
#[cfg(feature = "aws")]
fn build_request_options(
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
) -> Option<RequestOptions> {
    if connect_timeout.is_none() && read_timeout.is_none() {
        return None;
    }
    let opts = RequestOptions::new();
    if let Some(t) = connect_timeout {
        let _ = opts.set_connect_timeout(Some(t.as_nanos() as u64));
    }
    if let Some(t) = read_timeout {
        let _ = opts.set_first_byte_timeout(Some(t.as_nanos() as u64));
        let _ = opts.set_between_bytes_timeout(Some(t.as_nanos() as u64));
    }
    Some(opts)
}

/// A provider-agnostic HTTP request for the wasi:http transport core.
///
/// Internal to this module — callers go through [`send_request`] (generic)
/// or `WasiHttpClient` (AWS SDK, feature `aws`), never construct this directly, so they
/// cannot bypass the body-framing fixes in [`execute`].
struct WasiRequest {
    /// HTTP method, uppercase (`GET`, `PUT`, ...).
    method: String,
    /// Absolute request URI (scheme + authority + path + query).
    uri: String,
    /// Header name/value pairs. Values are bytes to allow non-UTF-8.
    headers: Vec<(String, Vec<u8>)>,
    /// Fully buffered request body, classified for explicit length framing.
    body: RequestBody,
}

/// The response from the wasi:http transport core. Internal to this module.
struct WasiResponse {
    /// HTTP status code.
    status: u16,
    /// Response header name/value pairs (values lossy-UTF-8 decoded).
    headers: Vec<(String, String)>,
    /// Response body bytes.
    body: Vec<u8>,
}

#[cfg(feature = "aws")]
fn connector_error(error: TransportFailure) -> ConnectorError {
    let phase = error.phase();
    let source = Box::new(error);
    if phase == SendPhase::NotSent {
        ConnectorError::io(source)
    } else {
        ConnectorError::other(source, None)
    }
}

/// Build a transport connector error for downstream native tests.
///
/// This test-only helper exists so downstream tests can exercise the
/// [`SendPhase::NotSent`] branch using the same construction as production.
#[cfg(feature = "test-utils")]
pub fn transport_connector_error_for_test(phase: SendPhase) -> ConnectorError {
    let failure = match phase {
        SendPhase::NotSent => PreSendError::new("test request was not sent").into(),
        SendPhase::MaybeSent => {
            TransportFailure::maybe_sent("test request may have been sent", None)
        }
    };
    connector_error(failure)
}

struct PreparedRequest {
    outgoing_request: OutgoingRequest,
    outgoing_body: OutgoingBody,
    body: RequestBody,
}

/// Construct all wasi:http request resources before handing the request to the
/// host. Every failure in this function is structurally pre-send.
fn prepare_request(request: WasiRequest) -> Result<PreparedRequest, PreSendError> {
    let WasiRequest {
        method,
        uri,
        mut headers,
        body,
    } = request;

    let parsed = uri
        .parse::<http::Uri>()
        .map_err(|error| PreSendError::new(format!("Invalid URI: {error}")))?;

    // Preserve the body's explicit length signal so body-less requests do not
    // fall back to chunked transfer encoding at the wasi:http boundary.
    inject_content_length_header(&mut headers, &body);
    let fields = Fields::from_list(&headers)
        .map_err(|error| PreSendError::new(format!("Failed to create headers: {error:?}")))?;
    let outgoing_request = OutgoingRequest::new(fields);

    let method = match method.as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        "HEAD" => Method::Head,
        "PATCH" => Method::Patch,
        "OPTIONS" => Method::Options,
        "TRACE" => Method::Trace,
        "CONNECT" => Method::Connect,
        other => Method::Other(other.to_string()),
    };
    outgoing_request
        .set_method(&method)
        .map_err(|()| PreSendError::new("Failed to set method"))?;

    let scheme = match parsed.scheme_str() {
        Some("https") => Some(Scheme::Https),
        Some("http") => Some(Scheme::Http),
        Some(other) => Some(Scheme::Other(other.to_string())),
        None => Some(Scheme::Https),
    };
    outgoing_request
        .set_scheme(scheme.as_ref())
        .map_err(|()| PreSendError::new("Failed to set scheme"))?;

    let authority = parsed.authority().map(|authority| authority.to_string());
    outgoing_request
        .set_authority(authority.as_deref())
        .map_err(|()| PreSendError::new("Failed to set authority"))?;

    let path_and_query = parsed
        .path_and_query()
        .map(|path| path.to_string())
        .unwrap_or_else(|| "/".to_string());
    outgoing_request
        .set_path_with_query(Some(&path_and_query))
        .map_err(|()| PreSendError::new("Failed to set path"))?;

    // Acquire the body before host handoff. The resulting resource is allowed
    // to outlive OutgoingRequest while the host consumes its backing channel.
    let outgoing_body = outgoing_request
        .body()
        .map_err(|()| PreSendError::new("Failed to get outgoing body"))?;

    Ok(PreparedRequest {
        outgoing_request,
        outgoing_body,
        body,
    })
}

/// The single wasi:http transport core. Both the generic
/// [`send_request`] and the AWS SDK `WasiHttpClient` (feature `aws`) funnel through
/// here, so the body-framing fixes (carina#3254 / #3320 / #3318) apply
/// to every caller.
struct WasiTransportRequest {
    request: WasiRequest,
    #[cfg(target_arch = "wasm32")]
    diagnostics: crate::ProviderDiagnostics,
}
fn phase_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}
fn execute(
    transport: WasiTransportRequest,
    options: Option<RequestOptions>,
) -> Result<WasiResponse, TransportFailure> {
    let WasiTransportRequest {
        request,
        #[cfg(target_arch = "wasm32")]
        diagnostics,
    } = transport;
    execute_observed(
        request,
        |request, phases| execute_request(request, options, phases),
        |event| {
            #[cfg(target_arch = "wasm32")]
            diagnostics.http(event);
            #[cfg(not(target_arch = "wasm32"))]
            let _ = event;
        },
    )
}
fn execute_observed(
    request: WasiRequest,
    send: impl FnOnce(WasiRequest, &mut [u64; 7]) -> Result<WasiResponse, TransportFailure>,
    mut emit: impl FnMut(carina_provider_protocol::diagnostics::HttpDiagnostic),
) -> Result<WasiResponse, TransportFailure> {
    let (method, uri, request_body_bytes) = (
        request.method.clone(),
        request.uri.clone(),
        request.body.content_length() as u64,
    );
    let headers = request.headers.clone();
    let mut phases_ms = [0; 7];
    let result = send(request, &mut phases_ms);
    emit(carina_provider_protocol::diagnostics::HttpDiagnostic {
        method,
        uri,
        headers,
        request_body_bytes,
        response_body_bytes: result
            .as_ref()
            .ok()
            .map(|response| response.body.len() as u64),
        status: result.as_ref().ok().map(|response| response.status),
        transport_code: result
            .as_ref()
            .err()
            .and_then(|error| error.code().map(trace_transport_code)),
        phases_ms,
    });
    result
}

fn trace_transport_code(
    code: &ErrorCode,
) -> carina_provider_protocol::diagnostics::HttpTransportCode {
    use carina_provider_protocol::diagnostics::HttpTransportCode as Code;
    match code {
        ErrorCode::DnsTimeout | ErrorCode::DnsError(_) => Code::Dns,
        ErrorCode::DestinationNotFound
        | ErrorCode::DestinationUnavailable
        | ErrorCode::DestinationIpProhibited
        | ErrorCode::DestinationIpUnroutable
        | ErrorCode::ConnectionRefused
        | ErrorCode::ConnectionTerminated
        | ErrorCode::ConnectionLimitReached => Code::Connect,
        ErrorCode::ConnectionTimeout
        | ErrorCode::ConnectionReadTimeout
        | ErrorCode::ConnectionWriteTimeout
        | ErrorCode::HttpResponseTimeout => Code::Timeout,
        ErrorCode::TlsProtocolError
        | ErrorCode::TlsCertificateError
        | ErrorCode::TlsAlertReceived(_) => Code::Tls,
        ErrorCode::HttpRequestBodySize(_)
        | ErrorCode::HttpResponseBodySize(_)
        | ErrorCode::HttpResponseIncomplete => Code::Body,
        ErrorCode::HttpRequestDenied
        | ErrorCode::HttpRequestLengthRequired
        | ErrorCode::HttpRequestMethodInvalid
        | ErrorCode::HttpRequestUriInvalid
        | ErrorCode::HttpRequestUriTooLong
        | ErrorCode::HttpRequestHeaderSectionSize(_)
        | ErrorCode::HttpRequestHeaderSize(_)
        | ErrorCode::HttpRequestTrailerSectionSize(_)
        | ErrorCode::HttpRequestTrailerSize(_)
        | ErrorCode::HttpResponseHeaderSectionSize(_)
        | ErrorCode::HttpResponseHeaderSize(_)
        | ErrorCode::HttpResponseTrailerSectionSize(_)
        | ErrorCode::HttpResponseTrailerSize(_)
        | ErrorCode::HttpResponseTransferCoding(_)
        | ErrorCode::HttpResponseContentCoding(_)
        | ErrorCode::HttpUpgradeFailed
        | ErrorCode::HttpProtocolError
        | ErrorCode::LoopDetected
        | ErrorCode::ConfigurationError
        | ErrorCode::InternalError(_) => Code::Protocol,
    }
}

fn execute_request(
    request: WasiRequest,
    options: Option<RequestOptions>,
    phases_ms: &mut [u64; 7],
) -> Result<WasiResponse, TransportFailure> {
    #[cfg(all(test, feature = "aws", not(target_arch = "wasm32")))]
    if let Some(response) = tests::capture_native_request(&request) {
        return response;
    }
    let req_start = Instant::now();
    let PreparedRequest {
        outgoing_request,
        outgoing_body,
        body,
    } = prepare_request(request)?;
    phases_ms[0] = phase_ms(req_start);

    // Hand the request off to the host *first* so the hyper task that
    // consumes the body channel is spawned before we start writing.
    // If we wrote the body first, the host channel (default capacity
    // `outgoing_body_buffer_chunks + 1 = 2`) would fill up with no
    // reader on the other side, and the trailing `write_ready().await`
    // inside `wasmtime-wasi-io`'s `blocking_write_and_flush` default
    // impl would block forever — the carina-rs/carina#3320 20-minute
    // host I/O hang.
    let future_response = outgoing_handler::handle(outgoing_request, options)
        .map_err(|e| TransportFailure::from_code(format!("outgoing-handler error: {e:?}"), e))?;
    phases_ms[1] = phase_ms(req_start);

    // Now ship the body. The hyper consumer is live, so each
    // `blocking_write_and_flush` chunk drains as it goes and the
    // channel stays well below its capacity bound.
    let write_error = write_body(&outgoing_body, body.as_bytes()).err();
    phases_ms[2] = phase_ms(req_start);
    // Close the producer even after a failed write, before waiting for the
    // response, so the host cannot remain blocked on an open request body.
    let finish_error = OutgoingBody::finish(outgoing_body, None)
        .err()
        .map(|e| TransportFailure::from_code(format!("Failed to finish body: {e:?}"), e));
    phases_ms[3] = phase_ms(req_start);
    // Wait for the response by polling
    let pollable = future_response.subscribe();
    pollable.block();
    phases_ms[4] = phase_ms(req_start);

    let response = resolve_response(future_response.get(), write_error.or(finish_error))?;
    phases_ms[5] = phase_ms(req_start);

    let response = read_response(response)?;
    phases_ms[6] = phase_ms(req_start);

    Ok(response)
}

/// Issue a plain HTTP request over wasi:http, using only `http`-crate
/// types — no AWS SDK dependency.
///
/// This is the generic transport for providers that talk to a REST API
/// directly (e.g. the GitHub provider). The request body is taken by
/// value; the response body is fully buffered into the returned
/// `http::Response<Vec<u8>>`.
///
/// Errors retain their typed send phase so non-AWS providers can make the
/// same resend-safety decision as the AWS SDK path. The body-framing fixes in
/// [`execute`] (carina#3254 / #3320 / #3318) apply to both paths.
pub fn send_request(
    registered: RegisteredRequest,
) -> Result<http::Response<Vec<u8>>, TransportFailure> {
    let RegisteredRequest {
        request,
        #[cfg(target_arch = "wasm32")]
        diagnostics,
    } = registered;
    let (parts, body) = request.into_parts();
    let headers = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
        .collect();
    let wasi_req = WasiRequest {
        method: parts.method.as_str().to_string(),
        uri: parts.uri.to_string(),
        headers,
        body: body.into(),
    };

    build_http_response(execute(
        WasiTransportRequest {
            request: wasi_req,
            #[cfg(target_arch = "wasm32")]
            diagnostics,
        },
        None,
    )?)
    .map_err(Into::into)
}

fn build_http_response(
    wasi_resp: WasiResponse,
) -> Result<http::Response<Vec<u8>>, ResponseBodyError> {
    let mut builder = http::Response::builder().status(wasi_resp.status);
    for (key, value) in wasi_resp.headers {
        builder = builder.header(key, value);
    }
    builder
        .body(wasi_resp.body)
        .map_err(|e| ResponseBodyError::new(format!("Failed to build response: {e}"), None))
}

/// Convert an AWS SDK HttpRequest to a [`WasiRequest`], run it through the
/// wasi:http transport core, and convert the response back to the SDK's
/// `Response<SdkBody>`.
#[cfg(feature = "aws")]
fn make_request(
    request: HttpRequest,
    options: Option<RequestOptions>,
    _diagnostics: &crate::ProviderDiagnostics,
) -> Result<Response<SdkBody>, ConnectorError> {
    let body = RequestBody::try_from(request.body())
        .map_err(|error| ConnectorError::user(Box::new(error)))?;
    let headers = request
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect();
    let wasi_req = WasiRequest {
        method: request.method().to_string(),
        uri: request.uri().to_string(),
        headers,
        body,
    };

    let wasi_resp = execute(
        WasiTransportRequest {
            request: wasi_req,
            #[cfg(target_arch = "wasm32")]
            diagnostics: _diagnostics.clone(),
        },
        options,
    )
    .map_err(connector_error)?;
    build_sdk_response(wasi_resp).map_err(|error| connector_error(error.into()))
}

#[cfg(feature = "aws")]
fn build_sdk_response(wasi_resp: WasiResponse) -> Result<Response<SdkBody>, ResponseBodyError> {
    // Build the AWS SDK Response
    let mut sdk_response = Response::new(
        aws_smithy_runtime_api::http::StatusCode::try_from(wasi_resp.status).map_err(|e| {
            ResponseBodyError::new(format!("Failed to convert response status: {e}"), None)
        })?,
        SdkBody::from(wasi_resp.body),
    );

    for (key, value) in wasi_resp.headers {
        sdk_response
            .headers_mut()
            .try_append(key, value)
            .map_err(|e| {
                ResponseBodyError::new(format!("Failed to append response header: {e}"), None)
            })?;
    }

    Ok(sdk_response)
}

/// Resolve the response future's state and any body write/finish failure.
///
/// A pre-send host failure drops the outgoing-body receiver, so a request with
/// a body can observe a closed stream before the future exposes the real DNS,
/// connect, TLS, or policy error. A server can also reply before consuming the
/// whole request body, closing that same receiver. Preserve either the host
/// error or the response; only fall back to the body error if neither exists.
/// The generic response keeps this decision testable without WASI resources.
fn resolve_response<T>(
    response: Option<Result<Result<T, ErrorCode>, ()>>,
    body_failure: Option<TransportFailure>,
) -> Result<T, TransportFailure> {
    match (response, body_failure) {
        (Some(Ok(Ok(response))), _) => Ok(response),
        (Some(Ok(Err(code))), Some(fallback)) => Err(body_failure_from_code(code, fallback)),
        (Some(Ok(Err(code))), None) => Err(TransportFailure::from_code(
            format!("HTTP error: {code:?}"),
            code,
        )),
        (None | Some(Err(())), Some(fallback)) => Err(fallback),
        (None, None) => Err(TransportFailure::maybe_sent(
            "Response not ready after block",
            None,
        )),
        (Some(Err(())), None) => Err(TransportFailure::maybe_sent("Response already taken", None)),
    }
}

fn body_failure_from_code(code: ErrorCode, fallback: TransportFailure) -> TransportFailure {
    let message = format!("HTTP error: {code:?} (after body write/finish failed: {fallback})");
    TransportFailure::from_code(message, code)
}

/// Read a received response. Every failure here is structurally post-response.
fn read_response(response: IncomingResponse) -> Result<WasiResponse, ResponseBodyError> {
    let status = response.status();
    let response_headers = response.headers();
    let headers = response_headers
        .entries()
        .into_iter()
        .map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned()))
        .collect();
    drop(response_headers);

    let incoming_body = response
        .consume()
        .map_err(|()| ResponseBodyError::new("Failed to consume response body", None))?;
    let body = read_body(&incoming_body)?;
    let _trailers = IncomingBody::finish(incoming_body);

    Ok(WasiResponse {
        status,
        headers,
        body,
    })
}

fn stream_error_code(error: &StreamError) -> Option<ErrorCode> {
    match error {
        StreamError::LastOperationFailed(io_error) => http_error_code(io_error),
        StreamError::Closed => None,
    }
}

/// Write bytes to an outgoing body, respecting the wasi:io
/// `blocking-write-and-flush` per-call byte limit.
///
/// `blocking_write_and_flush` in `wasmtime-wasi-io` traps the
/// component instance when the guest passes more than
/// [`BLOCKING_WRITE_AND_FLUSH_MAX_BYTES`] bytes at once
/// (carina-rs/carina#3318). The cap is part of the wasi:io WIT
/// contract; a trap there poisons the instance, so the next guest
/// entry fails with `cannot enter component instance`. Splitting via
/// [`chunks_for_blocking_write`] keeps every call inside the
/// contract, regardless of how large the SDK-buffered body is.
fn write_body(body: &OutgoingBody, data: &[u8]) -> Result<(), TransportFailure> {
    let stream = body
        .write()
        .map_err(|()| TransportFailure::maybe_sent("Failed to get output stream", None))?;
    for chunk in chunks_for_blocking_write(data) {
        debug_assert!(chunk.len() <= BLOCKING_WRITE_AND_FLUSH_MAX_BYTES);
        stream.blocking_write_and_flush(chunk).map_err(|error| {
            let message = format!("Write error: {error:?}");
            match stream_error_code(&error) {
                Some(code) => TransportFailure::from_code(message, code),
                None => TransportFailure::maybe_sent(message, None),
            }
        })?;
    }
    drop(stream);
    Ok(())
}

/// Read all bytes from an incoming body.
///
/// The host reports truncation/framing errors as `LastOperationFailed` before
/// `Closed`, so only a clean end of stream returns the accumulated bytes.
fn read_body(body: &IncomingBody) -> Result<Vec<u8>, ResponseBodyError> {
    let stream = body
        .stream()
        .map_err(|()| ResponseBodyError::new("Failed to get input stream", None))?;
    let mut buf = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => buf.extend_from_slice(&chunk),
            Err(error) => match error {
                StreamError::Closed => break,
                error => {
                    let message = format!("Read error: {error:?}");
                    return Err(ResponseBodyError::new(message, stream_error_code(&error)));
                }
            },
        }
    }
    drop(stream);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    // The request still crosses the paired provider and real WASI connector;
    // only the final WASI host I/O is replaced in this native test harness.
    #[cfg(all(feature = "aws", not(target_arch = "wasm32")))]
    thread_local! {
        static NATIVE_REQUESTS: std::cell::RefCell<Option<Vec<super::WasiRequest>>> = const {
            std::cell::RefCell::new(None)
        };
    }
    #[cfg(all(feature = "aws", not(target_arch = "wasm32")))]
    pub(super) fn capture_native_request(
        request: &super::WasiRequest,
    ) -> Option<Result<super::WasiResponse, super::TransportFailure>> {
        NATIVE_REQUESTS.with_borrow_mut(|requests| {
            let requests = requests.as_mut()?;
            requests.push(super::WasiRequest {
                method: request.method.clone(),
                uri: request.uri.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
            });
            Some(Ok(super::WasiResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            }))
        })
    }
    #[cfg(all(feature = "aws", not(target_arch = "wasm32")))]
    struct NativeRequestCapture;
    #[cfg(all(feature = "aws", not(target_arch = "wasm32")))]
    impl NativeRequestCapture {
        fn start() -> Self {
            NATIVE_REQUESTS.with_borrow_mut(|slot| {
                assert!(slot.replace(Vec::new()).is_none());
            });
            Self
        }
        fn finish(self) -> Vec<super::WasiRequest> {
            NATIVE_REQUESTS.with_borrow_mut(|slot| slot.take().unwrap())
        }
    }
    #[cfg(all(feature = "aws", not(target_arch = "wasm32")))]
    impl Drop for NativeRequestCapture {
        fn drop(&mut self) {
            NATIVE_REQUESTS.with_borrow_mut(|slot| {
                slot.take();
            });
        }
    }

    #[cfg(all(feature = "aws", not(target_arch = "wasm32")))]
    #[tokio::test]
    async fn unbuffered_sdk_body_is_rejected_before_send() {
        let session =
            carina_core::provider::ProviderPlan::state_only().session("native", None, None);
        let connector = WasiHttpConnector {
            diagnostics: crate::ProviderDiagnostics::from_session(&session),
            connect_timeout: None,
            read_timeout: None,
        };
        for body in [
            SdkBody::taken(),
            SdkBody::from_body_1_x(SdkBody::from("streamed payload")),
            SdkBody::retryable(|| SdkBody::from_body_1_x(SdkBody::from("streamed payload"))),
        ] {
            let capture = NativeRequestCapture::start();
            let request = HttpRequest::try_from(
                http::Request::builder()
                    .method("PUT")
                    .uri("https://example.com/object")
                    .body(body)
                    .unwrap(),
            )
            .unwrap();

            let result = connector.call(request).await;
            assert!(capture.finish().is_empty(), "unbuffered body reached send");
            let error = result.expect_err("unbuffered SDK body must be rejected");
            assert!(error.is_user());
            assert!(!error.is_io());
            assert!(!error.is_timeout());
            assert!(
                error
                    .source()
                    .is_some_and(|source| source.is::<UnbufferedRequestBody>()),
                "typed unbuffered-body error must remain in the source chain"
            );
        }
    }

    #[cfg(all(feature = "aws", not(target_arch = "wasm32")))]
    #[tokio::test]
    async fn buffered_sdk_bodies_keep_their_length_framing() {
        let session =
            carina_core::provider::ProviderPlan::state_only().session("native", None, None);
        let connector = WasiHttpConnector {
            diagnostics: crate::ProviderDiagnostics::from_session(&session),
            connect_timeout: None,
            read_timeout: None,
        };
        for (body, expected) in [
            (SdkBody::empty(), RequestBody::Empty),
            (SdkBody::from(Vec::new()), RequestBody::Empty),
            (
                SdkBody::from("hello"),
                RequestBody::Sized(b"hello".to_vec()),
            ),
        ] {
            let capture = NativeRequestCapture::start();
            let request = HttpRequest::try_from(
                http::Request::builder()
                    .method("PUT")
                    .uri("https://example.com/object")
                    .body(body)
                    .unwrap(),
            )
            .unwrap();

            assert_eq!(
                connector.call(request).await.unwrap().status().as_u16(),
                200
            );
            let mut requests = capture.finish();
            assert_eq!(requests.len(), 1);
            let request = requests.first_mut().unwrap();
            assert_eq!(request.body, expected);
            inject_content_length_header(&mut request.headers, &request.body);
            assert_eq!(
                request.headers,
                vec![(
                    "content-length".to_string(),
                    expected.content_length().to_string().into_bytes(),
                )]
            );
        }
    }

    #[cfg(all(feature = "aws", not(target_arch = "wasm32")))]
    #[tokio::test]
    async fn two_thousand_signed_requests_do_not_grow_the_inventory() {
        use crate::protected_material::{AwsSessionMaterial, GithubTokenMaterial};
        use crate::{ProviderDiagnostics, aws_session_tests::StaticSource};
        use aws_credential_types::provider::ProvideCredentials;
        use aws_sigv4::{
            http_request::{SignableBody, SignableRequest, SigningSettings, sign},
            sign::v4,
        };
        use aws_smithy_runtime_api::client::{
            http::{HttpClient, HttpConnector, HttpConnectorSettings},
            orchestrator::HttpRequest,
            runtime_components::RuntimeComponentsBuilder,
        };
        use aws_smithy_types::body::SdkBody;
        use carina_core::provider::ProviderPlan;
        use std::time::{Duration, UNIX_EPOCH};

        let session = ProviderPlan::state_only().session("native", None, None);
        let diagnostics = ProviderDiagnostics::from_session(&session);
        let config = super::AwsSdkParts::new(
            StaticSource(AwsSessionMaterial::new(
                aws_credential_types::Credentials::new(
                    "long-lived-access",
                    "long-lived-key",
                    Some("long-lived-session".into()),
                    None,
                    "example-source",
                ),
            )),
            diagnostics.clone(),
        )
        .apply(Default::default())
        .build();
        let components = RuntimeComponentsBuilder::for_tests().build().unwrap();
        let connector = config
            .http_client()
            .unwrap()
            .http_connector(&HttpConnectorSettings::default(), &components);
        let github = diagnostics
            .register_github_token(GithubTokenMaterial::new("long-lived-github".into()))
            .unwrap();
        let capture = NativeRequestCapture::start();
        for index in 0..2000 {
            let mut request = http::Request::builder()
                .uri(format!("https://example.com/?Action=DescribeVpcs&list-type=2&X-Amz-Signature=sig{index}"))
                .body(Vec::new()).unwrap();
            let identity = config
                .credentials_provider()
                .unwrap()
                .provide_credentials()
                .await
                .unwrap()
                .into();
            let parameters = v4::SigningParams::builder()
                .identity(&identity)
                .region("us-east-1")
                .name("ec2")
                .time(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
                .settings(SigningSettings::default())
                .build()
                .unwrap()
                .into();
            let uri = request.uri().to_string();
            let signable = SignableRequest::new(
                request.method().as_str(),
                &uri,
                std::iter::empty(),
                SignableBody::Bytes(request.body()),
            )
            .unwrap();
            let (instructions, _signature) = sign(signable, &parameters).unwrap().into_parts();
            instructions.apply_to_request_http1x(&mut request);
            let request = HttpRequest::try_from(request.map(SdkBody::from)).unwrap();
            let response = connector.call(request).await.unwrap();
            assert_eq!(response.status().as_u16(), 200, "AWS request {index}");
            assert!(
                github.authorize(http::Request::new(Vec::new())).is_ok(),
                "GitHub request {index}"
            );
        }
        let requests = capture.finish();
        assert_eq!(requests.len(), 2000);
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(request.method, "GET");
            assert!(
                request
                    .uri
                    .ends_with(&format!("X-Amz-Signature=sig{index}"))
            );
            let authorization = request
                .headers
                .iter()
                .find(|(name, _)| name == "authorization")
                .unwrap();
            let authorization = std::str::from_utf8(&authorization.1).unwrap();
            assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=long-lived-access/"));
            assert!(authorization.contains("Signature="));
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(name, value)| name == "x-amz-security-token"
                        && value == b"long-lived-session")
            );
        }
        assert!(
            !session
                .guest_inventory()
                .would_exceed_secret_limit(&session.guest_inputs(), 4)
        );
        assert!(
            session
                .guest_inventory()
                .would_exceed_secret_limit(&session.guest_inputs(), 3)
        );
    }
    #[test]
    fn completed_http_trace_contains_headers_and_timings_once() {
        let request = super::WasiRequest {
            method: "POST".into(),
            uri: "https://example.com/".into(),
            headers: vec![("x-request-id".into(), b"request-123".to_vec())],
            body: b"request".to_vec().into(),
        };
        let mut events = Vec::new();
        let response = super::execute_observed(
            request,
            |_request, phases| {
                *phases = [1, 2, 3, 4, 5, 6, 7];
                Ok(super::WasiResponse {
                    status: 201,
                    headers: vec![],
                    body: b"response".to_vec(),
                })
            },
            |event| events.push(event),
        )
        .unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].headers,
            vec![("x-request-id".into(), b"request-123".to_vec())]
        );
        assert_eq!(events[0].phases_ms, [1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(events[0].status, Some(201));
        assert_eq!(events[0].request_body_bytes, 7);
        assert_eq!(events[0].response_body_bytes, Some(8));
        assert_eq!(events[0].transport_code, None);
    }
    #[test]
    fn completed_http_trace_reports_early_failures_once() {
        for code in [None, Some(super::ErrorCode::ConnectionRefused)] {
            let request = super::WasiRequest {
                method: "GET".into(),
                uri: "https://example.com/".into(),
                headers: vec![("x-request-id".into(), b"early".to_vec())],
                body: RequestBody::Empty,
            };
            let expected = code.as_ref().map(super::trace_transport_code);
            let mut events = Vec::new();
            let result = super::execute_observed(
                request,
                |_request, _phases| {
                    Err(super::TransportFailure::maybe_sent(
                        "early transport failure",
                        code,
                    ))
                },
                |event| events.push(event),
            );
            assert!(result.is_err());
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].headers,
                vec![("x-request-id".into(), b"early".to_vec())]
            );
            assert_eq!(events[0].status, None);
            assert_eq!(events[0].transport_code, expected);
            assert_eq!(events[0].phases_ms, [0; 7]);
        }
    }
    #[cfg(feature = "aws")]
    use std::error::Error as _;

    #[cfg(feature = "aws")]
    use wasi::http::types::{DnsErrorPayload, FieldSizePayload, TlsAlertReceivedPayload};

    use super::*;

    #[cfg(feature = "aws")]
    #[derive(Debug)]
    struct WrappedTransportFailure(TransportFailure);

    #[cfg(feature = "aws")]
    impl fmt::Display for WrappedTransportFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("wrapped transport failure")
        }
    }

    #[cfg(feature = "aws")]
    impl std::error::Error for WrappedTransportFailure {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[cfg(feature = "aws")]
    #[test]
    fn every_error_code_maps_to_the_expected_connector_category_and_source() {
        let field_size = || FieldSizePayload {
            field_name: Some("x-test".to_string()),
            field_size: Some(1),
        };
        let cases = vec![
            (ErrorCode::DnsTimeout, SendPhase::NotSent),
            (
                ErrorCode::DnsError(DnsErrorPayload {
                    rcode: Some("NXDOMAIN".to_string()),
                    info_code: Some(3),
                }),
                SendPhase::NotSent,
            ),
            (ErrorCode::DestinationNotFound, SendPhase::NotSent),
            (ErrorCode::DestinationUnavailable, SendPhase::NotSent),
            (ErrorCode::DestinationIpProhibited, SendPhase::NotSent),
            (ErrorCode::DestinationIpUnroutable, SendPhase::NotSent),
            (ErrorCode::ConnectionRefused, SendPhase::NotSent),
            (ErrorCode::ConnectionTerminated, SendPhase::MaybeSent),
            (ErrorCode::ConnectionTimeout, SendPhase::NotSent),
            (ErrorCode::ConnectionReadTimeout, SendPhase::MaybeSent),
            (ErrorCode::ConnectionWriteTimeout, SendPhase::MaybeSent),
            (ErrorCode::ConnectionLimitReached, SendPhase::MaybeSent),
            (ErrorCode::TlsProtocolError, SendPhase::NotSent),
            (ErrorCode::TlsCertificateError, SendPhase::NotSent),
            (
                ErrorCode::TlsAlertReceived(TlsAlertReceivedPayload {
                    alert_id: Some(0),
                    alert_message: Some("close notify".to_string()),
                }),
                SendPhase::MaybeSent,
            ),
            (ErrorCode::HttpRequestDenied, SendPhase::NotSent),
            (ErrorCode::HttpRequestLengthRequired, SendPhase::MaybeSent),
            (
                ErrorCode::HttpRequestBodySize(Some(1)),
                SendPhase::MaybeSent,
            ),
            (ErrorCode::HttpRequestMethodInvalid, SendPhase::NotSent),
            (ErrorCode::HttpRequestUriInvalid, SendPhase::NotSent),
            (ErrorCode::HttpRequestUriTooLong, SendPhase::NotSent),
            (
                ErrorCode::HttpRequestHeaderSectionSize(Some(1)),
                SendPhase::MaybeSent,
            ),
            (
                ErrorCode::HttpRequestHeaderSize(Some(field_size())),
                SendPhase::MaybeSent,
            ),
            (
                ErrorCode::HttpRequestTrailerSectionSize(Some(1)),
                SendPhase::MaybeSent,
            ),
            (
                ErrorCode::HttpRequestTrailerSize(field_size()),
                SendPhase::MaybeSent,
            ),
            (ErrorCode::HttpResponseIncomplete, SendPhase::MaybeSent),
            (
                ErrorCode::HttpResponseHeaderSectionSize(Some(1)),
                SendPhase::MaybeSent,
            ),
            (
                ErrorCode::HttpResponseHeaderSize(field_size()),
                SendPhase::MaybeSent,
            ),
            (
                ErrorCode::HttpResponseBodySize(Some(1)),
                SendPhase::MaybeSent,
            ),
            (
                ErrorCode::HttpResponseTrailerSectionSize(Some(1)),
                SendPhase::MaybeSent,
            ),
            (
                ErrorCode::HttpResponseTrailerSize(field_size()),
                SendPhase::MaybeSent,
            ),
            (
                ErrorCode::HttpResponseTransferCoding(Some("chunked".to_string())),
                SendPhase::MaybeSent,
            ),
            (
                ErrorCode::HttpResponseContentCoding(Some("gzip".to_string())),
                SendPhase::MaybeSent,
            ),
            (ErrorCode::HttpResponseTimeout, SendPhase::MaybeSent),
            (ErrorCode::HttpUpgradeFailed, SendPhase::MaybeSent),
            (ErrorCode::HttpProtocolError, SendPhase::MaybeSent),
            (ErrorCode::LoopDetected, SendPhase::MaybeSent),
            (ErrorCode::ConfigurationError, SendPhase::MaybeSent),
            (
                ErrorCode::InternalError(Some("test".to_string())),
                SendPhase::MaybeSent,
            ),
        ];

        for (code, expected_phase) in cases {
            let expected_code = format!("{code:?}");
            let error = connector_error(TransportFailure::from_code("transport failed", code));

            match expected_phase {
                SendPhase::NotSent => assert!(error.is_io(), "{expected_code}"),
                SendPhase::MaybeSent => {
                    assert!(error.is_other(), "{expected_code}");
                    assert!(!error.is_timeout(), "{expected_code}");
                    assert!(!error.is_io(), "{expected_code}");
                }
            }

            let failure = error
                .source()
                .and_then(|source| source.downcast_ref::<TransportFailure>())
                .expect("connector source must retain the transport failure");
            assert_eq!(failure.phase(), expected_phase, "{expected_code}");
            assert_eq!(
                failure.code().map(|code| format!("{code:?}")),
                Some(expected_code)
            );
        }
    }

    #[cfg(feature = "aws")]
    #[test]
    fn send_phase_of_accepts_direct_transport_failures_and_rejects_wrapped_or_foreign_sources() {
        let not_sent = connector_error(PreSendError::new("request was not sent").into());
        assert_eq!(send_phase_of(&not_sent), Some(SendPhase::NotSent));

        let maybe_sent = connector_error(TransportFailure::maybe_sent(
            "request may have been sent",
            None,
        ));
        assert_eq!(send_phase_of(&maybe_sent), Some(SendPhase::MaybeSent));

        let wrapped_not_sent = ConnectorError::io(Box::new(WrappedTransportFailure(
            PreSendError::new("request was not sent").into(),
        )));
        assert_eq!(send_phase_of(&wrapped_not_sent), None);

        let foreign = ConnectorError::other(
            Box::new(std::io::Error::other("foreign connector error")),
            None,
        );
        assert_eq!(send_phase_of(&foreign), None);
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn transport_connector_error_for_test_preserves_send_phase() {
        for phase in [SendPhase::NotSent, SendPhase::MaybeSent] {
            let error = transport_connector_error_for_test(phase);
            assert_eq!(send_phase_of(&error), Some(phase));
        }
    }

    #[test]
    fn response_resolution_preserves_status_headers_and_body() {
        // Both a successful HTTP status and an HTTP error are complete transport
        // responses, even if the server stopped accepting the request body.
        for status in [200, 413] {
            for body_failure in [
                None,
                Some(TransportFailure::maybe_sent("Write error: Closed", None)),
                Some(TransportFailure::from_code(
                    "Write error",
                    ErrorCode::ConnectionRefused,
                )),
            ] {
                let response = http::Response::builder()
                    .status(status)
                    .header("x-early-response", "preserved")
                    .body(b"early response body".to_vec())
                    .unwrap();
                let response = resolve_response(Some(Ok(Ok(response))), body_failure)
                    .expect("an available HTTP response must survive any body write failure");

                assert_eq!(response.status(), status);
                assert_eq!(response.headers()["x-early-response"], "preserved");
                assert_eq!(response.body(), b"early response body");
            }
        }
    }

    #[test]
    fn response_resolution_uses_host_error_with_or_without_body_failure() {
        for body_failed in [false, true] {
            for code in [
                ErrorCode::DnsTimeout,
                ErrorCode::ConnectionRefused,
                ErrorCode::HttpProtocolError,
            ] {
                let expected_phase = send_phase(&code);
                let expected_code = format!("{code:?}");
                let mut expected_message = format!("HTTP error: {code:?}");
                let body_failure = body_failed.then(|| {
                    expected_message
                        .push_str(" (after body write/finish failed: Failed to finish body)");
                    TransportFailure::from_code(
                        "Failed to finish body",
                        ErrorCode::HttpRequestBodySize(Some(4096)),
                    )
                });
                let failure =
                    resolve_response::<()>(Some(Ok(Err(code))), body_failure).unwrap_err();

                assert_eq!(failure.phase(), expected_phase);
                assert_eq!(
                    failure.code().map(|code| format!("{code:?}")),
                    Some(expected_code)
                );
                assert_eq!(failure.to_string(), expected_message);
            }
        }
    }

    #[test]
    fn response_resolution_keeps_body_failure_when_response_is_pending_or_taken() {
        for pending in [false, true] {
            for body_failure in [
                TransportFailure::maybe_sent("Write error: Closed", None),
                TransportFailure::from_code("Write error", ErrorCode::ConnectionRefused),
                TransportFailure::from_code(
                    "Failed to finish body",
                    ErrorCode::HttpRequestBodySize(Some(4096)),
                ),
            ] {
                let expected_phase = body_failure.phase();
                let expected_code = format!("{:?}", body_failure.code());
                let expected_message = body_failure.to_string();
                let response = if pending { None } else { Some(Err(())) };
                let failure = resolve_response::<()>(response, Some(body_failure)).unwrap_err();

                assert_eq!(failure.phase(), expected_phase);
                assert_eq!(format!("{:?}", failure.code()), expected_code);
                assert_eq!(failure.to_string(), expected_message);
            }
        }
    }

    #[test]
    fn response_resolution_without_body_failure_reports_pending_or_taken() {
        for (response, message) in [
            (None, "Response not ready after block"),
            (Some(Err(())), "Response already taken"),
        ] {
            let failure = resolve_response::<()>(response, None).unwrap_err();

            assert_eq!(failure.phase(), SendPhase::MaybeSent);
            assert!(failure.code().is_none());
            assert_eq!(failure.to_string(), message);
        }
    }

    #[test]
    fn response_body_error_conversion_is_maybe_sent_regardless_of_code() {
        for code in [
            None,
            Some(ErrorCode::DnsTimeout),
            Some(ErrorCode::ConnectionRefused),
            Some(ErrorCode::TlsCertificateError),
            Some(ErrorCode::HttpProtocolError),
        ] {
            let original_code = format!("{code:?}");
            let failure = TransportFailure::from(ResponseBodyError::new("Read error", code));

            assert_eq!(failure.phase(), SendPhase::MaybeSent);
            assert_eq!(format!("{:?}", failure.code()), original_code);
            assert_eq!(failure.to_string(), "Read error");
        }
    }
}
