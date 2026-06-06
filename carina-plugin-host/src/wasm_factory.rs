//! WasmProviderFactory loads a WASM component and implements ProviderFactory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use sha2::{Digest, Sha256};

use tokio::sync::Mutex;
use wasmtime::component::ResourceTable;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::cli::{WasiCli, WasiCliView as _};
use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView as _};
use wasmtime_wasi::random::WasiRandom;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView};

use carina_core::provider::{
    BoxFuture, CreateRequest, DeleteRequest, Provider, ProviderError, ProviderFactory,
    ProviderNormalizer, ProviderResult, ReadRequest, SavedAttrs, UpdateRequest,
};
use carina_core::resource::{DataSource, Resource, ResourceId, State, Value};
use carina_core::schema::{CompletionValue, ResourceSchema, TypeIdentity};
use carina_core::value::SerializationError;

use crate::wasm_bindings::CarinaProvider;
use crate::wasm_bindings_http::CarinaProviderWithHttp;
use crate::wasm_convert;

/// Wrap a `SerializationError` from a sync `core_to_wit_*` call site
/// into the matching async `BoxFuture` shape that `Provider` trait
/// methods return. Pulls the `e.to_string()` allocation out of the
/// async block so the future is `'static`.
fn early_provider_err<T: 'static>(e: SerializationError) -> BoxFuture<'static, ProviderResult<T>> {
    let msg = e.to_string();
    Box::pin(async move { Err(ProviderError::internal(msg)) })
}

/// Unwrap a `core_to_wit_*` result that **must** succeed by invariant.
/// `core_to_wit_*` rejects every `Value` variant the WASM provider
/// boundary cannot serialize — `Unknown`, `ResourceRef`,
/// `Interpolation`, and `FunctionCall`. State and saved attrs are
/// post-apply concrete values, and `normalize_desired` runs after
/// `PlanPreprocessor::prepare`'s strip-and-restore pass, so reaching
/// any of these arms is a producer-side bug. Embeds the failing
/// `SerializationError` in the panic message so the regression
/// surfaces with the actual variant + context.
fn expect_unresolvable_absent<T>(r: Result<T, SerializationError>, what: &'static str) -> T {
    r.unwrap_or_else(|e| panic!("{what} must not see an unserializable Value ({e})"))
}

// -- HTTP allow-list hooks --

/// HTTP allow-list suffix patterns for outgoing requests from WASM plugins.
///
/// Hosts matching these suffix patterns are permitted. See also
/// [`HTTP_ALLOWED_EXACT_HOSTS`] for exact-match entries.
const HTTP_ALLOWED_HOST_SUFFIXES: &[&str] = &[".amazonaws.com", ".amazonaws.com.cn"];

/// Metadata service addresses for EC2 IMDS and ECS task metadata.
const METADATA_HOSTS: &[&str] = &["169.254.169.254", "169.254.170.2"];

/// Connect timeout used when probing and capping metadata endpoint requests.
/// On EC2, IMDS responds in <10ms. A 1-second timeout lets non-EC2 environments
/// fail fast instead of hanging for the SDK's default timeout.
const METADATA_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Timeout for individual WASM plugin operations (in seconds).
///
/// Used for two layers of timeout enforcement:
/// 1. Epoch interruption: traps WASM computation that exceeds this budget.
/// 2. HTTP request cap: limits host-side HTTP calls that epochs cannot reach
///    (epochs only fire during WASM execution, not during host I/O waits).
const WASM_OPERATION_TIMEOUT_SECS: u64 = 30;

/// [`WASM_OPERATION_TIMEOUT_SECS`] as a `Duration`, used to cap per-request
/// HTTP timeouts in the [`AllowListHttpHooks`] layer.
const HTTP_API_REQUEST_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(WASM_OPERATION_TIMEOUT_SECS);

/// Build the standard wasmtime Config used for all WASM plugin engines.
///
/// Enables the component model and epoch-based interruption so that
/// long-running or stuck WASM operations can be terminated by the host.
fn build_engine_config() -> wasmtime::Config {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    config.epoch_interruption(true);
    config
}

/// Background thread that increments a wasmtime Engine's epoch once per second.
///
/// Each tick advances the epoch counter by 1. Stores with a deadline set via
/// `store.set_epoch_deadline(N)` will trap after N ticks have elapsed since the
/// deadline was set.
struct EpochTicker {
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Engine) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                engine.increment_epoch();
            }
        });
        EpochTicker {
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Returns `true` if the error message looks like a wasmtime epoch interruption trap.
fn is_epoch_trap_message(msg: &str) -> bool {
    msg.contains("interrupt") || msg.contains("epoch")
}

/// Wall-clock backstop for a single WASM provider operation.
///
/// Epoch interruption ([`WASM_OPERATION_TIMEOUT_SECS`] via
/// `store.set_epoch_deadline`) can only trap WASM *computation* — it does
/// not fire while the guest is parked in a host-side I/O wait such as a
/// `wasi:http` request whose response never completes (see the
/// [`WASM_OPERATION_TIMEOUT_SECS`] doc-comment). When that happens the
/// future hangs forever, the per-provider store `Mutex` stays held, every
/// other concurrent operation on the same provider blocks behind it, and
/// `Ctrl+C` cannot unwind a future parked inside a `wasmtime` call
/// (carina#3106).
///
/// This wraps the whole operation — both acquiring the store lock and the
/// guest call — in a `tokio::time::timeout` so a stuck host-side wait is
/// converted into a [`ProviderError::timeout`] instead of hanging
/// indefinitely.
///
/// # Sizing — why this is 20 minutes, not the ~30s epoch budget
///
/// This is a wall-clock bound on a *whole provider operation*, not a
/// single API call. A provider `create`/`delete` legitimately embeds its
/// own multi-minute poll-until-ready loop inside one WASM call (e.g. the
/// AWS provider's NAT Gateway delete waits ~7.5 min and Organizations
/// account creation waits up to 10 min — host-side `sleep` + HTTP that
/// the 30s epoch budget deliberately does not count because epochs only
/// tick on WASM compute). A backstop sized near the epoch budget would
/// falsely time out — and then poison (see [`SharedWasmInstance`]) — the
/// entire provider on every such resource.
///
/// 20 min is ~2× the longest known legitimate single-call provider
/// waiter, so it never trips a healthy operation, while still converting
/// the carina#3106 *unbounded* hang into a bounded, recoverable error.
///
/// **Provider contract:** a single provider operation must complete
/// within this budget. A waiter that needs longer must be expressed as
/// the carina `wait` construct (separate short reads the executor drives)
/// rather than a blocking loop inside one `create`/`delete` call.
const WASM_OPERATION_HARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// If `poisoned` is set, return the fail-fast error a poisoned instance
/// must give for `operation`; otherwise `None` and the caller may proceed.
fn poisoned_guard(poisoned: &AtomicBool, operation: &str) -> Option<ProviderError> {
    poisoned.load(Ordering::Acquire).then(|| {
        ProviderError::internal(format!(
            "WASM provider is unusable: a prior operation timed out and left \
             the plugin instance in an undefined state (operation \
             '{operation}'); re-run the command"
        ))
    })
}

/// Run `op` against `instance` under [`WASM_OPERATION_HARD_TIMEOUT`].
///
/// `operation` names the call for the error message (e.g. `"create"`).
///
/// Three outcomes:
/// - `instance` already poisoned by a prior timeout → fail fast without
///   touching the store (a cancelled wasmtime async call leaves the
///   shared `Store` unreusable; see [`SharedWasmInstance::poisoned`]).
/// - `op` completes within budget → its result, untouched.
/// - the deadline elapses → `op` is dropped; the poisoning is done by
///   [`LockedStore`]'s drop while it still holds the store lock (not
///   here), so a waiter cannot acquire the freed-but-not-yet-flagged
///   lock. Returns [`ProviderError::timeout`].
async fn with_operation_timeout<T>(
    instance: &SharedWasmInstance,
    operation: &str,
    op: impl std::future::Future<Output = ProviderResult<T>>,
) -> ProviderResult<T> {
    if let Some(err) = poisoned_guard(&instance.poisoned, operation) {
        return Err(err);
    }
    match tokio::time::timeout(WASM_OPERATION_HARD_TIMEOUT, op).await {
        Ok(result) => result,
        Err(_elapsed) => Err(ProviderError::timeout(format!(
            "WASM plugin operation '{operation}' exceeded {}s (host-side I/O \
             wait that epoch interruption cannot reach; check network/AWS \
             connectivity)",
            WASM_OPERATION_HARD_TIMEOUT.as_secs()
        ))),
    }
}

/// A held store lock that poisons its instance on drop **unless
/// [`disarm`](Self::disarm)ed** after the guest call completed.
///
/// Field order is load-bearing: Rust drops fields in declaration order,
/// so `poison` (the [`PoisonOnDrop`] guard) drops *before* `store` (the
/// `MutexGuard`). When `tokio::time::timeout` cancels the in-flight
/// operation it drops this whole value; the flag is therefore set
/// **while the store `Mutex` is still held**, closing the window where a
/// sibling operation already queued on the lock could otherwise acquire
/// the freed-but-not-yet-flagged store and call into the unusable
/// wasmtime `Store` (carina#3106). A normal completion calls
/// [`disarm`](Self::disarm) so a successful op does not poison.
struct LockedStore<'a> {
    poison: PoisonOnDrop<'a>,
    store: tokio::sync::MutexGuard<'a, Store<HostState>>,
}

/// Sets `poisoned` on drop unless disarmed. Separate from [`LockedStore`]
/// only so the drop-order guarantee is expressed by field ordering.
struct PoisonOnDrop<'a> {
    poisoned: &'a AtomicBool,
    armed: bool,
}

impl Drop for PoisonOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.poisoned.store(true, Ordering::Release);
        }
    }
}

impl<'a> LockedStore<'a> {
    /// Acquire the store lock for `operation`, re-checking the poison
    /// flag *after* the lock is held (an op queued on the `Mutex` when a
    /// sibling timed out passed [`with_operation_timeout`]'s pre-flight
    /// check while the flag was still clear), then arm the epoch
    /// deadline.
    async fn acquire(
        instance: &'a SharedWasmInstance,
        operation: &str,
    ) -> ProviderResult<LockedStore<'a>> {
        let mut store = instance.store.lock().await;
        if let Some(err) = poisoned_guard(&instance.poisoned, operation) {
            return Err(err);
        }
        store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);
        Ok(LockedStore {
            poison: PoisonOnDrop {
                poisoned: &instance.poisoned,
                armed: true,
            },
            store,
        })
    }

    /// The guest call completed (success *or* a clean provider error, not
    /// a cancellation); do not poison the instance.
    fn disarm(&mut self) {
        self.poison.armed = false;
    }

    fn store(&mut self) -> &mut Store<HostState> {
        &mut self.store
    }
}

/// Strip port from authority (e.g., "s3.amazonaws.com:443" -> "s3.amazonaws.com").
fn host_without_port(host: &str) -> &str {
    host.split(':').next().unwrap_or(host)
}

/// Returns `true` if the given host (authority without port) is allowed
/// by the HTTP allow-list.
fn is_host_allowed(host: &str) -> bool {
    let h = host_without_port(host);
    METADATA_HOSTS.contains(&h)
        || HTTP_ALLOWED_HOST_SUFFIXES
            .iter()
            .any(|suffix| h.ends_with(suffix))
}

