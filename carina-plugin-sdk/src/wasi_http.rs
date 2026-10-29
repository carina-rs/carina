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
//! - [`WasiHttpClient`] — implements the AWS SDK's `HttpClient` trait by
//!   translating its `HttpRequest`/`SdkBody` into the same neutral core,
//!   so AWS SDK operations work inside a WASM component too.
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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpClient,
    SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_runtime_api::http::Response;
use aws_smithy_types::body::SdkBody;

use wasi::http::outgoing_handler;
use wasi::http::types::{
    ErrorCode, Fields, FutureIncomingResponse, IncomingBody, Method, OutgoingBody, OutgoingRequest,
    RequestOptions, Scheme, http_error_code,
};
use wasi::io::streams::StreamError;

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
        // These request validation failures are defined before transmission.
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

    /// Return whether request bytes could have been sent.
    pub fn phase(&self) -> SendPhase {
        self.phase
    }

    /// Return the wasi:http error code when the failing API supplied one.
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

impl fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TransportFailure {}

/// Return the send phase carried by a connector error from this transport.
///
/// This walks the complete source chain. `None` means the connector error did
/// not originate from this transport and must not be interpreted as NotSent.
pub fn send_phase_of(error: &ConnectorError) -> Option<SendPhase> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = current {
        if let Some(failure) = error.downcast_ref::<TransportFailure>() {
            return Some(failure.phase());
        }
        current = error.source();
    }
    None
}

/// When `CARINA_WASI_HTTP_TRACE=1` is set in the host environment, emit a
/// per-phase wall-clock breakdown of each request to stderr.
///
/// The env var is read once on first use and cached; flipping it
/// mid-process has no effect. Off by default — the gate is a single atomic
/// load per phase when disabled, so leaving the instrumentation in place
/// costs essentially nothing.
fn trace_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("CARINA_WASI_HTTP_TRACE")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

/// An HTTP client that uses wasi:http/outgoing-handler for making requests.
///
/// This is intended to be used as the HTTP client for the AWS SDK when running
/// inside a WASM component. The host (via wasmtime-wasi-http) provides the
/// actual HTTP implementation.
#[derive(Clone)]
pub struct WasiHttpClient;

impl fmt::Debug for WasiHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasiHttpClient").finish()
    }
}

impl WasiHttpClient {
    /// Create a new WasiHttpClient.
    pub fn new() -> Self {
        Self
    }

    /// Create a SharedHttpClient for use with aws-config.
    pub fn shared() -> SharedHttpClient {
        SharedHttpClient::new(Self::new())
    }
}

impl Default for WasiHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for WasiHttpClient {
    fn http_connector(
        &self,
        settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(WasiHttpConnector {
            connect_timeout: settings.connect_timeout(),
            read_timeout: settings.read_timeout(),
        })
    }
}

/// An HTTP connector that carries SDK-supplied timeouts into wasi:http requests.
#[derive(Clone)]
struct WasiHttpConnector {
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
}

impl fmt::Debug for WasiHttpConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasiHttpConnector")
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .finish()
    }
}

impl HttpConnector for WasiHttpConnector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let options = build_request_options(self.connect_timeout, self.read_timeout);
        HttpConnectorFuture::ready(make_request(request, options))
    }
}

/// Build wasi:http RequestOptions from SDK-supplied timeouts.
///
/// wasi:http Duration is nanoseconds (u64), so we convert from std::time::Duration.
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
/// or [`WasiHttpClient`] (AWS SDK), never construct this directly, so they
/// cannot bypass the body-framing fixes in [`execute`].
struct WasiRequest {
    /// HTTP method, uppercase (`GET`, `PUT`, ...).
    method: String,
    /// Absolute request URI (scheme + authority + path + query).
    uri: String,
    /// Header name/value pairs. Values are bytes to allow non-UTF-8.
    headers: Vec<(String, Vec<u8>)>,
    /// Request body (empty `Vec` for body-less requests).
    body: Vec<u8>,
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

fn connector_error(error: TransportFailure) -> ConnectorError {
    let phase = error.phase();
    let source = Box::new(error);
    if phase == SendPhase::NotSent {
        ConnectorError::io(source)
    } else {
        ConnectorError::other(source, None)
    }
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
        body: body_bytes,
    } = request;

    let parsed = uri
        .parse::<http::Uri>()
        .map_err(|error| PreSendError::new(format!("Invalid URI: {error}")))?;

