//! WasmProviderFactory loads a WASM component and implements ProviderFactory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    BoxFuture, Provider, ProviderError, ProviderFactory, ProviderNormalizer, ProviderResult,
    SavedAttrs,
};
use carina_core::resource::{
    Expr, LifecycleConfig, Resource, ResourceId, State, Value, contains_resource_ref,
};
use carina_core::schema::ResourceSchema;

use crate::wasm_bindings::CarinaProvider;
use crate::wasm_bindings_http::CarinaProviderWithHttp;
use crate::wasm_convert;

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
        // Cap timeouts for metadata endpoints so non-EC2/ECS environments fail fast.
        // On EC2, IMDS responds in <10ms; 1s is generous.
        if is_metadata_host(authority) {
            if config.connect_timeout > METADATA_PROBE_TIMEOUT {
                config.connect_timeout = METADATA_PROBE_TIMEOUT;
            }
            if config.first_byte_timeout > METADATA_PROBE_TIMEOUT {
                config.first_byte_timeout = METADATA_PROBE_TIMEOUT;
            }
            if config.between_bytes_timeout > METADATA_PROBE_TIMEOUT {
                config.between_bytes_timeout = METADATA_PROBE_TIMEOUT;
            }
        }
        Ok(wasmtime_wasi_http::p2::default_send_request(
            request, config,
        ))
    }
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

    async fn call_validate_config(
        &self,
        store: &mut Store<HostState>,
        attrs: &[(String, wit_types::Value)],
    ) -> wasmtime::Result<Result<(), String>> {
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

    async fn call_initialize(
        &self,
        store: &mut Store<HostState>,
        attrs: &[(String, wit_types::Value)],
    ) -> wasmtime::Result<Result<(), String>> {
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
    ) -> wasmtime::Result<Result<wit_types::State, String>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_read(store, id, identifier)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_read(store, id, identifier)
                    .await
            }
        }
    }

    async fn call_create(
        &self,
        store: &mut Store<HostState>,
        resource: &wit_types::ResourceDef,
    ) -> wasmtime::Result<Result<wit_types::State, String>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_create(store, resource)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_create(store, resource)
                    .await
            }
        }
    }

    async fn call_update(
        &self,
        store: &mut Store<HostState>,
        id: &wit_types::ResourceId,
        identifier: &str,
        from: &wit_types::State,
        to: &wit_types::ResourceDef,
    ) -> wasmtime::Result<Result<wit_types::State, String>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_update(store, id, identifier, from, to)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_update(store, id, identifier, from, to)
                    .await
            }
        }
    }

    async fn call_delete(
        &self,
        store: &mut Store<HostState>,
        id: &wit_types::ResourceId,
        identifier: &str,
        options: &str,
    ) -> wasmtime::Result<Result<(), String>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_delete(store, id, identifier, options)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_delete(store, id, identifier, options)
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
}

/// Build `StoreLimits` used for every WASM plugin store.
///
/// * 256 MB max linear memory – the AWSCC provider uses ~45 MB for
///   `validate`, so this gives plenty of headroom.
/// * 20 000 table elements.
/// * 10 component instances.
fn build_store_limits() -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(256 * 1024 * 1024) // 256 MB
        .table_elements(20_000)
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
) -> Result<(Store<HostState>, WasmBindings), String> {
    let wasi_ctx = WasiCtxBuilder::new().inherit_stderr().build();
    let host_state = HostState {
        wasi_ctx,
        http_ctx: None,
        table: ResourceTable::new(),
        http_hooks: AllowListHttpHooks,
        limits: build_store_limits(),
    };
    let mut store = Store::new(engine, host_state);
    store.limiter(|state| &mut state.limits);

    let mut linker = Linker::new(engine);
    add_wasi_sans_sockets_to_linker(&mut linker)
        .map_err(|e| format!("Failed to add WASI to linker: {e}"))?;

    let bindings = CarinaProvider::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|e| format!("Failed to instantiate WASM component: {e}"))?;

    Ok((store, WasmBindings::Basic(bindings)))
}