/// Returns `true` if the host is a metadata service endpoint (EC2 IMDS or ECS).
fn is_metadata_host(host: &str) -> bool {
    METADATA_HOSTS.contains(&host_without_port(host))
}

/// Probe metadata endpoints and return true if any is reachable.
///
/// Uses parallel TCP connect attempts with a 1-second timeout.
/// Called once at startup; result is cached by the caller.
fn probe_metadata_endpoints() -> bool {
    use std::net::{SocketAddr, TcpStream};

    std::thread::scope(|s| {
        let handles: Vec<_> = METADATA_HOSTS
            .iter()
            .map(|host| {
                s.spawn(move || {
                    let addr: SocketAddr = format!("{host}:80").parse().unwrap();
                    TcpStream::connect_timeout(&addr, METADATA_PROBE_TIMEOUT).is_ok()
                })
            })
            .collect();
        handles.into_iter().any(|h| h.join().unwrap_or(false))
    })
}

/// Returns `true` if any metadata endpoint is reachable.
/// Result is cached for the lifetime of the process.
fn is_metadata_available() -> bool {
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(probe_metadata_endpoints)
}

/// Custom `WasiHttpHooks` that restricts outgoing HTTP requests to
/// hosts matching [`HTTP_ALLOWED_HOST_SUFFIXES`] or [`METADATA_HOSTS`].
///
/// Metadata requests are capped at [`METADATA_PROBE_TIMEOUT`] so that non-EC2/ECS
/// environments fail fast rather than waiting for the SDK's default timeout.
struct AllowListHttpHooks;

impl wasmtime_wasi_http::p2::WasiHttpHooks for AllowListHttpHooks {
    fn send_request(
        &mut self,
        request: hyper::Request<wasmtime_wasi_http::p2::body::HyperOutgoingBody>,
        mut config: wasmtime_wasi_http::p2::types::OutgoingRequestConfig,
    ) -> wasmtime_wasi_http::p2::HttpResult<wasmtime_wasi_http::p2::types::HostFutureIncomingResponse>
    {
        let authority = match request.uri().authority() {
            Some(a) => a.as_str(),
            None => "",
        };
        if !is_host_allowed(authority) {
            log::warn!(
                "WASM plugin HTTP request blocked: host {:?} is not in the allow-list",
                authority,
            );
            return Err(
                wasmtime_wasi_http::p2::bindings::http::types::ErrorCode::HttpRequestDenied.into(),
            );
        }
        // Metadata gets 1s; all other requests get the epoch budget.
        let cap = if is_metadata_host(authority) {
            METADATA_PROBE_TIMEOUT
        } else {
            HTTP_API_REQUEST_TIMEOUT
        };
        config.connect_timeout = config.connect_timeout.min(cap);
        config.first_byte_timeout = config.first_byte_timeout.min(cap);
        config.between_bytes_timeout = config.between_bytes_timeout.min(cap);

        if trace_http_enabled() {
            let method = request.method().to_string();
            let uri = request.uri().to_string();
            let spawn_start = std::time::Instant::now();
            let handle = wasmtime_wasi::runtime::spawn(async move {
                let queue_ms = spawn_start.elapsed().as_millis();
                let handler_start = std::time::Instant::now();
                let result = traced_send_request_handler(request, config).await;
                let handler_ms = handler_start.elapsed().as_millis();
                eprintln!(
                    "carina-host-http-trace method={} uri={} queue_ms={} handler_ms={} status={}",
                    method,
                    uri,
                    queue_ms,
                    handler_ms,
                    match &result {
                        Ok(resp) => format!("{}", resp.resp.status().as_u16()),
                        Err(e) => format!("err:{:?}", e),
                    },
                );
                Ok(result)
            });
            return Ok(wasmtime_wasi_http::p2::types::HostFutureIncomingResponse::pending(handle));
        }

        Ok(wasmtime_wasi_http::p2::default_send_request(
            request, config,
        ))
    }
}

/// Companion to carina-plugin-sdk's `CARINA_WASI_HTTP_TRACE` switch.
///
/// When set to "1", the host-side `WasiHttpHooks::send_request` spawns the
/// outgoing request via `traced_send_request_handler` (a phase-instrumented
/// copy of wasmtime-wasi-http's `default_send_request_handler`) and emits
/// the wall-clock breakdown to stderr. Off by default; the gate is a
/// single atomic load per request when disabled.
fn trace_http_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("CARINA_WASI_HTTP_TRACE")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

/// Phase-instrumented copy of `wasmtime_wasi_http::p2::default_send_request_handler`.
///
/// Carries the same TCP/TLS/HTTP path verbatim (so the trace measures what
/// production actually does), but records `Instant::elapsed()` between each
/// phase and emits a single stderr line at the end. Used only when
/// [`trace_http_enabled`] returns true. The non-traced path keeps calling
/// upstream's handler directly.
///
/// Phases (cumulative ms from entry):
/// - `tcp_connect_ms` — DNS + TCP three-way handshake (`TcpStream::connect`)
/// - `tls_handshake_ms` — rustls/tokio-rustls handshake (HTTPS only)
/// - `http_handshake_ms` — hyper http/1.1 protocol handshake
/// - `send_request_ms` — request headers/body sent, response head arrived
///
/// Errors are mapped to the upstream `ErrorCode` variants for parity with
/// the non-traced path.
async fn traced_send_request_handler(
    mut request: hyper::Request<wasmtime_wasi_http::p2::body::HyperOutgoingBody>,
    config: wasmtime_wasi_http::p2::types::OutgoingRequestConfig,
) -> Result<
    wasmtime_wasi_http::p2::types::IncomingResponse,
    wasmtime_wasi_http::p2::bindings::http::types::ErrorCode,
> {
    use http_body_util::BodyExt;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpStream;
    use tokio::time::timeout;
    use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;

    let wasmtime_wasi_http::p2::types::OutgoingRequestConfig {
        use_tls,
        connect_timeout,
        first_byte_timeout,
        between_bytes_timeout,
    } = config;

    let phase_start = std::time::Instant::now();
    let ms = |start: std::time::Instant| start.elapsed().as_millis();

    let method = request.method().to_string();
    let uri = request.uri().to_string();

    let authority = if let Some(authority) = request.uri().authority() {
        if authority.port().is_some() {
            authority.to_string()
        } else {
            let port = if use_tls { 443 } else { 80 };
            format!("{authority}:{port}")
        }
    } else {
        return Err(ErrorCode::HttpRequestUriInvalid);
    };

    let tcp_stream = timeout(connect_timeout, TcpStream::connect(&authority))
        .await
        .map_err(|_| ErrorCode::ConnectionTimeout)?
        .map_err(|_| ErrorCode::ConnectionRefused)?;
    let tcp_connect_ms = ms(phase_start);

    let (mut sender, worker, tls_handshake_ms, http_handshake_ms) = if use_tls {
        use rustls::pki_types::ServerName;

        let root_cert_store = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls_config));
        let host = authority.split(':').next().unwrap_or(&authority);
        let domain = ServerName::try_from(host.to_owned()).map_err(|_| {
            ErrorCode::DnsError(
                wasmtime_wasi_http::p2::bindings::http::types::DnsErrorPayload {
                    rcode: Some("invalid dns name".to_string()),
                    info_code: Some(0),
                },
            )
        })?;
        let stream = connector
            .connect(domain, tcp_stream)
            .await
            .map_err(|_| ErrorCode::TlsProtocolError)?;
        let tls_handshake_ms = ms(phase_start);

        let stream = TokioIo::new(stream);
        let (sender, conn) = timeout(
            connect_timeout,
            hyper::client::conn::http1::handshake(stream),
        )
        .await
        .map_err(|_| ErrorCode::ConnectionTimeout)?
        .map_err(|_| ErrorCode::HttpProtocolError)?;
        let http_handshake_ms = ms(phase_start);

        let worker = wasmtime_wasi::runtime::spawn(async move {
            let _ = conn.await;
        });
        (sender, worker, tls_handshake_ms, http_handshake_ms)
    } else {
        let stream = TokioIo::new(tcp_stream);
        let (sender, conn) = timeout(
            connect_timeout,
            hyper::client::conn::http1::handshake(stream),
        )
        .await
        .map_err(|_| ErrorCode::ConnectionTimeout)?
        .map_err(|_| ErrorCode::HttpProtocolError)?;
        let http_handshake_ms = ms(phase_start);

        let worker = wasmtime_wasi::runtime::spawn(async move {
            let _ = conn.await;
        });
        (sender, worker, tcp_connect_ms, http_handshake_ms)
    };

    // Strip scheme and authority from the request URI: HTTP/1.1 wants only
    // the path on a non-proxy connection.
    *request.uri_mut() = hyper::Uri::builder()
        .path_and_query(
            request
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/"),
        )
        .build()
        .expect("comes from valid request");

    let resp = timeout(first_byte_timeout, sender.send_request(request))
        .await
        .map_err(|_| ErrorCode::ConnectionReadTimeout)?
        .map_err(|_| ErrorCode::HttpProtocolError)?
        .map(|body| {
            body.map_err(|_| ErrorCode::HttpProtocolError)
                .boxed_unsync()
        });
    let send_request_ms = ms(phase_start);

    eprintln!(
        "carina-host-http-trace-phases method={} uri={} \
         tcp_connect_ms={} tls_handshake_ms={} http_handshake_ms={} send_request_ms={}",
        method, uri, tcp_connect_ms, tls_handshake_ms, http_handshake_ms, send_request_ms,
    );

    Ok(wasmtime_wasi_http::p2::types::IncomingResponse {
        resp,
        worker: Some(worker),
        between_bytes_timeout,
    })
}

// -- Host state for WASI --

struct HostState {
    wasi_ctx: WasiCtx,
    http_ctx: Option<WasiHttpCtx>,
    table: ResourceTable,
    http_hooks: AllowListHttpHooks,
    limits: StoreLimits,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: self
                .http_ctx
                .as_mut()
                .expect("HTTP not enabled for this provider"),
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

// -- Helper: create a new Store + CarinaProvider instance --

/// Bindings enum wrapping both non-HTTP and HTTP WASM component bindings.
/// Both worlds export the same `carina:provider/provider` interface.
enum WasmBindings {
    Basic(CarinaProvider),
    Http(CarinaProviderWithHttp),
}

use crate::wasm_bindings::carina::provider::types as wit_types;

impl WasmBindings {
    async fn call_info(&self, store: &mut Store<HostState>) -> wasmtime::Result<String> {
        match self {
            WasmBindings::Basic(b) => b.carina_provider_provider().call_info(store).await,
            WasmBindings::Http(b) => b.carina_provider_provider().call_info(store).await,
        }
    }

    async fn call_schemas(&self, store: &mut Store<HostState>) -> wasmtime::Result<String> {
        match self {
            WasmBindings::Basic(b) => b.carina_provider_provider().call_schemas(store).await,
            WasmBindings::Http(b) => b.carina_provider_provider().call_schemas(store).await,
        }
    }