    // Classify the body into Empty / Sized so the wire framing is explicit.
    // Without this, body-less requests lose their length signal at the
    // wasi:http boundary and hyper falls back to chunked transfer encoding.
    let body = RequestBody::from_sdk_body(&body_bytes);
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
/// [`send_request`] and the AWS SDK [`WasiHttpClient`] funnel through
/// here, so the body-framing fixes (carina#3254 / #3320 / #3318) apply
/// to every caller.
fn execute(
    request: WasiRequest,
    options: Option<RequestOptions>,
) -> Result<WasiResponse, TransportFailure> {
    let trace = trace_enabled();
    let req_start = if trace { Some(Instant::now()) } else { None };
    let trace_method = if trace {
        request.method.clone()
    } else {
        String::new()
    };
    let trace_uri = if trace {
        request.uri.clone()
    } else {
        String::new()
    };

    if trace {
        // Dump request headers before any wasi:http translation. Used to
        // confirm whether body-less requests carry `content-length: 0` /
        // `transfer-encoding: chunked` / `expect: 100-continue` (the
        // body-framing hypotheses narrowed for the ~20 s S3 latency).
        let header_pairs: Vec<String> = request
            .headers
            .iter()
            .map(|(k, v)| format!("{}={}", k, String::from_utf8_lossy(v)))
            .collect();
        eprintln!(
            "carina-wasi-http-trace-headers method={} uri={} body_in={} headers=[{}]",
            trace_method,
            trace_uri,
            request.body.len(),
            header_pairs.join("; "),
        );
    }

    let PreparedRequest {
        outgoing_request,
        outgoing_body,
        body,
    } = prepare_request(request)?;
    let body_len = body.content_length();
    let t_setup_done = req_start.map(|s| s.elapsed());

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
    let t_handle_done = req_start.map(|s| s.elapsed());

    // Now ship the body. The hyper consumer is live, so each
    // `blocking_write_and_flush` chunk drains as it goes and the
    // channel stays well below its capacity bound.
    let write_error = write_body(&outgoing_body, body.as_bytes()).err();
    let t_write_body_done = req_start.map(|s| s.elapsed());
    let finish_error = OutgoingBody::finish(outgoing_body, None)
        .err()
        .map(|e| TransportFailure::from_code(format!("Failed to finish body: {e:?}"), e));
    let t_body_finish_done = req_start.map(|s| s.elapsed());
    if let Some(error) = write_error.or(finish_error) {
        return Err(body_failure_with_response_code(&future_response, error));
    }

    // Wait for the response by polling
    let pollable = future_response.subscribe();
    pollable.block();
    let t_pollable_block_done = req_start.map(|s| s.elapsed());

    let response = future_response
        .get()
        .ok_or_else(|| TransportFailure::maybe_sent("Response not ready after block", None))?
        .map_err(|()| TransportFailure::maybe_sent("Response already taken", None))?
        .map_err(|e| TransportFailure::from_code(format!("HTTP error: {e:?}"), e))?;
    let t_get_response_done = req_start.map(|s| s.elapsed());

    // Read response status
    let status = response.status();

    // Read response headers
    let response_headers = response.headers();
    let header_entries: Vec<(String, String)> = response_headers
        .entries()
        .into_iter()
        .map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned()))
        .collect();
    drop(response_headers);

    // Read response body
    let incoming_body = response
        .consume()
        .map_err(|()| TransportFailure::maybe_sent("Failed to consume response body", None))?;
    let response_bytes = read_body(&incoming_body)?;
    let _trailers = IncomingBody::finish(incoming_body);
    let t_read_body_done = req_start.map(|s| s.elapsed());

    if trace {
        // Emit a single-line breakdown so it can be greped from log files.
        // Cumulative milliseconds since execute() entry, one column per
        // phase. Phases that should be ~instant on the WASM side
        // (everything except pollable.block) flag a host-side stall when
        // they grow.
        let ms = |d: Option<Duration>| d.map(|x| x.as_millis()).unwrap_or(0);
        eprintln!(
            "carina-wasi-http-trace method={} uri={} status={} body_in={} body_out={} \
             setup_ms={} handle_ms={} write_body_ms={} body_finish_ms={} \
             pollable_block_ms={} get_response_ms={} read_body_ms={}",
            trace_method,
            trace_uri,
            status,
            body_len,
            response_bytes.len(),
            ms(t_setup_done),
            ms(t_handle_done),
            ms(t_write_body_done),
            ms(t_body_finish_done),
            ms(t_pollable_block_done),
            ms(t_get_response_done),
            ms(t_read_body_done),
        );
    }

    Ok(WasiResponse {
        status,
        headers: header_entries,
        body: response_bytes,
    })
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
    request: http::Request<Vec<u8>>,
) -> Result<http::Response<Vec<u8>>, TransportFailure> {
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
        body,
    };

    let wasi_resp = execute(wasi_req, None)?;

    let mut builder = http::Response::builder().status(wasi_resp.status);
    for (key, value) in wasi_resp.headers {
        builder = builder.header(key, value);
    }
    builder
        .body(wasi_resp.body)
        .map_err(|e| TransportFailure::maybe_sent(format!("Failed to build response: {e}"), None))
}

