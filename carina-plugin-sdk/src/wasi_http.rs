//! WASM HTTP client that bridges the AWS SDK's HTTP interface to wasi:http.
//!
//! This module provides `WasiHttpClient`, which implements the AWS SDK's
//! `HttpClient` trait by delegating HTTP requests to the `wasi:http/outgoing-handler`
//! interface. This allows AWS SDK operations to work inside a WASM component
//! running on a host that provides wasi:http support.
//!
//! This module is only compiled for `target_arch = "wasm32"`.

use std::fmt;

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
use wasi::http::types::{Fields, IncomingBody, Method, OutgoingBody, OutgoingRequest, Scheme};
use wasi::io::streams::StreamError;

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
        _settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(self.clone())
    }
}

impl HttpConnector for WasiHttpClient {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        HttpConnectorFuture::ready(make_request(request))
    }
}

/// Convert an AWS SDK HttpRequest to a wasi:http outgoing request, execute it,
/// and convert the response back.
fn make_request(request: HttpRequest) -> Result<Response<SdkBody>, ConnectorError> {
    // Parse the URI
    let uri = request.uri().to_string();
    let parsed = uri
        .parse::<http::Uri>()
        .map_err(|e| ConnectorError::other(e.into(), None))?;

    // Build headers
    let headers_list: Vec<(String, Vec<u8>)> = request
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect();
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

    // Write the request body
    let body_bytes = request.body().bytes().unwrap_or(&[]).to_vec();
    let outgoing_body = outgoing_req
        .body()
        .map_err(|()| ConnectorError::other("Failed to get outgoing body".into(), None))?;
    write_body(&outgoing_body, &body_bytes)?;
    OutgoingBody::finish(outgoing_body, None)
        .map_err(|e| ConnectorError::other(format!("Failed to finish body: {e:?}").into(), None))?;

    // Send the request
    let future_response = outgoing_handler::handle(outgoing_req, None).map_err(|e| {
        ConnectorError::other(format!("outgoing-handler error: {e:?}").into(), None)
    })?;

    // Wait for the response by polling
    let pollable = future_response.subscribe();
    pollable.block();

    let response = future_response
        .get()
        .ok_or_else(|| ConnectorError::other("Response not ready after block".into(), None))?
        .map_err(|()| ConnectorError::other("Response already taken".into(), None))?
        .map_err(|e| ConnectorError::other(format!("HTTP error: {e:?}").into(), None))?;

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

/// Write bytes to an outgoing body.
fn write_body(body: &OutgoingBody, data: &[u8]) -> Result<(), ConnectorError> {
    let stream = body
        .write()
        .map_err(|()| ConnectorError::other("Failed to get output stream".into(), None))?;
    if !data.is_empty() {
        stream
            .blocking_write_and_flush(data)
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