    async fn call_provider_config_attribute_types(
        &self,
        store: &mut Store<HostState>,
    ) -> wasmtime::Result<String> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_provider_config_attribute_types(store)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_provider_config_attribute_types(store)
                    .await
            }
        }
    }

    async fn call_provider_config_completions(
        &self,
        store: &mut Store<HostState>,
    ) -> wasmtime::Result<String> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_provider_config_completions(store)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_provider_config_completions(store)
                    .await
            }
        }
    }

    async fn call_identity_attributes(
        &self,
        store: &mut Store<HostState>,
    ) -> wasmtime::Result<Vec<String>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_identity_attributes(store)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_identity_attributes(store)
                    .await
            }
        }
    }

    async fn call_get_enum_aliases(
        &self,
        store: &mut Store<HostState>,
    ) -> wasmtime::Result<String> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_get_enum_aliases(store)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_get_enum_aliases(store)
                    .await
            }
        }
    }

    async fn call_validate_config(
        &self,
        store: &mut Store<HostState>,
        attrs: &[(String, wit_types::Value)],
    ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_validate_config(store, attrs)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_validate_config(store, attrs)
                    .await
            }
        }
    }

    async fn call_validate_custom_type(
        &self,
        store: &mut Store<HostState>,
        identity: &wit_types::TypeIdentity,
        value: &str,
    ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_validate_custom_type(store, identity, value)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_validate_custom_type(store, identity, value)
                    .await
            }
        }
    }

    async fn call_initialize(
        &self,
        store: &mut Store<HostState>,
        attrs: &[(String, wit_types::Value)],
    ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_initialize(store, attrs)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_initialize(store, attrs)
                    .await
            }
        }
    }

    async fn call_read(
        &self,
        store: &mut Store<HostState>,
        id: &wit_types::ResourceId,
        identifier: Option<&str>,
        request: wit_types::ReadRequest,
    ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_read(store, id, identifier, request)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_read(store, id, identifier, request)
                    .await
            }
        }
    }

    async fn call_read_data_source(
        &self,
        store: &mut Store<HostState>,
        resource: &wit_types::ResourceDef,
    ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_read_data_source(store, resource)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_read_data_source(store, resource)
                    .await
            }
        }
    }

    async fn call_create(
        &self,
        store: &mut Store<HostState>,
        id: &wit_types::ResourceId,
        request: &wit_types::CreateRequest,
    ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_create(store, id, request)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_create(store, id, request)
                    .await
            }
        }
    }

    async fn call_update(
        &self,
        store: &mut Store<HostState>,
        id: &wit_types::ResourceId,
        identifier: &str,
        request: &wit_types::UpdateRequest,
    ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_update(store, id, identifier, request)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_update(store, id, identifier, request)
                    .await
            }
        }
    }

    async fn call_delete(
        &self,
        store: &mut Store<HostState>,
        id: &wit_types::ResourceId,
        identifier: &str,
        request: wit_types::DeleteRequest,
    ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_delete(store, id, identifier, request)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_delete(store, id, identifier, request)
                    .await
            }
        }
    }

    async fn call_normalize_desired(
        &self,
        store: &mut Store<HostState>,
        resources: &[wit_types::ResourceDef],
    ) -> wasmtime::Result<Vec<wit_types::ResourceDef>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_normalize_desired(store, resources)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_normalize_desired(store, resources)
                    .await
            }
        }
    }

    async fn call_normalize_state(
        &self,
        store: &mut Store<HostState>,
        states: &[(String, wit_types::State)],
    ) -> wasmtime::Result<Vec<(String, wit_types::State)>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_normalize_state(store, states)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_normalize_state(store, states)
                    .await
            }
        }
    }

    async fn call_hydrate_read_state(
        &self,
        store: &mut Store<HostState>,
        states: &[(String, wit_types::State)],
        saved_attrs: &[(String, Vec<(String, wit_types::Value)>)],
    ) -> wasmtime::Result<Vec<(String, wit_types::State)>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_hydrate_read_state(store, states, saved_attrs)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_hydrate_read_state(store, states, saved_attrs)
                    .await
            }
        }
    }

    async fn call_merge_default_tags(
        &self,
        store: &mut Store<HostState>,
        resources: &[wit_types::ResourceDef],
        default_tags: &[(String, wit_types::Value)],
    ) -> wasmtime::Result<Vec<wit_types::ResourceDef>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_merge_default_tags(store, resources, default_tags)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_merge_default_tags(store, resources, default_tags)
                    .await
            }
        }
    }
}

/// Build `StoreLimits` used for every WASM plugin store.
///
/// * 256 MB max linear memory – the AWSCC provider uses ~45 MB for
///   `validate`, so this gives plenty of headroom.
/// * 65 536 table elements. The aws provider's WASM table grew past
///   the previous 20 000 ceiling once `aws-sdk-sqs` was linked in
///   (#2993); pick a wide value so similar additions don't hit the
///   limit again. Cost is metadata-only — wasmtime allocates table
///   slots lazily.
/// * 10 component instances.
fn build_store_limits() -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(256 * 1024 * 1024) // 256 MB
        .table_elements(65_536)
        .instances(10)
        .build()
}

/// Add WASI interfaces to the linker, excluding `wasi:sockets`.
///
/// Instead of `wasmtime_wasi::p2::add_to_linker_async()` which adds ALL
/// interfaces (including TCP/UDP sockets), this function selectively links
/// only the interfaces that WASM provider plugins actually need:
///
/// - `wasi:io`         (poll, streams, error)
/// - `wasi:clocks`     (wall-clock, monotonic-clock)
/// - `wasi:random`     (random, insecure, insecure-seed)
/// - `wasi:cli`        (stderr, environment, exit, terminal, stdin, stdout)
/// - `wasi:filesystem`  (types, preopens)
///
/// This prevents a malicious plugin from opening raw TCP/UDP connections.
fn add_wasi_sans_sockets_to_linker<T: WasiView>(linker: &mut Linker<T>) -> wasmtime::Result<()> {
    use wasmtime_wasi::p2::bindings::{cli, filesystem, random};

    // Start with the proxy interfaces (io + clocks + random::random + basic cli).
    wasmtime_wasi::p2::add_to_linker_proxy_interfaces_async(linker)?;

    // Add remaining random interfaces.
    random::insecure::add_to_linker::<T, WasiRandom>(linker, |t| t.ctx().ctx.random())?;
    random::insecure_seed::add_to_linker::<T, WasiRandom>(linker, |t| t.ctx().ctx.random())?;

    // Add remaining cli interfaces.
    let exit_opts = cli::exit::LinkOptions::default();
    cli::exit::add_to_linker::<T, WasiCli>(linker, &exit_opts, T::cli)?;
    cli::environment::add_to_linker::<T, WasiCli>(linker, T::cli)?;
    cli::terminal_input::add_to_linker::<T, WasiCli>(linker, T::cli)?;
    cli::terminal_output::add_to_linker::<T, WasiCli>(linker, T::cli)?;
    cli::terminal_stdin::add_to_linker::<T, WasiCli>(linker, T::cli)?;
    cli::terminal_stdout::add_to_linker::<T, WasiCli>(linker, T::cli)?;
    cli::terminal_stderr::add_to_linker::<T, WasiCli>(linker, T::cli)?;

    // Add filesystem interfaces.
    filesystem::types::add_to_linker::<T, WasiFilesystem>(linker, T::filesystem)?;
    filesystem::preopens::add_to_linker::<T, WasiFilesystem>(linker, T::filesystem)?;

    Ok(())
}

async fn create_instance(
    engine: &Engine,
    component: &Component,
    provider_kind: Option<&str>,
) -> Result<(Store<HostState>, WasmBindings), String> {
    let wasi_ctx = build_sandboxed_wasi_ctx(provider_kind);
    let host_state = HostState {
        wasi_ctx,
        http_ctx: None,
        table: ResourceTable::new(),
        http_hooks: AllowListHttpHooks,
        limits: build_store_limits(),
    };
    let mut store = Store::new(engine, host_state);
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);

    let mut linker = Linker::new(engine);
    add_wasi_sans_sockets_to_linker(&mut linker)
        .map_err(|e| format!("Failed to add WASI to linker: {e}"))?;

    let bindings = CarinaProvider::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|e| format!("Failed to instantiate WASM component: {e}"))?;

    Ok((store, WasmBindings::Basic(bindings)))
}

/// Environment variables exposed to **every** WASM guest, regardless of
/// provider kind.
///
/// These are provider-agnostic utilities, NOT credentials. Credentials
/// live in per-provider-kind partitions below so that one provider's
/// secret never reaches another provider's guest.
const SHARED_ENV_ALLOWLIST: &[&str] = &[
    "HOME",
    "RUST_LOG",
    // When set to "1", the WASM-side wasi:http bridge in carina-plugin-sdk
    // emits a per-phase wall-clock breakdown of each request to stderr.
    // Off by default; intended for diagnosing transport-level latency.
    "CARINA_WASI_HTTP_TRACE",
];

/// Environment variables exposed only to the AWS providers (`aws`,
/// `awscc`). These are exactly the AWS SDK's auto-discovered credential
/// and region inputs; the SDK's own chain (`aws_config::defaults().load()`)
/// reads them — the host merely makes them visible inside the sandbox.
const AWS_ENV_ALLOWLIST: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_ENDPOINT_URL",
    "AWS_EC2_METADATA_DISABLED",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
];

/// Environment variables exposed only to the GitHub provider.
const GITHUB_ENV_ALLOWLIST: &[&str] = &["GITHUB_TOKEN"];

/// A provider kind whose credential partition the host knows about.
///
/// The WASM guest reports a free-form provider name via `info()`; that
/// raw string is classified once into this closed enum by
/// [`ProviderKind::from_name`]. Keeping the classification in a closed
/// enum makes [`credential_partition`] an *exhaustive* match: adding a
/// new credentialed provider forces a new arm to be handled at compile
/// time, so a future provider cannot silently fall through and receive
/// no credentials by omission (the "new caller tomorrow" guarantee —
/// the type, not a convention, answers what partition a provider gets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    /// AWS native-SDK and Cloud Control providers; both consume the AWS
    /// SDK's auto-discovered credential/region env inputs.
    Aws,
    /// The GitHub provider.
    GitHub,
    /// Any provider the host has no credential partition for (e.g. the
    /// mock provider, or the kind-less info/schemas instance). Receives
    /// the shared group only — never another provider's credentials.
    Other,
}

impl ProviderKind {
    /// Classify a guest-reported provider name. Unrecognized names map to
    /// [`ProviderKind::Other`] (fail-closed: no credentials), so a typo or
    /// casing drift in a provider's `info()` name yields *fewer* secrets,
    /// never another provider's.
    fn from_name(name: Option<&str>) -> Self {
        match name {
            Some("aws") | Some("awscc") => ProviderKind::Aws,
            Some("github") => ProviderKind::GitHub,
            _ => ProviderKind::Other,
        }
    }

    /// The credential env-var partition for this kind. Exhaustive by
    /// construction — a new `ProviderKind` variant will not compile until
    /// its partition is decided here.
    fn credential_partition(self) -> &'static [&'static str] {
        match self {
            ProviderKind::Aws => AWS_ENV_ALLOWLIST,
            ProviderKind::GitHub => GITHUB_ENV_ALLOWLIST,
            ProviderKind::Other => &[],
        }
    }
}

