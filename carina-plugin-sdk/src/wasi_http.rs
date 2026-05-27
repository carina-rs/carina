//! WASM HTTP client that bridges the AWS SDK's HTTP interface to wasi:http.
//!
//! This module provides `WasiHttpClient`, which implements the AWS SDK's
//! `HttpClient` trait by delegating HTTP requests to the `wasi:http/outgoing-handler`
//! interface. This allows AWS SDK operations to work inside a WASM component
//! running on a host that provides wasi:http support.
//!
//! This module is only compiled for `target_arch = "wasm32"`.

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
    Fields, IncomingBody, Method, OutgoingBody, OutgoingRequest, RequestOptions, Scheme,
};
use wasi::io::streams::StreamError;

use crate::wasi_http_body::{
    BLOCKING_WRITE_AND_FLUSH_MAX_BYTES, RequestBody, chunks_for_blocking_write,
    inject_content_length_header,
};

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

/// Convert an AWS SDK HttpRequest to a wasi:http outgoing request, execute it,
/// and convert the response back.
fn make_request(
    request: HttpRequest,
    options: Option<RequestOptions>,
) -> Result<Response<SdkBody>, ConnectorError> {
    let trace = trace_enabled();
    let req_start = if trace { Some(Instant::now()) } else { None };
    let trace_method = if trace {
        request.method().to_string()
    } else {
        String::new()
    };
    let trace_uri = if trace {
        request.uri().to_string()
    } else {
        String::new()
    };

    if trace {
        // Dump request headers as we received them from the AWS SDK, before
        // any wasi:http translation. Used to confirm whether DELETE requests
        // carry `content-length: 0` / `transfer-encoding: chunked` /
        // `expect: 100-continue` (the body-framing hypotheses being narrowed
        // for the ~20 s S3 latency).
        let body_in = request.body().bytes().map(|b| b.len()).unwrap_or(0);
        let header_pairs: Vec<String> = request
            .headers()
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        eprintln!(
            "carina-wasi-http-trace-headers method={} uri={} body_in={} headers=[{}]",
            trace_method,
            trace_uri,
            body_in,
            header_pairs.join("; "),
        );
    }

    // Parse the URI
    let uri = request.uri().to_string();
    let parsed = uri
        .parse::<http::Uri>()
        .map_err(|e| ConnectorError::other(e.into(), None))?;

    // Classify the SDK body into Empty / Sized so the wire framing is
    // explicit. Without this, AWS SDK-emitted body-less DELETEs lose
    // their length signal at the wasi:http boundary, the host falls
    // back to `Transfer-Encoding: chunked`, and S3 sits for ~20s
    // (carina-rs/carina#3254) waiting for chunked-body bytes that
    // hyper never produces.
    let body = RequestBody::from_sdk_body(request.body().bytes().unwrap_or(&[]));

    // Build headers, injecting `content-length` if the SDK omitted it.
    let mut headers_list: Vec<(String, Vec<u8>)> = request
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect();
    inject_content_length_header(&mut headers_list, &body);
    let fields = Fields::from_list(&headers_list).map_err(|e| {
        ConnectorError::other(format!("Failed to create headers: {e:?}").into(), None)
    })?;

    // Create outgoing request
    let outgoing_req = OutgoingRequest::new(fields);

    // Set method
    let method = match request.method() {
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
    outgoing_req
        .set_method(&method)
        .map_err(|()| ConnectorError::other("Failed to set method".into(), None))?;

    // Set scheme
    let scheme = match parsed.scheme_str() {
        Some("https") => Some(Scheme::Https),
        Some("http") => Some(Scheme::Http),
        Some(other) => Some(Scheme::Other(other.to_string())),
        None => Some(Scheme::Https),
    };
    outgoing_req
        .set_scheme(scheme.as_ref())
        .map_err(|()| ConnectorError::other("Failed to set scheme".into(), None))?;

    // Set authority (host[:port])
    let authority = parsed.authority().map(|a| a.to_string());
    outgoing_req
        .set_authority(authority.as_deref())
        .map_err(|()| ConnectorError::other("Failed to set authority".into(), None))?;

    // Set path with query
    let path_and_query = parsed
        .path_and_query()
        .map(|pq| pq.to_string())
        .unwrap_or_else(|| "/".to_string());
    outgoing_req
        .set_path_with_query(Some(&path_and_query))
        .map_err(|()| ConnectorError::other("Failed to set path".into(), None))?;

    let t_setup_done = req_start.map(|s| s.elapsed());

    // Acquire the `OutgoingBody` resource **before** handing the
    // request to `outgoing_handler::handle`. `outgoing_request.body()`
    // constructs the host-side mpsc channel that backs the body, and
    // the `OutgoingBody` resource is allowed to outlive the request
    // (see `wasmtime_wasi_http::p2::types_impl`: "we could be still
    // writing to the stream after `outgoing-handler.handle` is
    // called").
    let body_len = body.content_length();
    let outgoing_body = outgoing_req
        .body()
        .map_err(|()| ConnectorError::other("Failed to get outgoing body".into(), None))?;

    // Hand the request off to the host *first* so the hyper task that
    // consumes the body channel is spawned before we start writing.
    // If we wrote the body first, the host channel (default capacity
    // `outgoing_body_buffer_chunks + 1 = 2`) would fill up with no
    // reader on the other side, and the trailing `write_ready().await`
    // inside `wasmtime-wasi-io`'s `blocking_write_and_flush` default
    // impl would block forever — the carina-rs/carina#3320 20-minute
    // host I/O hang.
    let future_response = outgoing_handler::handle(outgoing_req, options).map_err(|e| {
        ConnectorError::other(format!("outgoing-handler error: {e:?}").into(), None)
    })?;
    let t_handle_done = req_start.map(|s| s.elapsed());

    // Now ship the body. The hyper consumer is live, so each
    // `blocking_write_and_flush` chunk drains as it goes and the
    // channel stays well below its capacity bound.
    write_body(&outgoing_body, body.as_bytes())?;
    let t_write_body_done = req_start.map(|s| s.elapsed());
    OutgoingBody::finish(outgoing_body, None)
        .map_err(|e| ConnectorError::other(format!("Failed to finish body: {e:?}").into(), None))?;
    let t_body_finish_done = req_start.map(|s| s.elapsed());

    // Wait for the response by polling
    let pollable = future_response.subscribe();
    pollable.block();
    let t_pollable_block_done = req_start.map(|s| s.elapsed());

    let response = future_response
        .get()
        .ok_or_else(|| ConnectorError::other("Response not ready after block".into(), None))?
        .map_err(|()| ConnectorError::other("Response already taken".into(), None))?
        .map_err(|e| ConnectorError::other(format!("HTTP error: {e:?}").into(), None))?;
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
        .map_err(|()| ConnectorError::other("Failed to consume response body".into(), None))?;
    let response_bytes = read_body(&incoming_body)?;
    let _trailers = IncomingBody::finish(incoming_body);
    let t_read_body_done = req_start.map(|s| s.elapsed());

    if trace {
        // Emit a single-line breakdown so it can be greped from log files.
        // Cumulative milliseconds since make_request entry, one column per phase.
        // Phases that should be ~instant on the WASM side (everything except
        // pollable.block) are the ones that flag a host-side stall when they
        // grow.
        let ms = |d: Option<Duration>| d.map(|x| x.as_millis()).unwrap_or(0);
        // Columns are emitted in execution order so a reader can scan
        // left-to-right and see where the wall-clock went. The order
        // is: setup → handle (hand request off so the hyper consumer
        // is live before we write — see carina-rs/carina#3320) →
        // write_body → body_finish → pollable_block → get_response →
        // read_body.
        eprintln!(
            "carina-wasi-http-trace method={} uri={} status={} body_in={} body_out={} \
             setup_ms={} handle_ms={} write_body_ms={} body_finish_ms={} \
             pollable_block_ms={} get_response_ms={} read_body_ms={}",
            trace_method,
            trace_uri,
            u16::from(status),
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

    // Build the AWS SDK Response
    let mut sdk_response = Response::new(
        aws_smithy_runtime_api::http::StatusCode::try_from(status)
            .map_err(|e| ConnectorError::other(e.into(), None))?,
        SdkBody::from(response_bytes),
    );

    // Copy headers
    for (key, value) in header_entries {
        sdk_response
            .headers_mut()
            .try_append(key, value)
            .map_err(|e| ConnectorError::other(e.into(), None))?;
    }

    Ok(sdk_response)
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
fn write_body(body: &OutgoingBody, data: &[u8]) -> Result<(), ConnectorError> {
    let stream = body
        .write()
        .map_err(|()| ConnectorError::other("Failed to get output stream".into(), None))?;
    for chunk in chunks_for_blocking_write(data) {
        debug_assert!(chunk.len() <= BLOCKING_WRITE_AND_FLUSH_MAX_BYTES);
        stream
            .blocking_write_and_flush(chunk)
            .map_err(|e| ConnectorError::other(format!("Write error: {e:?}").into(), None))?;
    }
    drop(stream);
    Ok(())
}

/// Read all bytes from an incoming body.
fn read_body(body: &IncomingBody) -> Result<Vec<u8>, ConnectorError> {
    let stream = body
        .stream()
        .map_err(|()| ConnectorError::other("Failed to get input stream".into(), None))?;
    let mut buf = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => buf.extend_from_slice(&chunk),
            Err(StreamError::Closed) => break,
            Err(e) => {
                return Err(ConnectorError::other(
                    format!("Read error: {e:?}").into(),
                    None,
                ));
            }
        }
    }
    drop(stream);
    Ok(buf)
}