/// Convert an AWS SDK HttpRequest to a [`WasiRequest`], run it through the
/// wasi:http transport core, and convert the response back to the SDK's
/// `Response<SdkBody>`.
fn make_request(
    request: HttpRequest,
    options: Option<RequestOptions>,
) -> Result<Response<SdkBody>, ConnectorError> {
    let headers = request
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect();
    let wasi_req = WasiRequest {
        method: request.method().to_string(),
        uri: request.uri().to_string(),
        headers,
        body: request.body().bytes().unwrap_or(&[]).to_vec(),
    };

    let wasi_resp = execute(wasi_req, options).map_err(connector_error)?;

    // Build the AWS SDK Response
    let mut sdk_response = Response::new(
        aws_smithy_runtime_api::http::StatusCode::try_from(wasi_resp.status).map_err(|e| {
            connector_error(TransportFailure::maybe_sent(
                format!("Failed to convert response status: {e}"),
                None,
            ))
        })?,
        SdkBody::from(wasi_resp.body),
    );

    for (key, value) in wasi_resp.headers {
        sdk_response
            .headers_mut()
            .try_append(key, value)
            .map_err(|e| {
                connector_error(TransportFailure::maybe_sent(
                    format!("Failed to append response header: {e}"),
                    None,
                ))
            })?;
    }

    Ok(sdk_response)
}

/// Prefer the response future's host error over a body-stream failure.
///
/// A pre-send host failure drops the outgoing-body receiver, so a request with
/// a body can observe a closed stream before the future exposes the real DNS,
/// connect, TLS, or policy error. The body has already been finished before
/// this function runs, which lets the host task complete; once it does, the
/// future resolves and `block` cannot wait on an open body producer.
fn body_failure_with_response_code(
    future_response: &FutureIncomingResponse,
    fallback: TransportFailure,
) -> TransportFailure {
    let pollable = future_response.subscribe();
    pollable.block();
    match future_response.get() {
        Some(Ok(Err(code))) => body_failure_from_code(code, fallback),
        _ => fallback,
    }
}

fn body_failure_from_code(code: ErrorCode, fallback: TransportFailure) -> TransportFailure {
    let message = format!("HTTP error: {code:?} (after body write/finish failed: {fallback})");
    TransportFailure::from_code(message, code)
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
fn read_body(body: &IncomingBody) -> Result<Vec<u8>, TransportFailure> {
    let stream = body
        .stream()
        .map_err(|()| TransportFailure::maybe_sent("Failed to get input stream", None))?;
    let mut buf = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => buf.extend_from_slice(&chunk),
            Err(error) => match error {
                StreamError::Closed => break,
                error => {
                    let message = format!("Read error: {error:?}");
                    return Err(TransportFailure::maybe_sent(
                        message,
                        stream_error_code(&error),
                    ));
                }
            },
        }
    }
    drop(stream);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use wasi::http::types::{DnsErrorPayload, FieldSizePayload, TlsAlertReceivedPayload};

    use super::*;

    #[derive(Debug)]
    struct WrappedTransportFailure(TransportFailure);

    impl fmt::Display for WrappedTransportFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("wrapped transport failure")
        }
    }

    impl std::error::Error for WrappedTransportFailure {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

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

    #[test]
    fn send_phase_of_finds_transport_failures_and_rejects_foreign_sources() {
        let not_sent = connector_error(PreSendError::new("request was not sent").into());
        assert_eq!(send_phase_of(&not_sent), Some(SendPhase::NotSent));

        let maybe_sent = ConnectorError::other(
            Box::new(WrappedTransportFailure(TransportFailure::from_code(
                "response timed out",
                ErrorCode::ConnectionReadTimeout,
            ))),
            None,
        );
        assert_eq!(send_phase_of(&maybe_sent), Some(SendPhase::MaybeSent));

        let foreign = ConnectorError::other(
            Box::new(std::io::Error::other("foreign connector error")),
            None,
        );
        assert_eq!(send_phase_of(&foreign), None);
    }

    #[test]
    fn authoritative_response_code_is_in_body_failure_message() {
        let failure = body_failure_from_code(
            ErrorCode::HttpProtocolError,
            TransportFailure::maybe_sent("Write error: Closed", None),
        );

        assert_eq!(failure.phase(), SendPhase::MaybeSent);
        assert!(matches!(failure.code(), Some(ErrorCode::HttpProtocolError)));
        assert_eq!(
            failure.to_string(),
            "HTTP error: ErrorCode::HttpProtocolError (after body write/finish failed: Write error: Closed)"
        );
    }
}