/// Resolve the set of allowlisted env-var names a given provider's guest
/// may receive: the shared group plus that kind's own credential
/// partition.
///
/// `name` is the provider name as reported by `info()` (`"aws"`,
/// `"awscc"`, `"github"`, ...). `None` is the kind-less info/schemas
/// instance, which calls neither `initialize` nor any credentialed
/// operation and therefore gets the shared group only — no credentials.
fn env_keys_for_kind(name: Option<&str>) -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = SHARED_ENV_ALLOWLIST.to_vec();
    keys.extend_from_slice(ProviderKind::from_name(name).credential_partition());
    keys
}

/// Build a WASI context that only exposes the env vars allowlisted for
/// `provider_kind` (its credential partition plus the shared group).
///
/// See [`env_keys_for_kind`] for the partitioning rule.
fn build_sandboxed_wasi_ctx(provider_kind: Option<&str>) -> WasiCtx {
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stderr();
    let keys = env_keys_for_kind(provider_kind);
    for key in &keys {
        if let Ok(val) = std::env::var(key) {
            builder.env(key, &val);
        }
    }
    // Auto-disable IMDS if metadata endpoints are unreachable, unless the
    // user has explicitly set the variable. Only relevant for the AWS
    // partition, which is the only one carrying AWS_EC2_METADATA_DISABLED.
    if keys.contains(&"AWS_EC2_METADATA_DISABLED")
        && std::env::var("AWS_EC2_METADATA_DISABLED").is_err()
        && !is_metadata_available()
    {
        builder.env("AWS_EC2_METADATA_DISABLED", "true");
    }
    builder.build()
}

async fn create_instance_with_http(
    engine: &Engine,
    component: &Component,
    provider_kind: Option<&str>,
) -> Result<(Store<HostState>, WasmBindings), String> {
    let wasi_ctx = build_sandboxed_wasi_ctx(provider_kind);
    let host_state = HostState {
        wasi_ctx,
        http_ctx: Some(WasiHttpCtx::new()),
        table: ResourceTable::new(),
        http_hooks: AllowListHttpHooks,
        limits: build_store_limits(),
    };
    let mut store = Store::new(engine, host_state);
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);

    let mut linker = Linker::new(engine);
    add_wasi_sans_sockets_to_linker(&mut linker)
        .map_err(|e| format!("Failed to add WASI to linker: {e}"))?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)
        .map_err(|e| format!("Failed to add wasi:http to linker: {e}"))?;

    let bindings = CarinaProviderWithHttp::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|e| format!("Failed to instantiate WASM component (HTTP): {e}"))?;

    Ok((store, WasmBindings::Http(bindings)))
}

/// Output of `create_instance_auto`: the instantiated store, the
/// bindings, and whether the HTTP-enabled world was used.
type CreateInstanceResult = Result<(Store<HostState>, WasmBindings, bool), String>;

/// Try HTTP instantiation first, then basic. On double failure, report
/// both errors: the basic fallback's "wasi:http/types not found" is
/// misleading when the real cause is in the HTTP path.
///
/// Returns a boxed future (not `async fn`) to erase the future type at
/// this call site. Inlining this helper as a plain `async fn` composes
/// `CarinaProviderWithHttp::instantiate_async` and
/// `CarinaProvider::instantiate_async` into one anonymous future,
/// which combined with the deep call chain from `carina-cli` trips
/// rustc's layout-computation query depth limit on recent stable
/// toolchains (observed in `cargo check --all-features` CI).
fn create_instance_auto<'a>(
    engine: &'a Engine,
    component: &'a Component,
    provider_kind: Option<&'a str>,
) -> BoxFuture<'a, CreateInstanceResult> {
    Box::pin(async move {
        match create_instance_with_http(engine, component, provider_kind).await {
            Ok((store, bindings)) => Ok((store, bindings, true)),
            Err(http_err) => match create_instance(engine, component, provider_kind).await {
                Ok((store, bindings)) => Ok((store, bindings, false)),
                Err(basic_err) => Err(format_dual_instantiation_error(&http_err, &basic_err)),
            },
        }
    })
}

/// Format the combined error message when both the HTTP and basic
/// instantiation attempts fail. Extracted so regressions in the format
/// (or accidentally dropping one of the two errors) can be unit-tested.
fn format_dual_instantiation_error(http_err: &str, basic_err: &str) -> String {
    format!(
        "Failed to instantiate WASM component; \
         HTTP-enabled world failed: {http_err}; \
         basic fallback also failed: {basic_err}"
    )
}

// -- SharedWasmInstance --

/// A single WASM instance (store + bindings) shared between `WasmProvider`
/// and `WasmProviderNormalizer`. Both hold an `Arc` to this struct and
/// serialize access through the `Mutex<Store<HostState>>`.
struct SharedWasmInstance {
    store: Mutex<Store<HostState>>,
    bindings: WasmBindings,
    /// Set once an operation against this instance times out. A
    /// `tokio::time::timeout` that fires drops the in-flight future while
    /// it is suspended *inside* a `wasmtime` async call; that does not
    /// unwind the WASM guest, so the shared `Store` is left holding a
    /// half-executed call frame. wasmtime does not guarantee such a
    /// `Store` is reusable, so once this is set every subsequent
    /// operation fails fast with a clear error instead of touching the
    /// poisoned store and producing silent, non-deterministic corruption
    /// across the remaining resources in the plan/apply (carina#3106).
    poisoned: AtomicBool,
}

// Safety: The Store is behind a Mutex, so concurrent access is serialized.
// The bindings are only used while the store mutex is held.
unsafe impl Send for SharedWasmInstance {}
unsafe impl Sync for SharedWasmInstance {}

// -- WasmProviderFactory --

pub struct WasmProviderFactory {
    engine: Engine,
    component: Component,
    #[allow(dead_code)]
    wasm_path: PathBuf,
    name: String,
    display_name: String,
    version: String,
    schemas: Vec<ResourceSchema>,
    cached_config_completions: HashMap<String, Vec<CompletionValue>>,
    cached_identity_attributes: Vec<String>,
    cached_enum_aliases: HashMap<String, HashMap<String, HashMap<String, String>>>,
    /// Provider config attribute types (e.g., `region` → `AttributeType::StringEnum`).
    /// Used by `ProviderFactory::provider_config_attribute_types()` so the host
    /// validates provider attributes against these types using its own carina-core,
    /// catching format bugs without requiring a provider rebuild.
    cached_provider_config_types: HashMap<String, carina_core::schema::AttributeType>,
    enable_http: bool,
    /// Reusable WASM instance from factory initialization.
    /// Used by `validate_config()` to avoid creating a throwaway instance.
    init_instance: Mutex<(Store<HostState>, WasmBindings)>,
    /// Lazily created shared instances for provider + normalizer, keyed
    /// by binding name. `None` is the kind's default instance;
    /// `Some(name)` is a named instance (`let <name> = provider <kind>
    /// { ... }`). The first call for a given key creates the instance;
    /// subsequent calls reuse it via `Arc`. Keeping a per-binding entry
    /// is what makes carina#2191 routing work end-to-end — collapsing
    /// every binding onto a single shared instance would pin each kind
    /// to the first instance's attributes (e.g. region) regardless of
    /// what later instances configure.
    shared_instances: Mutex<HashMap<Option<String>, Arc<SharedWasmInstance>>>,
    /// Background thread that ticks the epoch counter for timeout enforcement.
    /// Kept alive for the lifetime of the factory; dropped automatically.
    _epoch_ticker: EpochTicker,
}