/// Environment variables allowed to pass through to WASM plugins.
///
/// Only variables needed by the AWS SDK and for debugging are included.
/// All other host environment variables are hidden from plugins.
const WASM_ENV_ALLOWLIST: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_ENDPOINT_URL",
    "AWS_EC2_METADATA_DISABLED",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "HOME",
    "RUST_LOG",
];

/// Build a WASI context that only exposes allowlisted environment variables.
fn build_sandboxed_wasi_ctx() -> WasiCtx {
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stderr();
    for key in WASM_ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            builder.env(key, &val);
        }
    }
    // Auto-disable IMDS if metadata endpoints are unreachable,
    // unless the user has explicitly set the variable.
    if std::env::var("AWS_EC2_METADATA_DISABLED").is_err() && !is_metadata_available() {
        builder.env("AWS_EC2_METADATA_DISABLED", "true");
    }
    builder.build()
}

async fn create_instance_with_http(
    engine: &Engine,
    component: &Component,
) -> Result<(Store<HostState>, WasmBindings), String> {
    let wasi_ctx = build_sandboxed_wasi_ctx();
    let host_state = HostState {
        wasi_ctx,
        http_ctx: Some(WasiHttpCtx::new()),
        table: ResourceTable::new(),
        http_hooks: AllowListHttpHooks,
        limits: build_store_limits(),
    };
    let mut store = Store::new(engine, host_state);
    store.limiter(|state| &mut state.limits);

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

// -- SharedWasmInstance --

/// A single WASM instance (store + bindings) shared between `WasmProvider`
/// and `WasmProviderNormalizer`. Both hold an `Arc` to this struct and
/// serialize access through the `Mutex<Store<HostState>>`.
struct SharedWasmInstance {
    store: Mutex<Store<HostState>>,
    bindings: WasmBindings,
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
    name_static: &'static str,
    display_name_static: &'static str,
    schemas: Vec<ResourceSchema>,
    enable_http: bool,
    /// Reusable WASM instance from factory initialization.
    /// Used by `validate_config()` to avoid creating a throwaway instance.
    init_instance: Mutex<(Store<HostState>, WasmBindings)>,
    /// Lazily created shared instance for provider + normalizer.
    /// The first call to `create_provider` or `create_normalizer` creates
    /// the instance; the second reuses it via `Arc`.
    shared_instance: Mutex<Option<Arc<SharedWasmInstance>>>,
}

impl WasmProviderFactory {
    /// Compute the default cache directory (`~/.carina/cache/`).
    /// Returns `None` if the home directory cannot be determined.
    fn default_cache_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".carina").join("cache"))
    }

    /// Compute a cache-safe filename for the given wasm path.
    ///
    /// The filename includes a SHA-256 hash of the canonical wasm path and the
    /// crate version so that different files or crate upgrades never collide.
    fn cache_key(wasm_path: &Path) -> String {
        let canonical = wasm_path
            .canonicalize()
            .unwrap_or_else(|_| wasm_path.to_path_buf());
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        // Include the crate version so cache is invalidated on upgrades.
        // Component::deserialize also checks wasmtime compatibility, but this
        // avoids unnecessary recompile-on-error cycles.
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
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);

        let engine =
            Engine::new(&config).map_err(|e| format!("Failed to create WASM engine: {e}"))?;

        let component = Component::from_file(&engine, &wasm_path).map_err(|e| {
            format!(
                "Failed to load WASM component from {}: {e}",
                wasm_path.display()
            )
        })?;

        // Detect whether the component needs HTTP by trying HTTP instantiation first,
        // then falling back to basic.
        let (mut store, bindings, enable_http) =
            match create_instance_with_http(&engine, &component).await {
                Ok((store, bindings)) => (store, bindings, true),
                Err(_) => {
                    let (store, bindings) = create_instance(&engine, &component).await?;
                    (store, bindings, false)
                }
            };
        let info_json = bindings
            .call_info(&mut store)
            .await
            .map_err(|e| format!("Failed to call info(): {e}"))?;

        let schemas_json = bindings
            .call_schemas(&mut store)
            .await
            .map_err(|e| format!("Failed to call schemas(): {e}"))?;

        let (name, display_name) = wasm_convert::json_to_provider_info(&info_json);
        let schemas: Vec<ResourceSchema> = wasm_convert::json_to_schemas(&schemas_json);

        let name_static: &'static str = Box::leak(name.into_boxed_str());
        let display_name_static: &'static str = Box::leak(display_name.into_boxed_str());

        Ok(Self {
            engine,
            component,
            wasm_path,
            name_static,
            display_name_static,
            schemas,
            enable_http,
            init_instance: Mutex::new((store, bindings)),
            shared_instance: Mutex::new(None),
        })
    }

    /// Precompile a .wasm file and save the result to a .cwasm file.
    pub fn precompile(wasm_path: &Path, cwasm_path: &Path) -> Result<(), String> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);

        let engine = Engine::new(&config).map_err(|e| format!("Engine error: {e}"))?;
        let wasm_bytes = std::fs::read(wasm_path).map_err(|e| format!("Read error: {e}"))?;
        let serialized = engine
            .precompile_component(&wasm_bytes)
            .map_err(|e| format!("Precompile error: {e}"))?;
        if let Some(parent) = cwasm_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Mkdir error: {e}"))?;
            // Write to a temp file and atomically rename to avoid race conditions
            // when multiple processes precompile the same component concurrently.
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
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);

        let engine =
            Engine::new(&config).map_err(|e| format!("Failed to create WASM engine: {e}"))?;

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
            match create_instance_with_http(&engine, &component).await {
                Ok((store, bindings)) => (store, bindings, true),
                Err(_) => {
                    let (store, bindings) = create_instance(&engine, &component).await?;
                    (store, bindings, false)
                }
            };
        let info_json = bindings
            .call_info(&mut store)
            .await
            .map_err(|e| format!("Failed to call info(): {e}"))?;

        let schemas_json = bindings
            .call_schemas(&mut store)
            .await
            .map_err(|e| format!("Failed to call schemas(): {e}"))?;

        let (name, display_name) = wasm_convert::json_to_provider_info(&info_json);
        let schemas: Vec<ResourceSchema> = wasm_convert::json_to_schemas(&schemas_json);

        let name_static: &'static str = Box::leak(name.into_boxed_str());
        let display_name_static: &'static str = Box::leak(display_name.into_boxed_str());

        Ok(Self {
            engine,
            component,
            wasm_path: cwasm_path.to_path_buf(),
            name_static,
            display_name_static,
            schemas,
            enable_http,
            init_instance: Mutex::new((store, bindings)),
            shared_instance: Mutex::new(None),
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
        attributes: &HashMap<String, Value>,
    ) -> Result<(Store<HostState>, WasmBindings), String> {
        let (mut store, bindings) = if self.enable_http {
            create_instance_with_http(&self.engine, &self.component).await?
        } else {
            create_instance(&self.engine, &self.component).await?
        };
        let wit_attrs = wasm_convert::core_to_wit_value_map(attributes);
        bindings
            .call_initialize(&mut store, &wit_attrs)
            .await
            .map_err(|e| format!("Failed to call initialize(): {e}"))?
            .map_err(|e| format!("Provider initialization failed: {e}"))?;

        Ok((store, bindings))
    }

    /// Get or create the shared WASM instance for provider + normalizer.
    ///
    /// The first call creates and initializes a new instance; subsequent calls
    /// return an `Arc` to the same instance. This avoids creating two separate
    /// WASM instances for the provider and normalizer.
    async fn get_or_create_shared_instance(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> Result<Arc<SharedWasmInstance>, String> {
        let mut guard = self.shared_instance.lock().await;
        if let Some(ref instance) = *guard {
            return Ok(Arc::clone(instance));
        }
        let (store, bindings) = self.create_initialized_instance(attributes).await?;
        let instance = Arc::new(SharedWasmInstance {
            store: Mutex::new(store),
            bindings,
        });
        *guard = Some(Arc::clone(&instance));
        Ok(instance)
    }
}

impl ProviderFactory for WasmProviderFactory {
    fn name(&self) -> &str {
        self.name_static
    }

    fn display_name(&self) -> &str {
        self.display_name_static
    }

    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String> {
        let wit_attrs = wasm_convert::core_to_wit_value_map(attributes);

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut guard = self.init_instance.lock().await;
                let (ref mut store, ref bindings) = *guard;
                bindings
                    .call_validate_config(store, &wit_attrs)
                    .await
                    .map_err(|e| format!("Failed to call validate_config(): {e}"))?
            })
        })
    }

    fn extract_region(&self, attributes: &HashMap<String, Value>) -> String {
        if let Some(Value::String(region)) = attributes.get("region") {
            carina_core::utils::convert_region_value(region)
        } else {
            "ap-northeast-1".to_string()
        }
    }

    fn create_provider(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn Provider>> {
        let attrs = attributes.clone();
        Box::pin(async move {
            let instance = self
                .get_or_create_shared_instance(&attrs)
                .await
                .expect("Failed to create WASM provider instance");
            Box::new(WasmProvider {
                instance,
                name: self.name_static,
            }) as Box<dyn Provider>
        })
    }

    fn create_normalizer(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Option<Box<dyn ProviderNormalizer>>> {
        let attrs = attributes.clone();
        Box::pin(async move {
            match self.get_or_create_shared_instance(&attrs).await {
                Ok(instance) => {
                    Some(Box::new(WasmProviderNormalizer { instance })
                        as Box<dyn ProviderNormalizer>)
                }
                Err(e) => {
                    log::error!("Failed to create WASM normalizer instance: {e}");
                    None
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
    name: &'static str,
}

// Safety: SharedWasmInstance.store is behind a Mutex, so concurrent access is
// serialized. The bindings are only used while the store mutex is held.
unsafe impl Send for WasmProvider {}
unsafe impl Sync for WasmProvider {}

impl Provider for WasmProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let id = id.clone();
        let identifier = identifier.map(|s| s.to_string());
        Box::pin(async move {
            let mut store = self.instance.store.lock().await;
            let result = self
                .instance
                .bindings
                .call_read(&mut store, &wit_id, identifier.as_deref())
                .await
                .map_err(|e| ProviderError::new(format!("WASM trap in read: {e}")))?;
            match result {
                Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                Err(err_json) => Err(wasm_convert::json_to_provider_error(&err_json)),
            }
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_resource = wasm_convert::core_to_wit_resource(resource);
        let id = resource.id.clone();
        Box::pin(async move {
            let mut store = self.instance.store.lock().await;
            let result = self
                .instance
                .bindings
                .call_create(&mut store, &wit_resource)
                .await
                .map_err(|e| ProviderError::new(format!("WASM trap in create: {e}")))?;
            match result {
                Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                Err(err_json) => Err(wasm_convert::json_to_provider_error(&err_json)),
            }
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let identifier = identifier.to_string();
        let wit_from = wasm_convert::core_to_wit_state(from);
        let wit_to = wasm_convert::core_to_wit_resource(to);
        let id = id.clone();
        Box::pin(async move {
            let mut store = self.instance.store.lock().await;
            let result = self
                .instance
                .bindings
                .call_update(&mut store, &wit_id, &identifier, &wit_from, &wit_to)
                .await
                .map_err(|e| ProviderError::new(format!("WASM trap in update: {e}")))?;
            match result {
                Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                Err(err_json) => Err(wasm_convert::json_to_provider_error(&err_json)),
            }
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let identifier = identifier.to_string();
        let options_json = wasm_convert::lifecycle_to_json(lifecycle);
        Box::pin(async move {
            let mut store = self.instance.store.lock().await;
            let result = self
                .instance
                .bindings
                .call_delete(&mut store, &wit_id, &identifier, &options_json)
                .await
                .map_err(|e| ProviderError::new(format!("WASM trap in delete: {e}")))?;
            match result {
                Ok(()) => Ok(()),
                Err(err_json) => Err(wasm_convert::json_to_provider_error(&err_json)),
            }
        })
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
    fn normalize_desired(&self, resources: &mut [Resource]) {
        let wit_resources: Vec<_> = resources
            .iter()
            .map(wasm_convert::core_to_wit_resource)
            .collect();

        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut store = self.instance.store.lock().await;
                self.instance
                    .bindings
                    .call_normalize_desired(&mut store, &wit_resources)
                    .await
            })
        });

        match result {
            Ok(result) => {
                for (core_res, wit_res) in resources.iter_mut().zip(result.iter()) {
                    let resolved = wasm_convert::wit_to_core_value_map(&wit_res.attributes);
                    for (key, value) in resolved {
                        // Skip attributes whose original value contains a ResourceRef.
                        // The WIT roundtrip converts ResourceRef to a debug string
                        // (e.g., "ResourceRef { path: ... }") because the WIT Value type
                        // has no ResourceRef variant. Overwriting would destroy the ref
                        // and prevent resolution at apply time.
                        if let Some(original) = core_res.attributes.get(&key)
                            && contains_resource_ref(original)
                        {
                            continue;
                        }
                        core_res.attributes.insert(key, Expr(value));
                    }
                }
            }
            Err(e) => log::error!("WASM trap in normalize_desired: {e}"),
        }
    }

    fn normalize_state(&self, current_states: &mut HashMap<ResourceId, State>) {
        let wit_states: Vec<(String, _)> = current_states
            .iter()
            .map(|(id, state)| (id.to_string(), wasm_convert::core_to_wit_state(state)))
            .collect();

        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut store = self.instance.store.lock().await;
                self.instance
                    .bindings
                    .call_normalize_state(&mut store, &wit_states)
                    .await
            })
        });

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
    }

    fn hydrate_read_state(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &SavedAttrs,
    ) {
        let wit_states: Vec<(String, _)> = current_states
            .iter()
            .map(|(id, state)| (id.to_string(), wasm_convert::core_to_wit_state(state)))
            .collect();

        let wit_saved: Vec<(String, Vec<(String, _)>)> = saved_attrs
            .iter()
            .map(|(id, attrs)| (id.to_string(), wasm_convert::core_to_wit_value_map(attrs)))
            .collect();

        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut store = self.instance.store.lock().await;
                self.instance
                    .bindings
                    .call_hydrate_read_state(&mut store, &wit_states, &wit_saved)
                    .await
            })
        });

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasm_env_allowlist_contains_required_vars() {
        // Verify the allowlist contains the expected AWS and utility variables
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_ACCESS_KEY_ID"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_SECRET_ACCESS_KEY"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_SESSION_TOKEN"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_REGION"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_DEFAULT_REGION"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_ENDPOINT_URL"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_EC2_METADATA_DISABLED"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_CONTAINER_CREDENTIALS_FULL_URI"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"HOME"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"RUST_LOG"));
    }

    #[test]
    fn test_wasm_env_allowlist_excludes_sensitive_vars() {
        // Verify that common sensitive/unrelated variables are NOT in the allowlist
        assert!(!WASM_ENV_ALLOWLIST.contains(&"PATH"));
        assert!(!WASM_ENV_ALLOWLIST.contains(&"SHELL"));
        assert!(!WASM_ENV_ALLOWLIST.contains(&"USER"));
        assert!(!WASM_ENV_ALLOWLIST.contains(&"SSH_AUTH_SOCK"));
        assert!(!WASM_ENV_ALLOWLIST.contains(&"GITHUB_TOKEN"));
        assert!(!WASM_ENV_ALLOWLIST.contains(&"DATABASE_URL"));
    }

    #[test]
    fn test_build_sandboxed_wasi_ctx_does_not_panic() {
        // Verify that building the sandboxed context succeeds even when
        // allowlisted variables are not set in the environment.
        // This confirms the `if let Ok(val)` guard handles missing vars.
        let _ctx = build_sandboxed_wasi_ctx();
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

        // table_growing: requesting up to 20_000 elements should succeed
        assert!(limits.table_growing(0, 20_000, None).unwrap());

        // table_growing: requesting beyond 20_000 should be denied
        assert!(!limits.table_growing(0, 20_001, None).unwrap());

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
    fn test_ecs_env_vars_in_allowlist() {
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"));
        assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_CONTAINER_CREDENTIALS_FULL_URI"));
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
}