impl WasmProviderFactory {
    /// Compute the default cache directory (`~/.carina/cache/`).
    /// Returns `None` if the home directory cannot be determined.
    fn default_cache_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".carina").join("cache"))
    }

    /// Load provider metadata from WIT functions. Returns empty defaults on failure.
    async fn load_metadata(
        bindings: &WasmBindings,
        store: &mut Store<HostState>,
    ) -> (
        HashMap<String, Vec<CompletionValue>>,
        Vec<String>,
        HashMap<String, HashMap<String, HashMap<String, String>>>,
        HashMap<String, carina_core::schema::AttributeType>,
    ) {
        let config_completions_json = bindings
            .call_provider_config_completions(store)
            .await
            .unwrap_or_else(|_| "{}".to_string());
        let config_completions: HashMap<String, Vec<CompletionValue>> =
            serde_json::from_str(&config_completions_json).unwrap_or_default();

        let identity_attributes = bindings
            .call_identity_attributes(store)
            .await
            .unwrap_or_default();

        let enum_aliases_json = bindings
            .call_get_enum_aliases(store)
            .await
            .unwrap_or_else(|_| "{}".to_string());
        let enum_aliases: HashMap<String, HashMap<String, HashMap<String, String>>> =
            serde_json::from_str(&enum_aliases_json).unwrap_or_default();

        let provider_config_types_json = bindings
            .call_provider_config_attribute_types(store)
            .await
            .unwrap_or_else(|_| "{}".to_string());
        let provider_config_types =
            wasm_convert::json_to_attribute_types(&provider_config_types_json);

        (
            config_completions,
            identity_attributes,
            enum_aliases,
            provider_config_types,
        )
    }

    /// Compute a cache-safe filename for the given wasm path.
    ///
    /// The filename includes a SHA-256 hash of the canonical wasm path and the
    /// crate version so that different files or crate upgrades never collide.
    /// Compute a cache-safe filename for the given wasm path.
    ///
    /// The filename includes a SHA-256 hash of the file content, canonical path,
    /// and crate version. Changing any of these produces a different cache key,
    /// ensuring the precompiled cache is invalidated on provider upgrades.
    fn cache_key(wasm_path: &Path) -> String {
        let canonical = wasm_path
            .canonicalize()
            .unwrap_or_else(|_| wasm_path.to_path_buf());
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        // Include file content so replacing a .wasm at the same path invalidates cache.
        if let Ok(content) = std::fs::read(wasm_path) {
            hasher.update(&content);
        }
        // Include the crate version so cache is invalidated on host upgrades.
        hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let stem = wasm_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("provider");
        format!("{stem}-{}.cwasm", &hash[..16])
    }

    /// Load a WASM provider, using the default precompile cache at `~/.carina/cache/`.
    ///
    /// If the cache directory cannot be created, falls back to compiling without caching.
    pub async fn new(wasm_path: PathBuf) -> Result<Self, String> {
        match Self::default_cache_dir() {
            Some(cache_dir) => Self::new_with_cache_dir(wasm_path, &cache_dir).await,
            None => Self::new_uncached(wasm_path).await,
        }
    }

    /// Load a WASM provider with an explicit cache directory.
    pub async fn new_with_cache_dir(wasm_path: PathBuf, cache_dir: &Path) -> Result<Self, String> {
        let cwasm_name = Self::cache_key(&wasm_path);
        let cwasm_path = cache_dir.join(&cwasm_name);

        // Try loading from existing cache.
        // On failure, retry once after a short delay — another process may be
        // writing the cache file atomically (write to .tmp then rename).
        if cwasm_path.exists() {
            match Self::from_precompiled(&cwasm_path).await {
                Ok(mut factory) => {
                    factory.wasm_path = wasm_path;
                    return Ok(factory);
                }
                Err(_) => {
                    // Another process may be writing; wait briefly and retry
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if cwasm_path.exists() {
                        match Self::from_precompiled(&cwasm_path).await {
                            Ok(mut factory) => {
                                factory.wasm_path = wasm_path;
                                return Ok(factory);
                            }
                            Err(e) => {
                                eprintln!("Precompile cache invalid, recompiling: {e}");
                                let _ = std::fs::remove_file(&cwasm_path);
                            }
                        }
                    }
                }
            }
        }

        // Try to precompile and cache
        match Self::precompile(&wasm_path, &cwasm_path) {
            Ok(()) => {
                let mut factory = Self::from_precompiled(&cwasm_path).await?;
                factory.wasm_path = wasm_path;
                Ok(factory)
            }
            Err(e) => {
                eprintln!("Failed to write precompile cache, loading directly: {e}");
                Self::new_uncached(wasm_path).await
            }
        }
    }

    /// Load a WASM provider without any precompile caching.
    async fn new_uncached(wasm_path: PathBuf) -> Result<Self, String> {
        let config = build_engine_config();
        let engine =
            Engine::new(&config).map_err(|e| format!("Failed to create WASM engine: {e}"))?;
        let epoch_ticker = EpochTicker::start(engine.clone());

        let component = Component::from_file(&engine, &wasm_path).map_err(|e| {
            format!(
                "Failed to load WASM component from {}: {e}",
                wasm_path.display()
            )
        })?;

        let (mut store, bindings, enable_http) =
            create_instance_auto(&engine, &component, None).await?;
        let info_json = bindings
            .call_info(&mut store)
            .await
            .map_err(|e| format!("Failed to call info(): {e}"))?;
        wasm_convert::check_protocol_version(&info_json)?;

        let schemas_json = bindings
            .call_schemas(&mut store)
            .await
            .map_err(|e| format!("Failed to call schemas(): {e}"))?;

        let (name, display_name, version) = wasm_convert::json_to_provider_info(&info_json);
        let schemas: Vec<ResourceSchema> = wasm_convert::json_to_schemas(&schemas_json);

        let (
            cached_config_completions,
            cached_identity_attributes,
            cached_enum_aliases,
            cached_provider_config_types,
        ) = Self::load_metadata(&bindings, &mut store).await;

        Ok(Self {
            engine,
            component,
            wasm_path,
            name,
            display_name,
            version,
            schemas,
            cached_config_completions,
            cached_identity_attributes,
            cached_enum_aliases,
            cached_provider_config_types,
            enable_http,
            init_instance: Mutex::new((store, bindings)),
            shared_instances: Mutex::new(HashMap::new()),
            _epoch_ticker: epoch_ticker,
        })
    }

    /// Precompile a .wasm file and save the result to a .cwasm file.
    ///
    /// **Mmap safety invariant:** `from_precompiled` loads `.cwasm` via
    /// `Component::deserialize_file`, which keeps the file memory-mapped for
    /// the lifetime of the returned `Component`. Any in-place mutation of
    /// `cwasm_path` — including `std::fs::write(cwasm_path, …)` or
    /// `OpenOptions::truncate(true)` — while a previous `Component` is still
    /// alive pulls backing pages out from under the mmap region; subsequent
    /// access then traps as `SIGBUS` on Linux (macOS is more lenient, so a
    /// bug slipping through on macOS still blows up in CI).
    ///
    /// To honor the invariant, this function writes to a sibling temp file
    /// and then `rename()`s it onto `cwasm_path`. Rename creates a new inode
    /// while keeping the old one alive until its last mapping closes, so any
    /// previously-loaded factories remain valid. Do not "simplify" this to
    /// a direct `std::fs::write(cwasm_path, …)`.
    ///
    /// The tempfile path includes the current process ID so that multiple
    /// processes racing to precompile the same component don't clobber each
    /// other's in-progress writes.
    pub fn precompile(wasm_path: &Path, cwasm_path: &Path) -> Result<(), String> {
        let config = build_engine_config();
        let engine = Engine::new(&config).map_err(|e| format!("Engine error: {e}"))?;
        let wasm_bytes = std::fs::read(wasm_path).map_err(|e| format!("Read error: {e}"))?;
        let serialized = engine
            .precompile_component(&wasm_bytes)
            .map_err(|e| format!("Precompile error: {e}"))?;
        if let Some(parent) = cwasm_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Mkdir error: {e}"))?;
            // Tempfile + atomic rename; preserves any live mmap held by a
            // previously-loaded Component (see doc comment above for why).
            let tmp_path = parent.join(format!(
                ".{}.tmp.{}",
                cwasm_path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or("cwasm"),
                std::process::id()
            ));
            std::fs::write(&tmp_path, &serialized).map_err(|e| format!("Write error: {e}"))?;
            std::fs::rename(&tmp_path, cwasm_path).map_err(|e| format!("Rename error: {e}"))?;
        } else {
            std::fs::write(cwasm_path, &serialized).map_err(|e| format!("Write error: {e}"))?;
        }
        Ok(())
    }

    /// Load from a precompiled .cwasm file.
    ///
    /// # Safety
    /// The .cwasm file must have been produced by `precompile()` using the same
    /// Wasmtime version. Deserializing an untrusted or corrupted file is unsafe.
    pub async fn from_precompiled(cwasm_path: &Path) -> Result<Self, String> {
        let config = build_engine_config();
        let engine =
            Engine::new(&config).map_err(|e| format!("Failed to create WASM engine: {e}"))?;
        let epoch_ticker = EpochTicker::start(engine.clone());

        // SAFETY: The caller is responsible for ensuring the .cwasm file was
        // produced by a trusted `precompile()` call with the same Wasmtime version.
        let component =
            unsafe { Component::deserialize_file(&engine, cwasm_path) }.map_err(|e| {
                format!(
                    "Failed to deserialize WASM component from {}: {e}",
                    cwasm_path.display()
                )
            })?;

        let (mut store, bindings, enable_http) =
            create_instance_auto(&engine, &component, None).await?;
        let info_json = bindings
            .call_info(&mut store)
            .await
            .map_err(|e| format!("Failed to call info(): {e}"))?;
        wasm_convert::check_protocol_version(&info_json)?;

        let schemas_json = bindings
            .call_schemas(&mut store)
            .await
            .map_err(|e| format!("Failed to call schemas(): {e}"))?;

        let (name, display_name, version) = wasm_convert::json_to_provider_info(&info_json);
        let schemas: Vec<ResourceSchema> = wasm_convert::json_to_schemas(&schemas_json);

        let (
            cached_config_completions,
            cached_identity_attributes,
            cached_enum_aliases,
            cached_provider_config_types,
        ) = Self::load_metadata(&bindings, &mut store).await;

        Ok(Self {
            engine,
            component,
            wasm_path: cwasm_path.to_path_buf(),
            name,
            display_name,
            version,
            schemas,
            cached_config_completions,
            cached_identity_attributes,
            cached_enum_aliases,
            cached_provider_config_types,
            enable_http,
            init_instance: Mutex::new((store, bindings)),
            shared_instances: Mutex::new(HashMap::new()),
            _epoch_ticker: epoch_ticker,
        })
    }

    /// Load from .wasm with automatic precompile caching.
    ///
    /// Checks for an existing `.cwasm` in `cache_dir`. If present, attempts to
    /// load it; if the cache is stale or invalid, recompiles and caches anew.
    ///
    /// **Deprecated**: Use `new()` or `new_with_cache_dir()` instead, which
    /// handle caching automatically.
    pub async fn from_file_cached(wasm_path: &Path, cache_dir: &Path) -> Result<Self, String> {
        Self::new_with_cache_dir(wasm_path.to_path_buf(), cache_dir).await
    }

    async fn create_initialized_instance(
        &self,
        attributes: &IndexMap<String, Value>,
    ) -> Result<(Store<HostState>, WasmBindings), String> {
        // This is the credentialed runtime instance (it calls
        // `initialize` and then read/create/update/delete). The provider
        // kind is known here (`self.name`, learned from `info()` at load
        // time), so the guest receives only its own credential partition.
        let kind = Some(self.name.as_str());
        let (mut store, bindings) = if self.enable_http {
            create_instance_with_http(&self.engine, &self.component, kind).await?
        } else {
            create_instance(&self.engine, &self.component, kind).await?
        };
        let wit_attrs =
            wasm_convert::core_to_wit_value_map(attributes).map_err(|e| e.to_string())?;
        bindings
            .call_initialize(&mut store, &wit_attrs)
            .await
            .map_err(|e| format!("Failed to call initialize(): {e}"))?
            .map_err(|e| {
                let core_err = wasm_convert::wit_to_core_provider_error(e);
                format!("Provider initialization failed: {}", core_err.message())
            })?;

        Ok((store, bindings))
    }

    /// Get or create the shared WASM instance for the given binding's
    /// provider + normalizer pair.
    ///
    /// The first call for a `binding` key creates and initializes a new
    /// instance; subsequent calls for the same key return an `Arc` to
    /// the same instance. Different bindings (e.g. `None` for the kind
    /// default and `Some("us")` for a named instance) deliberately get
    /// distinct instances — that is what makes per-instance config
    /// (region, credentials, etc.) survive into runtime calls.
    async fn get_or_create_shared_instance(
        &self,
        binding: Option<&str>,
        attributes: &IndexMap<String, Value>,
    ) -> Result<Arc<SharedWasmInstance>, String> {
        let key: Option<String> = binding.map(|s| s.to_string());
        let mut guard = self.shared_instances.lock().await;
        if let Some(instance) = guard.get(&key) {
            return Ok(Arc::clone(instance));
        }
        let (store, bindings) = self.create_initialized_instance(attributes).await?;
        let instance = Arc::new(SharedWasmInstance {
            store: Mutex::new(store),
            bindings,
            poisoned: AtomicBool::new(false),
        });
        guard.insert(key, Arc::clone(&instance));
        Ok(instance)
    }
}

impl WasmProviderFactory {
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Verify that this provider's version satisfies the given constraint.
    pub fn verify_version(&self, constraint_raw: &str) -> Result<(), String> {
        let req = semver::VersionReq::parse(constraint_raw)
            .map_err(|e| format!("Invalid version constraint '{}': {}", constraint_raw, e))?;
        let actual = semver::Version::parse(&self.version).map_err(|e| {
            format!(
                "Provider '{}' reports invalid version '{}': {}",
                self.name, self.version, e
            )
        })?;
        if !req.matches(&actual) {
            return Err(format!(
                "Provider '{}' version {} does not satisfy constraint '{}'",
                self.name, actual, constraint_raw
            ));
        }
        Ok(())
    }
}

impl ProviderFactory for WasmProviderFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn provider_config_attribute_types(
        &self,
    ) -> HashMap<String, carina_core::schema::AttributeType> {
        self.cached_provider_config_types.clone()
    }

    fn validate_config(&self, attributes: &IndexMap<String, Value>) -> Result<(), String> {
        let wit_attrs =
            wasm_convert::core_to_wit_value_map(attributes).map_err(|e| e.to_string())?;

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut guard = self.init_instance.lock().await;
                let (ref mut store, ref bindings) = *guard;
                store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);
                bindings
                    .call_validate_config(store, &wit_attrs)
                    .await
                    .map_err(|e| format!("Failed to call validate_config(): {e}"))?
                    .map_err(|e| {
                        wasm_convert::wit_to_core_provider_error(e)
                            .message()
                            .to_string()
                    })
            })
        })
    }

    fn validate_custom_type(&self, identity: &TypeIdentity, value: &str) -> Result<(), String> {
        let wit_identity = wasm_convert::core_type_identity_to_wit(identity);
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut guard = self.init_instance.lock().await;
                let (ref mut store, ref bindings) = *guard;
                store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);
                bindings
                    .call_validate_custom_type(store, &wit_identity, value)
                    .await
                    .map_err(|e| format!("Failed to call validate_custom_type(): {e}"))?
                    .map_err(|e| {
                        wasm_convert::wit_to_core_provider_error(e)
                            .message()
                            .to_string()
                    })
            })
        })
    }

    fn extract_region(&self, attributes: &IndexMap<String, Value>) -> String {
        // Delegate to the shared helper so both quoted-string
        // (`region = "us-east-1"`) and namespaced-identifier
        // (`region = aws.Region.us_east_1`) spellings resolve
        // correctly. carina#3021.
        carina_core::utils::extract_region_from_attrs(attributes, "ap-northeast-1")
    }

    fn config_completions(&self) -> HashMap<String, Vec<CompletionValue>> {
        self.cached_config_completions.clone()
    }

    fn identity_attributes(&self) -> Vec<&str> {
        self.cached_identity_attributes
            .iter()
            .map(|s| s.as_str())
            .collect()
    }

    fn get_enum_alias_reverse(
        &self,
        resource_type: &str,
        attr_name: &str,
        value: &str,
    ) -> Option<String> {
        self.cached_enum_aliases
            .get(resource_type)
            .and_then(|attrs| attrs.get(attr_name))
            .and_then(|aliases| aliases.get(value))
            .cloned()
    }

    fn create_provider(
        &self,
        binding: Option<&str>,
        attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        let attrs = attributes.clone();
        let binding = binding.map(|s| s.to_string());
        Box::pin(async move {
            // Surface the inner error string verbatim — it carries the
            // user-actionable message (e.g. allowed_account_ids
            // mismatch) produced by the provider's init step. Adding a
            // wrapper prefix here would leak the WASM hosting detail
            // into the user-facing error (see #2407).
            let instance = self
                .get_or_create_shared_instance(binding.as_deref(), &attrs)
                .await
                .map_err(ProviderError::invalid_input)?;
            Ok(Box::new(WasmProvider {
                instance,
                name: self.name.clone(),
            }) as Box<dyn Provider>)
        })
    }

    fn create_normalizer(
        &self,
        binding: Option<&str>,
        attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
        let attrs = attributes.clone();
        let binding = binding.map(|s| s.to_string());
        Box::pin(async move {
            match self
                .get_or_create_shared_instance(binding.as_deref(), &attrs)
                .await
            {
                Ok(instance) => {
                    Box::new(WasmProviderNormalizer { instance }) as Box<dyn ProviderNormalizer>
                }
                Err(e) => {
                    log::error!("Failed to create WASM normalizer instance: {e}");
                    Box::new(carina_core::provider::NoopNormalizer) as Box<dyn ProviderNormalizer>
                }
            }
        })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        self.schemas.clone()
    }
}

// -- WasmProvider --

pub struct WasmProvider {
    instance: Arc<SharedWasmInstance>,
    name: String,
}

// Safety: SharedWasmInstance.store is behind a Mutex, so concurrent access is
// serialized. The bindings are only used while the store mutex is held.
unsafe impl Send for WasmProvider {}
unsafe impl Sync for WasmProvider {}

impl Provider for WasmProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let wit_request = wasm_convert::core_to_wit_read_request(&request);
        let identifier = identifier.map(|s| s.to_string());
        let id = id.clone();
        Box::pin(with_operation_timeout(&self.instance, "read", async move {
            let mut locked = LockedStore::acquire(&self.instance, "read").await?;
            let call = self
                .instance
                .bindings
                .call_read(locked.store(), &wit_id, identifier.as_deref(), wit_request)
                .await;
            // The guest call returned (success or trap, not a
            // cancellation): the store is in a defined state, so do not
            // poison it.
            locked.disarm();
            let result = call.map_err(|e| {
                let msg = format!("{e}");
                if is_epoch_trap_message(&msg) {
                    ProviderError::timeout(format!(
                        "WASM plugin timed out after {WASM_OPERATION_TIMEOUT_SECS}s in read \
                         (check AWS credentials)"
                    ))
                } else {
                    ProviderError::internal(format!("WASM trap in read: {e}"))
                }
            })?;
            match result {
                Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
            }
        }))
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_resource = match wasm_convert::core_data_source_to_wit_resource(resource) {
            Ok(v) => v,
            Err(e) => return early_provider_err(e),
        };
        let id = resource.id.clone();
        Box::pin(with_operation_timeout(
            &self.instance,
            "read_data_source",
            async move {
                let mut locked = LockedStore::acquire(&self.instance, "read_data_source").await?;
                let call = self
                    .instance
                    .bindings
                    .call_read_data_source(locked.store(), &wit_resource)
                    .await;
                locked.disarm();
                let result = call.map_err(|e| {
                    let msg = format!("{e}");
                    if is_epoch_trap_message(&msg) {
                        ProviderError::timeout(format!(
                            "WASM plugin timed out after {WASM_OPERATION_TIMEOUT_SECS}s in \
                             read_data_source (check AWS credentials)"
                        ))
                    } else {
                        ProviderError::internal(format!("WASM trap in read_data_source: {e}"))
                    }
                })?;
                match result {
                    Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                    Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
                }
            },
        ))
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let wit_request = match wasm_convert::core_to_wit_create_request(&request) {
            Ok(v) => v,
            Err(e) => return early_provider_err(e),
        };
        let id = id.clone();
        Box::pin(with_operation_timeout(
            &self.instance,
            "create",
            async move {
                let mut locked = LockedStore::acquire(&self.instance, "create").await?;
                let call = self
                    .instance
                    .bindings
                    .call_create(locked.store(), &wit_id, &wit_request)
                    .await;
                locked.disarm();
                let result = call.map_err(|e| {
                    let msg = format!("{e}");
                    if is_epoch_trap_message(&msg) {
                        ProviderError::timeout(format!(
                            "WASM plugin timed out after {WASM_OPERATION_TIMEOUT_SECS}s in create \
                             (check AWS credentials)"
                        ))
                    } else {
                        ProviderError::internal(format!("WASM trap in create: {e}"))
                    }
                })?;
                match result {
                    Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                    Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
                }
            },
        ))
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let identifier = identifier.to_string();
        let wit_request = match wasm_convert::core_to_wit_update_request(&request) {
            Ok(v) => v,
            Err(e) => return early_provider_err(e),
        };
        let id = id.clone();
        Box::pin(with_operation_timeout(
            &self.instance,
            "update",
            async move {
                let mut locked = LockedStore::acquire(&self.instance, "update").await?;
                let call = self
                    .instance
                    .bindings
                    .call_update(locked.store(), &wit_id, &identifier, &wit_request)
                    .await;
                locked.disarm();
                let result = call.map_err(|e| {
                    let msg = format!("{e}");
                    if is_epoch_trap_message(&msg) {
                        ProviderError::timeout(format!(
                            "WASM plugin timed out after {WASM_OPERATION_TIMEOUT_SECS}s in update \
                             (check AWS credentials)"
                        ))
                    } else {
                        ProviderError::internal(format!("WASM trap in update: {e}"))
                    }
                })?;
                match result {
                    Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                    Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
                }
            },
        ))
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let identifier = identifier.to_string();
        let wit_request = wasm_convert::core_to_wit_delete_request(&request);
        Box::pin(with_operation_timeout(
            &self.instance,
            "delete",
            async move {
                let mut locked = LockedStore::acquire(&self.instance, "delete").await?;
                let call = self
                    .instance
                    .bindings
                    .call_delete(locked.store(), &wit_id, &identifier, wit_request)
                    .await;
                locked.disarm();
                let result = call.map_err(|e| {
                    let msg = format!("{e}");
                    if is_epoch_trap_message(&msg) {
                        ProviderError::timeout(format!(
                            "WASM plugin timed out after {WASM_OPERATION_TIMEOUT_SECS}s in delete \
                             (check AWS credentials)"
                        ))
                    } else {
                        ProviderError::internal(format!("WASM trap in delete: {e}"))
                    }
                })?;
                match result {
                    Ok(()) => Ok(()),
                    Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
                }
            },
        ))
    }
}

// -- WasmProviderNormalizer --

pub struct WasmProviderNormalizer {
    instance: Arc<SharedWasmInstance>,
}

// Safety: Same rationale as WasmProvider.
unsafe impl Send for WasmProviderNormalizer {}
unsafe impl Sync for WasmProviderNormalizer {}

impl ProviderNormalizer for WasmProviderNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [Resource],
    ) -> carina_core::provider::BoxFuture<'a, ()> {
        Box::pin(async move {
            let wit_resources: Vec<_> = expect_unresolvable_absent(
                resources
                    .iter()
                    .map(wasm_convert::core_to_wit_resource)
                    .collect::<Result<Vec<_>, _>>(),
                "normalize_desired",
            );

            // Plain `.await` on the store lock, not a nested `block_on`:
            // the guard is acquired and dropped within this one polled
            // future, so the apply-path `renormalize` calling this once
            // per resource cannot self-deadlock (carina#3112).
            let result = {
                let mut store = self.instance.store.lock().await;
                store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);
                self.instance
                    .bindings
                    .call_normalize_desired(&mut store, &wit_resources)
                    .await
            };

            match result {
                Ok(result) => {
                    // `PlanPreprocessor::prepare` strips every attribute that
                    // recursively contains `Value::Deferred(DeferredValue::ResourceRef)` (alongside
                    // `Value::Deferred(DeferredValue::Unknown)`) before this normalizer runs and
                    // restores them afterwards (#2387), so we can blindly
                    // accept everything the WASM normalizer returns — the
                    // pre-#2387 `contains_resource_ref` overwrite-skip
                    // workaround is no longer reachable.
                    for (core_res, wit_res) in resources.iter_mut().zip(result.iter()) {
                        let resolved = wasm_convert::wit_to_core_value_map(&wit_res.attributes);
                        for (key, value) in resolved {
                            core_res.attributes.insert(key, value);
                        }
                    }
                }
                Err(e) => log::error!("WASM trap in normalize_desired: {e}"),
            }
        })
    }

    fn normalize_state<'a>(
        &'a self,
        current_states: &'a mut HashMap<ResourceId, State>,
    ) -> carina_core::provider::BoxFuture<'a, ()> {
        Box::pin(async move {
            let wit_states: Vec<(String, _)> = current_states
                .iter()
                .map(|(id, state)| {
                    let wit = expect_unresolvable_absent(
                        wasm_convert::core_to_wit_state(state),
                        "normalize_state",
                    );
                    (id.to_string(), wit)
                })
                .collect();

            // Plain `.await`, not a nested `block_on` — see `normalize_desired`.
            let result = {
                let mut store = self.instance.store.lock().await;
                store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);
                self.instance
                    .bindings
                    .call_normalize_state(&mut store, &wit_states)
                    .await
            };

            match result {
                Ok(result) => {
                    for state in current_states.values_mut() {
                        let key = state.id.to_string();
                        if let Some((_, wit_state)) = result.iter().find(|(k, _)| k == &key) {
                            state.attributes =
                                wasm_convert::wit_to_core_value_map(&wit_state.attributes);
                        }
                    }
                }
                Err(e) => log::error!("WASM trap in normalize_state: {e}"),
            }
        })
    }

    fn hydrate_read_state<'a>(
        &'a self,
        current_states: &'a mut HashMap<ResourceId, State>,
        saved_attrs: &'a SavedAttrs,
    ) -> carina_core::provider::BoxFuture<'a, ()> {
        Box::pin(async move {
            let wit_states: Vec<(String, _)> = current_states
                .iter()
                .map(|(id, state)| {
                    let wit = expect_unresolvable_absent(
                        wasm_convert::core_to_wit_state(state),
                        "hydrate_read_state (current_states)",
                    );
                    (id.to_string(), wit)
                })
                .collect();

            let wit_saved: Vec<(String, Vec<(String, _)>)> = saved_attrs
                .iter()
                .map(|(id, attrs)| {
                    let wit = expect_unresolvable_absent(
                        wasm_convert::core_to_wit_value_map(attrs),
                        "hydrate_read_state (saved_attrs)",
                    );
                    (id.to_string(), wit)
                })
                .collect();

            // Plain `.await`, not a nested `block_on` — see `normalize_desired`.
            let result = {
                let mut store = self.instance.store.lock().await;
                store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);
                self.instance
                    .bindings
                    .call_hydrate_read_state(&mut store, &wit_states, &wit_saved)
                    .await
            };

            match result {
                Ok(result) => {
                    for state in current_states.values_mut() {
                        let key = state.id.to_string();
                        if let Some((_, wit_state)) = result.iter().find(|(k, _)| k == &key) {
                            state.attributes =
                                wasm_convert::wit_to_core_value_map(&wit_state.attributes);
                        }
                    }
                }
                Err(e) => log::error!("WASM trap in hydrate_read_state: {e}"),
            }
        })
    }

    fn merge_default_tags<'a>(
        &'a self,
        resources: &'a mut [Resource],
        default_tags: &'a IndexMap<String, Value>,
        _registry: &'a carina_core::schema::SchemaRegistry,
    ) -> carina_core::provider::BoxFuture<'a, ()> {
        Box::pin(async move {
            if default_tags.is_empty() {
                return;
            }

            let wit_resources: Vec<_> = expect_unresolvable_absent(
                resources
                    .iter()
                    .map(wasm_convert::core_to_wit_resource)
                    .collect::<Result<Vec<_>, _>>(),
                "merge_default_tags",
            );

            // Per-key skip on serialize failure (vs. `core_to_wit_value_map`'s
            // all-or-nothing) — one bad default tag must not nuke the whole
            // merge for a multi-resource plan.
            let wit_default_tags: Vec<(String, wit_types::Value)> = default_tags
                .iter()
                .filter_map(|(k, v)| match wasm_convert::core_to_wit_value(v) {
                    Ok(wit_value) => Some((k.clone(), wit_value)),
                    Err(e) => {
                        log::error!("Skipping default_tag '{k}' with unresolvable value: {e}");
                        None
                    }
                })
                .collect();

            // Plain `.await`, not a nested `block_on` — see `normalize_desired`.
            let result = {
                let mut store = self.instance.store.lock().await;
                store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);
                self.instance
                    .bindings
                    .call_merge_default_tags(&mut store, &wit_resources, &wit_default_tags)
                    .await
            };

            match result {
                Ok(result) => {
                    // Guest preserves resource order; zip and overwrite
                    // attributes (merge may add `tags` and `_default_tag_keys`).
                    for (core_res, wit_res) in resources.iter_mut().zip(result.iter()) {
                        let resolved = wasm_convert::wit_to_core_value_map(&wit_res.attributes);
                        for (key, value) in resolved {
                            core_res.attributes.insert(key, value);
                        }
                    }
                }
                Err(e) => log::error!("WASM trap in merge_default_tags: {e}"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aws_partition_contains_required_vars() {
        // The AWS providers (aws, awscc) receive the AWS SDK's
        // auto-discovered credential/region inputs.
        let keys = env_keys_for_kind(Some("aws"));
        assert!(keys.contains(&"AWS_ACCESS_KEY_ID"));
        assert!(keys.contains(&"AWS_SECRET_ACCESS_KEY"));
        assert!(keys.contains(&"AWS_SESSION_TOKEN"));
        assert!(keys.contains(&"AWS_REGION"));
        assert!(keys.contains(&"AWS_DEFAULT_REGION"));
        assert!(keys.contains(&"AWS_ENDPOINT_URL"));
        assert!(keys.contains(&"AWS_EC2_METADATA_DISABLED"));
        assert!(keys.contains(&"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"));
        assert!(keys.contains(&"AWS_CONTAINER_CREDENTIALS_FULL_URI"));
        // awscc shares the same AWS partition.
        assert!(env_keys_for_kind(Some("awscc")).contains(&"AWS_ACCESS_KEY_ID"));
    }

    #[test]
    fn test_shared_vars_reach_every_kind() {
        // Provider-agnostic utility vars reach every guest, including
        // the kind-less (info/schemas) instance.
        for kind in [Some("aws"), Some("awscc"), Some("github"), None] {
            let keys = env_keys_for_kind(kind);
            assert!(keys.contains(&"HOME"), "HOME missing for {kind:?}");
            assert!(keys.contains(&"RUST_LOG"), "RUST_LOG missing for {kind:?}");
            assert!(
                keys.contains(&"CARINA_WASI_HTTP_TRACE"),
                "CARINA_WASI_HTTP_TRACE missing for {kind:?}"
            );
        }
    }

    #[test]
    fn test_github_token_is_partitioned_to_github_only() {
        // GITHUB_TOKEN reaches the github guest...
        assert!(env_keys_for_kind(Some("github")).contains(&"GITHUB_TOKEN"));
        // ...and NEVER the aws / awscc guests (the partition is a real
        // isolation boundary, not a flat global list).
        assert!(!env_keys_for_kind(Some("aws")).contains(&"GITHUB_TOKEN"));
        assert!(!env_keys_for_kind(Some("awscc")).contains(&"GITHUB_TOKEN"));
        // ...and not the kind-less info/schemas instance either.
        assert!(!env_keys_for_kind(None).contains(&"GITHUB_TOKEN"));
    }

    #[test]
    fn test_aws_creds_never_reach_github_guest() {
        // Symmetric isolation: AWS credentials must not leak into the
        // github guest.
        let github = env_keys_for_kind(Some("github"));
        assert!(!github.contains(&"AWS_ACCESS_KEY_ID"));
        assert!(!github.contains(&"AWS_SECRET_ACCESS_KEY"));
        assert!(!github.contains(&"AWS_SESSION_TOKEN"));
    }

    #[test]
    fn test_no_partition_leaks_sensitive_unrelated_vars() {
        // Common sensitive/unrelated variables are in NO partition.
        for kind in [Some("aws"), Some("awscc"), Some("github"), None] {
            let keys = env_keys_for_kind(kind);
            for forbidden in ["PATH", "SHELL", "USER", "SSH_AUTH_SOCK", "DATABASE_URL"] {
                assert!(
                    !keys.contains(&forbidden),
                    "{forbidden} leaked into {kind:?} partition"
                );
            }
        }
    }

    #[test]
    fn test_unknown_kind_gets_shared_only() {
        // An unrecognized provider kind receives only the shared group —
        // no credentials of any provider.
        let keys = env_keys_for_kind(Some("totally-unknown-provider"));
        assert!(keys.contains(&"HOME"));
        assert!(!keys.contains(&"AWS_ACCESS_KEY_ID"));
        assert!(!keys.contains(&"GITHUB_TOKEN"));
    }

    #[test]
    fn test_provider_kind_classification() {
        assert_eq!(ProviderKind::from_name(Some("aws")), ProviderKind::Aws);
        assert_eq!(ProviderKind::from_name(Some("awscc")), ProviderKind::Aws);
        assert_eq!(
            ProviderKind::from_name(Some("github")),
            ProviderKind::GitHub
        );
        // Unknown / mock / kind-less all fail closed to Other.
        assert_eq!(ProviderKind::from_name(Some("mock")), ProviderKind::Other);
        assert_eq!(ProviderKind::from_name(None), ProviderKind::Other);
        // Casing/spelling drift fails closed (does NOT match Aws/GitHub).
        assert_eq!(ProviderKind::from_name(Some("AWS")), ProviderKind::Other);
        assert_eq!(ProviderKind::from_name(Some("GitHub")), ProviderKind::Other);
    }

    #[test]
    fn test_other_kind_has_empty_credential_partition() {
        // The fail-closed kind carries no credentials — only the shared
        // group reaches it (asserted via env_keys_for_kind above).
        assert!(ProviderKind::Other.credential_partition().is_empty());
    }

    #[test]
    fn test_imds_var_only_in_aws_partition() {
        // AWS_EC2_METADATA_DISABLED gates the IMDS auto-disable probe in
        // build_sandboxed_wasi_ctx; that probe must run only for the AWS
        // partition. Pin the gate condition: the var is present for aws,
        // absent for github / None.
        assert!(env_keys_for_kind(Some("aws")).contains(&"AWS_EC2_METADATA_DISABLED"));
        assert!(!env_keys_for_kind(Some("github")).contains(&"AWS_EC2_METADATA_DISABLED"));
        assert!(!env_keys_for_kind(None).contains(&"AWS_EC2_METADATA_DISABLED"));
    }

    #[test]
    fn test_build_sandboxed_wasi_ctx_does_not_panic() {
        // Verify that building the sandboxed context succeeds even when
        // allowlisted variables are not set in the environment.
        // This confirms the `if let Ok(val)` guard handles missing vars.
        let _ctx = build_sandboxed_wasi_ctx(Some("aws"));
        let _ctx_none = build_sandboxed_wasi_ctx(None);
    }

    #[test]
    fn test_http_allowlist_permits_amazonaws_com() {
        assert!(is_host_allowed("s3.amazonaws.com"));
        assert!(is_host_allowed("ec2.us-east-1.amazonaws.com"));
        assert!(is_host_allowed("sts.amazonaws.com"));
        assert!(is_host_allowed(
            "cloudformation.ap-northeast-1.amazonaws.com"
        ));
    }

    #[test]
    fn test_http_allowlist_permits_amazonaws_com_cn() {
        assert!(is_host_allowed("s3.amazonaws.com.cn"));
        assert!(is_host_allowed("ec2.cn-north-1.amazonaws.com.cn"));
        assert!(is_host_allowed("sts.cn-northwest-1.amazonaws.com.cn"));
    }

    #[test]
    fn test_http_allowlist_permits_with_port() {
        assert!(is_host_allowed("s3.amazonaws.com:443"));
        assert!(is_host_allowed("ec2.cn-north-1.amazonaws.com.cn:443"));
    }

    #[test]
    fn test_store_limits_are_configured() {
        // Verify that build_store_limits() returns sensible values by
        // exercising the ResourceLimiter trait methods on the result.
        use wasmtime::ResourceLimiter;

        let mut limits = build_store_limits();

        // memory_growing: requesting up to 256 MB should succeed
        assert!(limits.memory_growing(0, 256 * 1024 * 1024, None).unwrap());

        // memory_growing: requesting beyond 256 MB should be denied
        assert!(
            !limits
                .memory_growing(0, 256 * 1024 * 1024 + 1, None)
                .unwrap()
        );

        // table_growing: requesting up to 65_536 elements should succeed
        assert!(limits.table_growing(0, 65_536, None).unwrap());

        // table_growing: requesting beyond 65_536 should be denied
        assert!(!limits.table_growing(0, 65_537, None).unwrap());

        // instances: should be capped at 10
        assert_eq!(limits.instances(), 10);
    }

    #[test]
    fn test_http_allowlist_blocks_other_hosts() {
        assert!(!is_host_allowed("evil.example.com"));
        assert!(!is_host_allowed("attacker.io"));
        assert!(!is_host_allowed("localhost"));
        assert!(!is_host_allowed(""));
        // Ensure partial matches don't pass
        assert!(!is_host_allowed("not-amazonaws.com"));
        assert!(!is_host_allowed("amazonaws.com.evil.com"));
        assert!(!is_host_allowed("fakeamazonaws.com"));
        // Bare domain without service prefix is not a valid AWS endpoint
        assert!(!is_host_allowed("amazonaws.com"));
    }

    #[test]
    fn test_http_allowlist_permits_imds() {
        // EC2 Instance Metadata Service (IMDS) endpoint
        assert!(is_host_allowed("169.254.169.254"));
        // IMDS with explicit port
        assert!(is_host_allowed("169.254.169.254:80"));
    }

    #[test]
    fn test_imds_connect_timeout_is_short() {
        // IMDS timeout should be short enough for non-EC2 environments
        assert!(METADATA_PROBE_TIMEOUT <= std::time::Duration::from_secs(2));
        assert!(METADATA_PROBE_TIMEOUT >= std::time::Duration::from_secs(1));
    }

    #[test]
    fn test_metadata_probe_completes_within_timeout() {
        // Verify that the probe completes within a reasonable time.
        // On EC2/ECS the probe returns true (metadata is available).
        // On local/CI-without-metadata the probe returns false.
        // Either result is valid; we only check that it doesn't hang.
        let start = std::time::Instant::now();
        let _result = probe_metadata_endpoints();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "probe took too long: {elapsed:?}"
        );
    }

    #[test]
    fn test_is_metadata_host() {
        // EC2 IMDS
        assert!(is_metadata_host("169.254.169.254"));
        assert!(is_metadata_host("169.254.169.254:80"));
        // ECS metadata endpoint
        assert!(is_metadata_host("169.254.170.2"));
        assert!(is_metadata_host("169.254.170.2:80"));
        // Non-metadata hosts
        assert!(!is_metadata_host("s3.amazonaws.com"));
        assert!(!is_metadata_host("169.254.169.1"));
    }

    #[test]
    fn test_http_allowlist_permits_ecs_metadata() {
        // ECS Task Metadata endpoint should be allowed
        assert!(is_host_allowed("169.254.170.2"));
        assert!(is_host_allowed("169.254.170.2:80"));
    }

    #[test]
    fn test_ecs_env_vars_in_aws_partition() {
        let keys = env_keys_for_kind(Some("aws"));
        assert!(keys.contains(&"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"));
        assert!(keys.contains(&"AWS_CONTAINER_CREDENTIALS_FULL_URI"));
    }

    #[test]
    fn test_metadata_probe_result_is_cached() {
        let first = is_metadata_available();
        let start = std::time::Instant::now();
        let second = is_metadata_available();
        let elapsed = start.elapsed();
        assert_eq!(first, second);
        assert!(
            elapsed < std::time::Duration::from_millis(10),
            "second call should be cached, took {elapsed:?}"
        );
    }

    #[test]
    fn test_engine_config_enables_epoch_interruption() {
        // Verify that build_engine_config() produces an Engine that supports
        // epoch deadlines. If epoch_interruption were not enabled, setting a
        // deadline on the Store would panic.
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let wasi_ctx = WasiCtxBuilder::new().build();
        let host_state = HostState {
            wasi_ctx,
            http_ctx: None,
            table: ResourceTable::new(),
            http_hooks: AllowListHttpHooks,
            limits: build_store_limits(),
        };
        let mut store = Store::new(&engine, host_state);
        // This will panic if epoch_interruption is not enabled on the engine.
        store.set_epoch_deadline(WASM_OPERATION_TIMEOUT_SECS);
    }

    #[test]
    fn test_epoch_ticker_increments_epoch() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let ticker = EpochTicker::start(engine.clone());

        // Sleep for 2.5 seconds; the ticker should have incremented at least twice.
        std::thread::sleep(std::time::Duration::from_millis(2500));

        // Verify by setting a deadline of 1 on a store — if the epoch has
        // advanced past 1, this deadline is already expired.
        let wasi_ctx = WasiCtxBuilder::new().build();
        let host_state = HostState {
            wasi_ctx,
            http_ctx: None,
            table: ResourceTable::new(),
            http_hooks: AllowListHttpHooks,
            limits: build_store_limits(),
        };
        let mut store = Store::new(&engine, host_state);
        store.set_epoch_deadline(1);

        // The epoch should be at least 2 by now, so a deadline of 1 is expired.
        // We can't easily check the epoch value directly, but we can verify
        // the ticker didn't crash and drop works cleanly.
        drop(ticker);
    }

    #[test]
    fn test_epoch_ticker_stops_on_drop() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let ticker = EpochTicker::start(engine.clone());
        // Drop should signal shutdown and join the thread without hanging.
        drop(ticker);
    }

    #[test]
    fn test_wasm_operation_timeout_is_reasonable() {
        // The timeout should be long enough for normal operations but short
        // enough to prevent indefinite hangs.
        const {
            assert!(
                WASM_OPERATION_TIMEOUT_SECS >= 10,
                "timeout too short for normal operations"
            );
            assert!(
                WASM_OPERATION_TIMEOUT_SECS <= 120,
                "timeout too long to be useful"
            );
        }
    }

    #[test]
    fn test_is_epoch_trap_detection() {
        assert!(is_epoch_trap_message("wasm trap: interrupt"));
        assert!(is_epoch_trap_message("epoch deadline reached"));
        assert!(!is_epoch_trap_message("out of memory"));
        assert!(!is_epoch_trap_message("unreachable code"));
    }

    #[test]
    fn test_http_api_request_timeout_is_longer_than_metadata() {
        // API requests need more time than metadata probes.
        assert!(HTTP_API_REQUEST_TIMEOUT > METADATA_PROBE_TIMEOUT);
    }

    #[test]
    fn test_cache_key_changes_when_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("provider.wasm");

        // Write initial content
        std::fs::write(&wasm_path, b"content_v1").unwrap();
        let key1 = WasmProviderFactory::cache_key(&wasm_path);

        // Change content at same path
        std::fs::write(&wasm_path, b"content_v2").unwrap();
        let key2 = WasmProviderFactory::cache_key(&wasm_path);

        assert_ne!(
            key1, key2,
            "cache key should change when file content changes"
        );
    }

    #[test]
    fn test_cache_key_stable_for_same_content() {
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("provider.wasm");
        std::fs::write(&wasm_path, b"same_content").unwrap();

        let key1 = WasmProviderFactory::cache_key(&wasm_path);
        let key2 = WasmProviderFactory::cache_key(&wasm_path);

        assert_eq!(key1, key2, "cache key should be stable for same content");
    }

    /// Regression guard for carina#1681: when both HTTP and basic
    /// instantiation fail, the combined error must preserve *both* causes.
    /// Dropping either side re-introduces the misdiagnosis pattern that
    /// caused the recurring false "wasi:http/types not found" reports.
    #[test]
    fn format_dual_instantiation_error_preserves_both_causes() {
        let msg = format_dual_instantiation_error(
            "HTTP cause: missing export `foo`",
            "BASIC cause: wasi:http/types not in linker",
        );
        assert!(msg.contains("HTTP cause: missing export `foo`"));
        assert!(msg.contains("BASIC cause: wasi:http/types not in linker"));
        assert!(msg.contains("HTTP-enabled world failed"));
        assert!(msg.contains("basic fallback also failed"));
    }

    /// A fresh (un-poisoned) instance lets an operation proceed: the
    /// guard returns `None`.
    #[test]
    fn poisoned_guard_allows_a_fresh_instance() {
        let flag = AtomicBool::new(false);
        assert!(poisoned_guard(&flag, "create").is_none());
    }

    /// Dropping an *armed* `PoisonOnDrop` (the carina#3106 cancellation
    /// path: `tokio::time::timeout` drops the in-flight operation while it
    /// is suspended inside a `wasmtime` async call, leaving the shared
    /// `Store` unreusable) poisons the instance, and every subsequent
    /// operation then fails fast instead of touching the poisoned store.
    #[test]
    fn armed_drop_poisons_then_guard_fails_fast_naming_the_operation() {
        let flag = AtomicBool::new(false);

        // Operation cancelled before completion: the armed guard is
        // dropped and must poison the instance.
        {
            let _armed = PoisonOnDrop {
                poisoned: &flag,
                armed: true,
            };
        }
        assert!(
            flag.load(Ordering::Acquire),
            "an armed PoisonOnDrop must poison the instance when dropped"
        );

        // A later operation on the same (now poisoned) instance fails fast.
        let guard_err = poisoned_guard(&flag, "delete")
            .expect("a poisoned instance must reject subsequent operations");
        assert!(
            matches!(guard_err, ProviderError::Internal(_)),
            "poisoned fail-fast must be a hard error, got {guard_err:?}"
        );
        let msg = format!("{guard_err:?}");
        assert!(
            msg.contains("delete") && msg.contains("re-run"),
            "fail-fast error should name the rejected op and tell the user to re-run, got: {msg}"
        );
    }

    /// A completed guest call disarms the guard, so a normal operation
    /// (success *or* a clean provider error — both mean the wasmtime call
    /// returned, not a cancellation) must NOT poison the instance.
    #[test]
    fn disarmed_drop_does_not_poison() {
        let flag = AtomicBool::new(false);
        {
            let mut guard = PoisonOnDrop {
                poisoned: &flag,
                armed: true,
            };
            guard.armed = false; // mirrors LockedStore::disarm()
        }
        assert!(
            !flag.load(Ordering::Acquire),
            "a disarmed PoisonOnDrop must not poison the instance"
        );
        assert!(
            poisoned_guard(&flag, "read").is_none(),
            "an un-poisoned instance must keep accepting operations"
        );
    }

    /// The hard timeout must outlast the epoch budget (so genuine
    /// WASM-computation overruns surface as the more specific epoch-trap
    /// message) and, more importantly, must be far longer than the
    /// longest *legitimate* single-call provider waiter — a provider
    /// `create`/`delete` can embed a multi-minute poll-until-ready loop
    /// (the AWS provider's Organizations account waiter is ~10 min). A
    /// backstop near the epoch budget would falsely time out and poison
    /// the provider on every such resource.
    #[test]
    fn hard_timeout_outlasts_epoch_budget_and_longest_legitimate_waiter() {
        assert!(
            WASM_OPERATION_HARD_TIMEOUT.as_secs() > WASM_OPERATION_TIMEOUT_SECS,
            "hard timeout must outlast the epoch budget so epoch traps win the race"
        );
        // Longest known legitimate single-call provider waiter is ~10 min
        // (AWS Organizations account creation). The backstop must clear it
        // with margin so a healthy long operation is never poisoned.
        const LONGEST_LEGITIMATE_WAITER_SECS: u64 = 10 * 60;
        assert!(
            WASM_OPERATION_HARD_TIMEOUT.as_secs() >= 2 * LONGEST_LEGITIMATE_WAITER_SECS,
            "hard timeout must be >= 2x the longest legitimate single-call \
             provider waiter so a healthy long operation is never falsely \
             timed out and poisoned"
        );
    }
}
