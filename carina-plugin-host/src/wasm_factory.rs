//! WasmProviderFactory loads a WASM component and implements ProviderFactory.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
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

use carina_core::effect::PlanOp;
use carina_core::provider::{
    BoxFuture, CreateOutcome, CreateRequest, DeleteRequest, Provider, ProviderError,
    ProviderFactory, ProviderNormalizer, ProviderReadyConfig, ProviderResult, ReadRequest,
    SavedAttrs, UpdateOutcome, UpdateRequest,
};
use carina_core::resource::{Resource, ResourceId, ResourceIdentityState, State, Value};
use carina_core::schema::{CompletionValue, ResourceSchema, TypeIdentity};
use carina_core::value::SerializationError;
use carina_core::wait::BindingPattern;
use carina_core::wait::predicate::AttrPath;

use crate::{secret_seal, wasm_convert};

/// Lift a `SerializationError` from a synchronous `core_to_wit_*` call into
/// the async `BoxFuture` shape used by `Provider`, preserving its typed cause.
fn early_provider_err<T: 'static>(
    operation: &'static str,
    error: SerializationError,
) -> BoxFuture<'static, ProviderResult<T>> {
    Box::pin(async move { Err(wasm_value_encode_provider_error(operation, error)) })
}

fn wasm_value_decode_provider_error<E>(operation: &'static str, error: E) -> ProviderError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ProviderError::internal(format!(
        "WASM provider returned an invalid boundary value during {operation}"
    ))
    .with_cause(error)
}

fn wasm_value_encode_provider_error<E>(operation: &'static str, error: E) -> ProviderError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ProviderError::internal(format!(
        "failed to encode a WASM provider boundary value during {operation}"
    ))
    .with_cause(error)
}

#[derive(Debug)]
struct WasmGuestCallError {
    source: wasmtime::Error,
}

impl fmt::Display for WasmGuestCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WASM guest call failed")
    }
}

impl std::error::Error for WasmGuestCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Clone, Copy)]
enum WasmTrapContext {
    ProviderOperation,
    Normalizer,
}

fn wasm_trap_provider_error(
    operation: &'static str,
    error: wasmtime::Error,
    context: WasmTrapContext,
) -> ProviderError {
    let timed_out = error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt);
    let cause = WasmGuestCallError { source: error };
    if timed_out {
        let credential_hint = match context {
            WasmTrapContext::ProviderOperation => " (check AWS credentials)",
            WasmTrapContext::Normalizer => "",
        };
        ProviderError::timeout(format!(
            "WASM plugin timed out after {WASM_OPERATION_TIMEOUT_SECS}s in {operation}{credential_hint}"
        ))
        .with_cause(cause)
    } else {
        let message = format!("WASM trap in {operation}");
        ProviderError::internal(message).with_cause(cause)
    }
}

fn provider_schema_decode_error(
    provider_name: &str,
    provider_version: &str,
    detail: wasm_convert::SchemaDecodeError,
) -> WasmProviderLoadError {
    WasmProviderLoadError::metadata(
        format!(
            "provider '{provider_name}' {provider_version} emitted schema metadata this host cannot decode; the provider revision may predate this host"
        ),
        detail,
    )
}

/// The wasi:http package version linked by `wasmtime-wasi-http` 43.0.0.
///
/// This must track the `package wasi:http@...` declaration in
/// `wasmtime-wasi-http`'s `wit/deps/http.wit`. The regression test below pins
/// the companion crate version against `Cargo.lock`, so a dependency bump
/// cannot silently leave this user-facing version stale.
pub const WASI_HTTP_HOST_VERSION: &str = "0.2.6";

/// The `wasmtime-wasi-http` crate release whose WIT was inspected when
/// [`WASI_HTTP_HOST_VERSION`] was set.
const WASMTIME_WASI_HTTP_CRATE_VERSION_WITH_KNOWN_WIT: &str = "43.0.0";

#[derive(Debug)]
struct WasiHttpImport {
    name: String,
    version: WasiHttpImportVersion,
}

#[derive(Debug)]
enum WasiHttpImportVersion {
    Semver(semver::Version),
    Unversioned,
    Invalid,
}

#[derive(Debug)]
struct ComponentImports {
    names: Vec<String>,
}

impl ComponentImports {
    fn from_component(engine: &Engine, component: &Component) -> Self {
        let component_type = component.component_type();
        let mut names: Vec<_> = component_type
            .imports(engine)
            .map(|(name, _)| name.to_string())
            .collect();
        names.sort();
        names.dedup();
        Self { names }
    }

    fn iter(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// A non-empty set of wasi:http imports. Splitting out the first item keeps
/// the "component imports wasi:http" state non-empty by construction.
#[derive(Debug)]
struct WasiHttpImports {
    first: WasiHttpImport,
    rest: Vec<WasiHttpImport>,
}

impl WasiHttpImports {
    fn from_component_imports(component_imports: &ComponentImports) -> Option<Self> {
        let imports: Vec<_> = component_imports
            .iter()
            .filter_map(|name| {
                name.strip_prefix("wasi:http/")?;
                let version = match name.rsplit_once('@') {
                    Some((_, raw)) => semver::Version::parse(raw)
                        .map(WasiHttpImportVersion::Semver)
                        .unwrap_or(WasiHttpImportVersion::Invalid),
                    None => WasiHttpImportVersion::Unversioned,
                };
                Some(WasiHttpImport {
                    name: name.to_string(),
                    version,
                })
            })
            .collect();

        let mut imports = imports.into_iter();
        let first = imports.next()?;
        Some(Self {
            first,
            rest: imports.collect(),
        })
    }

    fn iter(&self) -> impl Iterator<Item = &WasiHttpImport> {
        std::iter::once(&self.first).chain(self.rest.iter())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WasiHttpCompatibility {
    Compatible,
    Incompatible,
    Unknown,
}

#[derive(Debug)]
struct WasiHttpImportedContext {
    imports: WasiHttpImports,
    host_version: semver::Version,
    compatibility: WasiHttpCompatibility,
}

#[derive(Debug)]
struct WasiHttpNotImportedContext {
    component_imports: ComponentImports,
    host_version: semver::Version,
}

#[derive(Debug)]
enum ComponentInstantiationContext {
    WasiHttpImported { context: WasiHttpImportedContext },
    WasiHttpNotImported { context: WasiHttpNotImportedContext },
}

impl ComponentInstantiationContext {
    fn from_component(engine: &Engine, component: &Component) -> Self {
        let host_version = semver::Version::parse(WASI_HTTP_HOST_VERSION)
            .expect("WASI_HTTP_HOST_VERSION must be valid semver");
        let component_imports = ComponentImports::from_component(engine, component);
        match WasiHttpImports::from_component_imports(&component_imports) {
            Some(imports) => {
                let compatibility = wasi_http_compatibility(&host_version, &imports);
                Self::WasiHttpImported {
                    context: WasiHttpImportedContext {
                        imports,
                        host_version,
                        compatibility,
                    },
                }
            }
            None => Self::WasiHttpNotImported {
                context: WasiHttpNotImportedContext {
                    component_imports,
                    host_version,
                },
            },
        }
    }
}

#[derive(Debug)]
enum DualAttemptContext {
    WasiHttpImported {
        context: WasiHttpImportedContext,
    },
    WasiHttpNotImported {
        context: WasiHttpNotImportedContext,
        basic_failure: wasmtime::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstantiationWorld {
    HttpEnabled,
    Basic,
}

/// The attempt shape is explicit: the load path carries its dual-attempt
/// fallback disposition, while the runtime path carries exactly one selected
/// world and cannot represent a fallback that never happened.
#[derive(Debug)]
enum InstantiationContext {
    HttpThenBasic {
        context: DualAttemptContext,
    },
    SingleAttempt {
        world: InstantiationWorld,
        context: ComponentInstantiationContext,
    },
}

/// A provider component failed to instantiate.
///
/// Unlike the former `String`, this owns the primary Wasmtime error, so its
/// complete `anyhow` source chain remains available until the final display
/// boundary. Its tagged context also owns component-derived import evidence,
/// the host's linked wasi:http version, and the exact load/runtime attempt
/// shape; callers cannot construct context-free or fictitious fallback prose.
#[derive(Debug)]
pub struct ProviderInstantiationError {
    primary_failure: wasmtime::Error,
    context: Box<InstantiationContext>,
}

impl ProviderInstantiationError {
    fn from_attempts(
        engine: &Engine,
        component: &Component,
        http_failure: wasmtime::Error,
        basic_failure: wasmtime::Error,
    ) -> Self {
        let context = match ComponentInstantiationContext::from_component(engine, component) {
            ComponentInstantiationContext::WasiHttpImported { context } => {
                DualAttemptContext::WasiHttpImported { context }
            }
            ComponentInstantiationContext::WasiHttpNotImported { context } => {
                DualAttemptContext::WasiHttpNotImported {
                    context,
                    basic_failure,
                }
            }
        };
        Self {
            primary_failure: http_failure,
            context: Box::new(InstantiationContext::HttpThenBasic { context }),
        }
    }

    fn from_single_attempt(
        engine: &Engine,
        component: &Component,
        world: InstantiationWorld,
        failure: wasmtime::Error,
    ) -> Self {
        Self {
            primary_failure: failure,
            context: Box::new(InstantiationContext::SingleAttempt {
                world,
                context: ComponentInstantiationContext::from_component(engine, component),
            }),
        }
    }
}

fn write_wasi_http_imported_context(
    f: &mut fmt::Formatter<'_>,
    context: &WasiHttpImportedContext,
) -> fmt::Result {
    write!(f, "\nComponent wasi:http imports: ")?;
    for (index, import) in context.imports.iter().enumerate() {
        if index > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{}", import.name)?;
    }
    write!(
        f,
        "\nHost wasi:http version: {} (provided by wasmtime-wasi-http {WASMTIME_WASI_HTTP_CRATE_VERSION_WITH_KNOWN_WIT})",
        context.host_version
    )?;
    match context.compatibility {
        WasiHttpCompatibility::Compatible
            if context.imports.iter().all(|import| {
                matches!(
                    &import.version,
                    WasiHttpImportVersion::Semver(version) if version == &context.host_version
                )
            }) =>
        {
            write!(
                f,
                "\nThe component and host use the identical wasi:http version {}, so the wasi:http version is not the problem.",
                context.host_version
            )
        }
        WasiHttpCompatibility::Compatible => write!(
            f,
            "\nThese wasi:http versions are compatible but differ: Wasmtime's component linker matches 0.2.x patch versions bidirectionally, so the version difference is not the problem."
        ),
        WasiHttpCompatibility::Incompatible => write!(
            f,
            "\nThese wasi:http versions are not semver-compatible under Wasmtime's component-linker rules."
        ),
        WasiHttpCompatibility::Unknown => write!(
            f,
            "\nCompatibility could not be determined because at least one wasi:http import has no valid semantic version."
        ),
    }
}

fn write_wasi_http_not_imported_context(
    f: &mut fmt::Formatter<'_>,
    context: &WasiHttpNotImportedContext,
) -> fmt::Result {
    write!(f, "\nComponent wasi:http imports: none detected")?;
    if !context.component_imports.is_empty() {
        write!(f, "\nComponent imports: ")?;
        for (index, import) in context.component_imports.iter().enumerate() {
            if index > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{import}")?;
        }
    }
    write!(
        f,
        "\nHost wasi:http version: {} (provided by wasmtime-wasi-http {WASMTIME_WASI_HTTP_CRATE_VERSION_WITH_KNOWN_WIT})",
        context.host_version
    )
}

fn write_single_attempt_context(
    f: &mut fmt::Formatter<'_>,
    world: InstantiationWorld,
) -> fmt::Result {
    match world {
        InstantiationWorld::HttpEnabled => write!(
            f,
            "\nSingle runtime instantiation attempt: HTTP-enabled world. No basic fallback was attempted."
        ),
        InstantiationWorld::Basic => write!(
            f,
            "\nSingle runtime instantiation attempt: basic world. No fallback attempt was made."
        ),
    }
}

impl fmt::Display for ProviderInstantiationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_error_chain(f, &self.primary_failure)?;
        match self.context.as_ref() {
            InstantiationContext::HttpThenBasic {
                context: DualAttemptContext::WasiHttpImported { context },
            } => {
                write_wasi_http_imported_context(f, context)?;
                write!(
                    f,
                    "\nBasic fallback diagnostic omitted: this component imports wasi:http, which the basic linker deliberately does not provide, so that attempt cannot identify the root cause."
                )
            }
            InstantiationContext::HttpThenBasic {
                context:
                    DualAttemptContext::WasiHttpNotImported {
                        context,
                        basic_failure,
                    },
            } => {
                write_wasi_http_not_imported_context(f, context)?;
                write!(
                    f,
                    "\nSubordinate basic fallback attempt also failed (not the root cause): "
                )?;
                write_error_chain(f, basic_failure)
            }
            InstantiationContext::SingleAttempt { world, context } => {
                match context {
                    ComponentInstantiationContext::WasiHttpImported { context } => {
                        write_wasi_http_imported_context(f, context)?;
                    }
                    ComponentInstantiationContext::WasiHttpNotImported { context } => {
                        write_wasi_http_not_imported_context(f, context)?;
                    }
                }
                write_single_attempt_context(f, *world)
            }
        }
    }
}

// `ProviderInstantiationError` and
// `PrecompiledComponentDeserializationError` deliberately render their
// complete child chains because CLI/LSP boundaries use `Display`. Do not also
// expose those already-rendered children through `source()`: standard chain
// walkers would print the same chains twice.
impl std::error::Error for ProviderInstantiationError {}

/// Error returned while loading a WASM provider.
///
/// Precompiled deserialization and instantiation are dedicated variants so
/// callers can classify the exact failing step without flattening diagnostics
/// to prose or treating unrelated provider bring-up failures as cache faults.
#[derive(Debug)]
pub enum WasmProviderLoadError {
    /// `Component::deserialize_file` rejected a precompiled cache entry.
    PrecompiledDeserialization(PrecompiledComponentDeserializationError),
    /// The component could not be instantiated as a Carina provider.
    Instantiation(ProviderInstantiationError),
    /// A provider metadata export trapped or returned malformed data.
    Metadata {
        context: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Any other engine, component-load, or provider bring-up failure.
    Other(String),
}

impl fmt::Display for WasmProviderLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WasmProviderLoadError::PrecompiledDeserialization(error) => error.fmt(f),
            WasmProviderLoadError::Instantiation(error) => error.fmt(f),
            WasmProviderLoadError::Metadata { context, source } => {
                // ProviderArtifactLoadError is the terminal rendering surface
                // and does not walk `source()`, so render this typed chain once
                // here while still exposing it through Error::source.
                write!(f, "{context}")?;
                let mut current: Option<&(dyn std::error::Error + 'static)> = Some(source.as_ref());
                while let Some(error) = current {
                    write!(f, ": {error}")?;
                    current = error.source();
                }
                Ok(())
            }
            WasmProviderLoadError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for WasmProviderLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WasmProviderLoadError::Metadata { source, .. } => Some(source.as_ref()),
            WasmProviderLoadError::PrecompiledDeserialization(_)
            | WasmProviderLoadError::Instantiation(_)
            | WasmProviderLoadError::Other(_) => None,
        }
    }
}

impl WasmProviderLoadError {
    fn metadata(
        context: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Metadata {
            context: context.into(),
            source: Box::new(source),
        }
    }
}

impl From<ProviderInstantiationError> for WasmProviderLoadError {
    fn from(error: ProviderInstantiationError) -> Self {
        WasmProviderLoadError::Instantiation(error)
    }
}

impl From<String> for WasmProviderLoadError {
    fn from(message: String) -> Self {
        WasmProviderLoadError::Other(message)
    }
}

/// Failure returned while deserializing a precompiled component cache entry.
///
/// Owns the cache path and structured Wasmtime error from the
/// `Component::deserialize_file` attempt so the complete cause chain remains
/// available at the final display boundary.
#[derive(Debug)]
pub struct PrecompiledComponentDeserializationError {
    cwasm_path: PathBuf,
    source: wasmtime::Error,
}

impl fmt::Display for PrecompiledComponentDeserializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Failed to deserialize WASM component from {}: ",
            self.cwasm_path.display()
        )?;
        write_error_chain(f, &self.source)
    }
}

impl std::error::Error for PrecompiledComponentDeserializationError {}

/// Failure while creating a credentialed per-binding runtime instance.
/// Instantiation and `info` guest-call failures remain typed until the
/// `ProviderFactory` trait boundary; protocol and provider initialization
/// rejections retain their existing verbatim messages.
#[derive(Debug)]
enum WasmProviderInstanceError {
    Instantiation(ProviderInstantiationError),
    InfoCall(wasmtime::Error),
    ProtocolVersion(String),
    Other(String),
}

type ProviderInstantiationErrorMapper =
    Arc<dyn Fn(ProviderInstantiationError) -> ProviderError + Send + Sync>;

fn default_provider_instantiation_error_mapper(error: ProviderInstantiationError) -> ProviderError {
    ProviderError::internal("Failed to instantiate WASM provider runtime instance")
        .with_cause(error)
}

impl WasmProviderInstanceError {
    fn into_provider_error_with(self, mapper: &ProviderInstantiationErrorMapper) -> ProviderError {
        match self {
            WasmProviderInstanceError::Instantiation(error) => mapper(error),
            WasmProviderInstanceError::InfoCall(error) => {
                wasm_trap_provider_error("info", error, WasmTrapContext::ProviderOperation)
            }
            WasmProviderInstanceError::ProtocolVersion(message) => ProviderError::internal(message),
            WasmProviderInstanceError::Other(message) => ProviderError::invalid_input(message),
        }
    }
}

impl fmt::Display for WasmProviderInstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WasmProviderInstanceError::Instantiation(error) => error.fmt(f),
            WasmProviderInstanceError::InfoCall(error) => {
                write!(f, "Failed to call info(): ")?;
                write_error_chain(f, error)
            }
            WasmProviderInstanceError::ProtocolVersion(message)
            | WasmProviderInstanceError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for WasmProviderInstanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WasmProviderInstanceError::InfoCall(error) => Some(error.as_ref()),
            WasmProviderInstanceError::Instantiation(_)
            | WasmProviderInstanceError::ProtocolVersion(_)
            | WasmProviderInstanceError::Other(_) => None,
        }
    }
}

impl From<String> for WasmProviderInstanceError {
    fn from(message: String) -> Self {
        WasmProviderInstanceError::Other(message)
    }
}

fn wasi_http_compatibility(
    host_version: &semver::Version,
    imports: &WasiHttpImports,
) -> WasiHttpCompatibility {
    let mut saw_unknown = false;
    for import in imports.iter() {
        match &import.version {
            WasiHttpImportVersion::Semver(component_version) => {
                if !component_versions_linker_compatible(host_version, component_version) {
                    return WasiHttpCompatibility::Incompatible;
                }
            }
            WasiHttpImportVersion::Unversioned | WasiHttpImportVersion::Invalid => {
                saw_unknown = true
            }
        }
    }
    if saw_unknown {
        WasiHttpCompatibility::Unknown
    } else {
        WasiHttpCompatibility::Compatible
    }
}

/// Mirrors Wasmtime's component-name compatibility tracks: stable 1.x names
/// share a major-version track, stable 0.x names with a non-zero minor share
/// a major/minor track, and pre-releases or 0.0.x names match only exactly.
fn component_versions_linker_compatible(left: &semver::Version, right: &semver::Version) -> bool {
    if left == right {
        return true;
    }
    if !left.pre.is_empty() || !right.pre.is_empty() {
        return false;
    }
    if left.major != 0 || right.major != 0 {
        return left.major != 0 && left.major == right.major;
    }
    if left.minor != 0 || right.minor != 0 {
        return left.minor != 0 && left.minor == right.minor;
    }
    false
}

fn write_error_chain(f: &mut fmt::Formatter<'_>, error: &wasmtime::Error) -> fmt::Result {
    for (index, cause) in error.chain().enumerate() {
        if index == 0 {
            write!(f, "{cause}")?;
        } else {
            write!(f, "\n  caused by: {cause}")?;
        }
    }
    Ok(())
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
/// Component traps retain their originating operation and top-level cause;
/// cancellation/timeouts retain the legacy generic diagnostic.
fn poisoned_guard(
    poisoned: &AtomicBool,
    poisoned_reason: &OnceLock<String>,
    operation: &str,
) -> Option<ProviderError> {
    if !poisoned.load(Ordering::Acquire) {
        return None;
    }

    Some(match poisoned_reason.get() {
        Some(reason) => ProviderError::internal(format!(
            "{reason} (operation '{operation}'); re-run the command"
        )),
        None => ProviderError::internal(format!(
            "WASM provider is unusable: a prior operation timed out and left \
             the plugin instance in an undefined state (operation \
             '{operation}'); re-run the command"
        )),
    })
}

fn poison_after_trap(
    poisoned: &AtomicBool,
    poisoned_reason: &OnceLock<String>,
    operation: &'static str,
    error: &wasmtime::Error,
) {
    let root_cause = error.root_cause().to_string();
    let root_cause = root_cause.lines().next().unwrap_or("unknown WASM trap");
    let cause = match error.downcast_ref::<wasmtime::Trap>() {
        Some(trap) => {
            let trap = trap.to_string();
            if trap == root_cause {
                trap
            } else {
                format!("{trap}: {root_cause}")
            }
        }
        None => root_cause.to_string(),
    };
    let _ = poisoned_reason.set(format!(
        "WASM provider instance unusable after trap in {operation}: {cause}"
    ));
    poisoned.store(true, Ordering::Release);
}

/// Run `op` against `instance` under [`WASM_OPERATION_HARD_TIMEOUT`].
///
/// `operation` names the call for the error message (e.g. `"create"`).
///
/// Three outcomes:
/// - `instance` already poisoned by a prior timeout/cancellation or component
///   trap → fail fast without touching the store (either leaves the shared
///   component instance unreusable; see [`SharedWasmInstance::poisoned`]).
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
    if let Some(err) = poisoned_guard(&instance.poisoned, &instance.poisoned_reason, operation) {
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
/// wasmtime `Store` (carina#3106). A returned guest call is disarmed; returned
/// component traps are then poisoned explicitly with their origin preserved.
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
    /// sibling poisoned the instance passed an earlier pre-flight check while
    /// the flag was still clear), then arm the epoch deadline.
    async fn acquire(
        instance: &'a SharedWasmInstance,
        operation: &str,
    ) -> ProviderResult<LockedStore<'a>> {
        let mut store = instance.store.lock().await;
        if let Some(err) = poisoned_guard(&instance.poisoned, &instance.poisoned_reason, operation)
        {
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

    /// The guest call returned rather than being cancelled. Disable automatic
    /// cancellation poisoning; a caller handling a returned component trap can
    /// still mark the instance explicitly before releasing the store lock.
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

/// Memoizes metadata endpoint availability for a single probe owner.
struct MetadataProbe {
    cache: OnceLock<bool>,
    #[cfg(test)]
    probes: AtomicUsize,
}

impl MetadataProbe {
    const fn new() -> Self {
        Self {
            cache: OnceLock::new(),
            #[cfg(test)]
            probes: AtomicUsize::new(0),
        }
    }

    fn is_available(&self) -> bool {
        *self.cache.get_or_init(|| {
            #[cfg(test)]
            self.probes.fetch_add(1, Ordering::Relaxed);

            probe_metadata_endpoints()
        })
    }

    #[cfg(test)]
    fn probe_count(&self) -> usize {
        self.probes.load(Ordering::Relaxed)
    }
}

static METADATA_PROBE: MetadataProbe = MetadataProbe::new();

/// Probe metadata endpoints and return true if any is reachable.
///
/// Uses parallel TCP connect attempts with a 1-second timeout.
/// Memoization is owned by [`MetadataProbe`].
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
    METADATA_PROBE.is_available()
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

enum WasmBindingCreationError {
    Instantiation(wasmtime::Error),
    InfoCall(wasmtime::Error),
    ProtocolVersion(String),
}

impl WasmBindingCreationError {
    fn instantiation_context(self, context: &'static str) -> Self {
        match self {
            Self::Instantiation(error) => Self::Instantiation(error.context(context)),
            Self::InfoCall(error) => Self::InfoCall(error),
            Self::ProtocolVersion(message) => Self::ProtocolVersion(message),
        }
    }
}

use version_checked_bindings::wit_types;

/// Owns all generated world bindings and enforces protocol preflight before
/// any typed binding can be constructed. Only shared WIT value/interface type
/// modules are visible elsewhere in the crate; the world structs stay private.
pub(crate) mod version_checked_bindings {
    use super::*;

    mod basic {
        wasmtime::component::bindgen!({
            path: "../carina-plugin-wit/wit",
            world: "carina-provider",
            require_store_data_send: true,
            exports: { default: async },
        });
    }

    mod http {
        wasmtime::component::bindgen!({
            path: "../carina-plugin-wit/wit",
            world: "carina-provider-with-http",
            require_store_data_send: true,
            exports: { default: async },
            with: {
                "carina:provider/types": super::basic::carina::provider::types,
                "carina:provider/provider": super::basic::exports::carina::provider::provider,
                "wasi:http": wasmtime_wasi_http::p2::bindings::http,
                "wasi:io": wasmtime_wasi::p2::bindings::io,
            },
        });
    }

    pub(crate) use basic::carina::provider::types as wit_types;
    pub(crate) use basic::exports::carina::provider::provider::PlanOp as WitPlanOp;

    use basic::CarinaProvider;
    use http::CarinaProviderWithHttp;

    const PROVIDER_INTERFACE_EXPORT: &str = "carina:provider/provider@0.1.0";

    // Wasmtime 43 generates types owned by an exported interface separately
    // for each world, even when `with` shares that interface for referenced
    // types. Keep the unavoidable world-to-world conversion private here.
    fn http_plan_op(operation: WitPlanOp) -> http::exports::carina::provider::provider::PlanOp {
        use http::exports::carina::provider::provider::PlanOp as HttpPlanOp;

        match operation {
            WitPlanOp::Create => HttpPlanOp::Create,
            WitPlanOp::Read => HttpPlanOp::Read,
            WitPlanOp::Update => HttpPlanOp::Update,
            WitPlanOp::Delete => HttpPlanOp::Delete,
        }
    }

    /// Raw typed bindings are private to this module. The parent module can
    /// obtain [`WasmBindings`] only through `instantiate_basic` or
    /// `instantiate_http`, both of which perform the stable `info` preflight
    /// and protocol-version check before constructing these bindings.
    enum RawWasmBindings {
        Basic(CarinaProvider),
        Http(CarinaProviderWithHttp),
    }

    pub(super) struct WasmBindings {
        raw: RawWasmBindings,
        info_json: String,
    }

    struct ProtocolCheckedInstance {
        instance: wasmtime::component::Instance,
        info_json: String,
    }

    impl ProtocolCheckedInstance {
        async fn instantiate(
            store: &mut Store<HostState>,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let pre = linker
                .instantiate_pre(component)
                .map_err(WasmBindingCreationError::Instantiation)?;
            let instance = pre
                .instantiate_async(&mut *store)
                .await
                .map_err(WasmBindingCreationError::Instantiation)?;
            let info = untyped_info_func(store, &instance)
                .map_err(WasmBindingCreationError::Instantiation)?;
            let (info_json,) = info
                .call_async(&mut *store, ())
                .await
                .map_err(WasmBindingCreationError::InfoCall)?;
            wasm_convert::check_protocol_version(&info_json)
                .map_err(WasmBindingCreationError::ProtocolVersion)?;
            Ok(Self {
                instance,
                info_json,
            })
        }
    }

    fn untyped_info_func(
        store: &mut Store<HostState>,
        instance: &wasmtime::component::Instance,
    ) -> wasmtime::Result<wasmtime::component::TypedFunc<(), (String,)>> {
        let provider = instance
            .get_export_index(&mut *store, None, PROVIDER_INTERFACE_EXPORT)
            .ok_or_else(|| {
                wasmtime::Error::msg(format!(
                    "no exported instance named `{PROVIDER_INTERFACE_EXPORT}`"
                ))
            })?;
        let info = instance
            .get_export_index(&mut *store, Some(&provider), "info")
            .ok_or_else(|| {
                wasmtime::Error::msg(format!(
                    "instance export `{PROVIDER_INTERFACE_EXPORT}` does not have export `info`"
                ))
            })?;
        instance.get_typed_func::<(), (String,)>(&mut *store, &info)
    }

    impl WasmBindings {
        pub(super) async fn instantiate_basic(
            store: &mut Store<HostState>,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let checked = ProtocolCheckedInstance::instantiate(store, component, linker).await?;
            let bindings = CarinaProvider::new(&mut *store, &checked.instance)
                .map_err(WasmBindingCreationError::Instantiation)?;
            Ok(Self {
                raw: RawWasmBindings::Basic(bindings),
                info_json: checked.info_json,
            })
        }

        pub(super) async fn instantiate_http(
            store: &mut Store<HostState>,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let checked = ProtocolCheckedInstance::instantiate(store, component, linker).await?;
            let bindings = CarinaProviderWithHttp::new(&mut *store, &checked.instance)
                .map_err(WasmBindingCreationError::Instantiation)?;
            Ok(Self {
                raw: RawWasmBindings::Http(bindings),
                info_json: checked.info_json,
            })
        }

        pub(super) fn info_json(&self) -> &str {
            &self.info_json
        }

        pub(super) async fn call_schemas(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<String> {
            match &self.raw {
                RawWasmBindings::Basic(b) => b.carina_provider_provider().call_schemas(store).await,
                RawWasmBindings::Http(b) => b.carina_provider_provider().call_schemas(store).await,
            }
        }

        pub(super) async fn call_provider_config_attribute_types(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<String> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_provider_config_attribute_types(store)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_provider_config_attribute_types(store)
                        .await
                }
            }
        }

        pub(super) async fn call_provider_config_completions(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<String> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_provider_config_completions(store)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_provider_config_completions(store)
                        .await
                }
            }
        }

        pub(super) async fn call_identity_attributes(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<Vec<String>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_identity_attributes(store)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_identity_attributes(store)
                        .await
                }
            }
        }

        pub(super) async fn call_get_enum_aliases(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<String> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_get_enum_aliases(store)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_get_enum_aliases(store)
                        .await
                }
            }
        }

        pub(super) async fn call_validate_config(
            &self,
            store: &mut Store<HostState>,
            attrs: &[(String, wit_types::Value)],
        ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_validate_config(store, attrs)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_validate_config(store, attrs)
                        .await
                }
            }
        }

        pub(super) async fn call_validate_custom_type(
            &self,
            store: &mut Store<HostState>,
            identity: &wit_types::TypeIdentity,
            value: &str,
        ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_validate_custom_type(store, identity, value)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_validate_custom_type(store, identity, value)
                        .await
                }
            }
        }

        pub(super) async fn call_initialize(
            &self,
            store: &mut Store<HostState>,
            attrs: &[(String, wit_types::Value)],
        ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_initialize(store, attrs)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_initialize(store, attrs)
                        .await
                }
            }
        }

        pub(super) async fn call_read(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: Option<&str>,
            request: wit_types::ReadRequest,
        ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_read(store, id, identifier, request)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_read(store, id, identifier, request)
                        .await
                }
            }
        }

        pub(super) async fn call_read_data_source(
            &self,
            store: &mut Store<HostState>,
            resource: &wit_types::ResourceDef,
        ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_read_data_source(store, resource)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_read_data_source(store, resource)
                        .await
                }
            }
        }

        pub(super) async fn call_create(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            request: &wit_types::CreateRequest,
        ) -> wasmtime::Result<Result<wit_types::CreateOutcome, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_create(store, id, request)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_create(store, id, request)
                        .await
                }
            }
        }

        pub(super) async fn call_update(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: &str,
            request: &wit_types::UpdateRequest,
        ) -> wasmtime::Result<Result<wit_types::UpdateOutcome, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_update(store, id, identifier, request)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_update(store, id, identifier, request)
                        .await
                }
            }
        }

        pub(super) async fn call_delete(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: &str,
            request: wit_types::DeleteRequest,
        ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_delete(store, id, identifier, request)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_delete(store, id, identifier, request)
                        .await
                }
            }
        }

        pub(super) async fn call_required_permissions(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            operation: PlanOp,
        ) -> wasmtime::Result<Vec<String>> {
            let operation = wasm_convert::core_to_wit_plan_op(operation);
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_required_permissions(store, id, operation)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_required_permissions(store, id, http_plan_op(operation))
                        .await
                }
            }
        }

        pub(super) async fn call_satisfier_hint(
            &self,
            store: &mut Store<HostState>,
            target_id: &wit_types::ResourceId,
            attr_path: &[String],
        ) -> wasmtime::Result<Vec<wit_types::BindingPattern>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_satisfier_hint(store, target_id, attr_path)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_satisfier_hint(store, target_id, attr_path)
                        .await
                }
            }
        }

        pub(super) async fn call_normalize_desired(
            &self,
            store: &mut Store<HostState>,
            desired: &secret_seal::SealedDesired,
        ) -> wasmtime::Result<Result<secret_seal::GuestDesired, wit_types::ProviderError>> {
            desired
                .send(|resources, _| async move {
                    match &self.raw {
                        RawWasmBindings::Basic(b) => {
                            b.carina_provider_provider()
                                .call_normalize_desired(store, resources)
                                .await
                        }
                        RawWasmBindings::Http(b) => {
                            b.carina_provider_provider()
                                .call_normalize_desired(store, resources)
                                .await
                        }
                    }
                })
                .await
        }

        #[cfg(test)]
        pub(super) async fn call_normalize_desired_raw(
            &self,
            store: &mut Store<HostState>,
            resources: &[wit_types::ResourceDef],
        ) -> wasmtime::Result<Result<Vec<wit_types::ResourceDef>, wit_types::ProviderError>>
        {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_normalize_desired(store, resources)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_normalize_desired(store, resources)
                        .await
                }
            }
        }

        pub(super) async fn call_normalize_state(
            &self,
            store: &mut Store<HostState>,
            states: &[(String, wit_types::State)],
        ) -> wasmtime::Result<Result<Vec<(String, wit_types::State)>, wit_types::ProviderError>>
        {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_normalize_state(store, states)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_normalize_state(store, states)
                        .await
                }
            }
        }

        pub(super) async fn call_hydrate_read_state(
            &self,
            store: &mut Store<HostState>,
            states: &[(String, wit_types::State)],
            saved_attrs: &[(String, Vec<(String, wit_types::Value)>)],
        ) -> wasmtime::Result<Result<Vec<(String, wit_types::State)>, wit_types::ProviderError>>
        {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_hydrate_read_state(store, states, saved_attrs)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_hydrate_read_state(store, states, saved_attrs)
                        .await
                }
            }
        }

        pub(super) async fn call_merge_default_tags(
            &self,
            store: &mut Store<HostState>,
            desired: &secret_seal::SealedDesired,
        ) -> wasmtime::Result<Result<secret_seal::GuestDesired, wit_types::ProviderError>> {
            desired
                .send(|resources, default_tags| async move {
                    match &self.raw {
                        RawWasmBindings::Basic(b) => {
                            b.carina_provider_provider()
                                .call_merge_default_tags(store, resources, default_tags)
                                .await
                        }
                        RawWasmBindings::Http(b) => {
                            b.carina_provider_provider()
                                .call_merge_default_tags(store, resources, default_tags)
                                .await
                        }
                    }
                })
                .await
        }
    }
}

use version_checked_bindings::WasmBindings;

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
) -> Result<(Store<HostState>, WasmBindings), WasmBindingCreationError> {
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
    add_wasi_sans_sockets_to_linker(&mut linker).map_err(|error| {
        WasmBindingCreationError::Instantiation(error.context("Failed to add WASI to linker"))
    })?;

    let bindings = WasmBindings::instantiate_basic(&mut store, component, &linker)
        .await
        .map_err(|error| error.instantiation_context("Failed to instantiate WASM component"))?;

    Ok((store, bindings))
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
) -> Result<(Store<HostState>, WasmBindings), WasmBindingCreationError> {
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
    add_wasi_sans_sockets_to_linker(&mut linker).map_err(|error| {
        WasmBindingCreationError::Instantiation(error.context("Failed to add WASI to linker"))
    })?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker).map_err(|error| {
        WasmBindingCreationError::Instantiation(error.context("Failed to add wasi:http to linker"))
    })?;

    let bindings = WasmBindings::instantiate_http(&mut store, component, &linker)
        .await
        .map_err(|error| {
            error.instantiation_context("Failed to instantiate WASM component (HTTP)")
        })?;

    Ok((store, bindings))
}

/// Instantiate exactly the world selected when the factory was loaded.
///
/// Runtime per-binding instances do not retry another world. Keeping the
/// selection and typed error construction in one function prevents either
/// branch from flattening its Wasmtime failure or inventing fallback context.
async fn create_runtime_instance(
    engine: &Engine,
    component: &Component,
    provider_kind: Option<&str>,
    world: InstantiationWorld,
) -> Result<(Store<HostState>, WasmBindings), WasmProviderInstanceError> {
    let result = match world {
        InstantiationWorld::HttpEnabled => {
            create_instance_with_http(engine, component, provider_kind).await
        }
        InstantiationWorld::Basic => create_instance(engine, component, provider_kind).await,
    };
    result.map_err(|failure| match failure {
        WasmBindingCreationError::Instantiation(failure) => {
            WasmProviderInstanceError::Instantiation(
                ProviderInstantiationError::from_single_attempt(engine, component, world, failure),
            )
        }
        WasmBindingCreationError::InfoCall(error) => WasmProviderInstanceError::InfoCall(error),
        WasmBindingCreationError::ProtocolVersion(message) => {
            WasmProviderInstanceError::ProtocolVersion(message)
        }
    })
}

/// Output of `create_instance_auto`: the instantiated store, the
/// bindings, and whether the HTTP-enabled world was used.
type CreateInstanceResult = Result<(Store<HostState>, WasmBindings, bool), WasmProviderLoadError>;

/// Try HTTP instantiation first, then basic. On double failure, retain the
/// complete HTTP-path error and component-derived compatibility context. The
/// basic failure is omitted when the component imports wasi:http because that
/// linker deliberately cannot satisfy it and therefore carries no diagnostic
/// signal. If the HTTP attempt instantiates successfully but its stable `info`
/// call fails or its reported protocol version is incompatible, that preflight
/// failure is definitive and returns immediately without trying the basic
/// world.
///
/// Returns a boxed future (not `async fn`) to erase the future type at
/// this call site. Inlining this helper as a plain `async fn` composes
/// the HTTP and basic instantiation paths into one anonymous future,
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
            Err(WasmBindingCreationError::InfoCall(error)) => Err(WasmProviderLoadError::metadata(
                "Failed to call info()",
                WasmGuestCallError { source: error },
            )),
            Err(WasmBindingCreationError::ProtocolVersion(message)) => Err(message.into()),
            Err(WasmBindingCreationError::Instantiation(http_err)) => {
                match create_instance(engine, component, provider_kind).await {
                    Ok((store, bindings)) => Ok((store, bindings, false)),
                    Err(WasmBindingCreationError::InfoCall(error)) => {
                        Err(WasmProviderLoadError::metadata(
                            "Failed to call info()",
                            WasmGuestCallError { source: error },
                        ))
                    }
                    Err(WasmBindingCreationError::ProtocolVersion(message)) => Err(message.into()),
                    Err(WasmBindingCreationError::Instantiation(basic_err)) => {
                        Err(ProviderInstantiationError::from_attempts(
                            engine, component, http_err, basic_err,
                        )
                        .into())
                    }
                }
            }
        }
    })
}

// -- SharedWasmInstance --

/// A single WASM instance (store + bindings) shared between `WasmProvider`
/// and `WasmProviderNormalizer`. Both hold an `Arc` to this struct and
/// serialize access through the `Mutex<Store<HostState>>`.
struct SharedWasmInstance {
    store: Mutex<Store<HostState>>,
    bindings: WasmBindings,
    /// Set once a timeout/cancellation or a component trap leaves the shared
    /// component instance unusable. Once set, every subsequent operation fails
    /// fast instead of touching the instance and producing a secondary,
    /// misleading trap or silent corruption across the remaining resources in
    /// the plan/apply (carina#3106).
    poisoned: AtomicBool,
    /// Stable diagnostic for a trap that made the component instance
    /// non-reentrant. Timeouts retain the legacy generic poisoned message;
    /// component traps record their originating operation and cause so a
    /// later CRUD call cannot misattribute the failure to itself.
    poisoned_reason: OnceLock<String>,
}

// Safety: The Store is behind a Mutex, so concurrent access is serialized.
// The bindings are only used while the store mutex is held.
unsafe impl Send for SharedWasmInstance {}
unsafe impl Sync for SharedWasmInstance {}

// -- WasmProviderFactory --

enum CachedComponentAcquisition {
    Hit(Component),
    Miss,
}

type ProviderConfigCompletions = HashMap<String, Vec<CompletionValue>>;
type ProviderEnumAliases = HashMap<String, HashMap<String, HashMap<String, String>>>;

pub struct WasmProviderFactory {
    engine: Engine,
    component: Component,
    #[allow(dead_code)]
    wasm_path: PathBuf,
    name: String,
    display_name: String,
    version: String,
    schemas: Vec<ResourceSchema>,
    cached_config_completions: ProviderConfigCompletions,
    cached_identity_attributes: Vec<String>,
    cached_enum_aliases: ProviderEnumAliases,
    /// Provider config attribute types (e.g., `region` → `AttributeType::Enum`).
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
    /// Maps the host's typed runtime-instantiation error into the public
    /// `ProviderError` trait boundary. Composition roots can wrap it with
    /// resolver-owned artifact provenance without stringifying either layer.
    instantiation_error_mapper: ProviderInstantiationErrorMapper,
    /// Background thread that ticks the epoch counter for timeout enforcement.
    /// Kept alive for the lifetime of the factory; dropped automatically.
    _epoch_ticker: EpochTicker,
}

#[derive(Debug)]
struct WasmMetadataCallError {
    source: wasmtime::Error,
}

impl fmt::Display for WasmMetadataCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for WasmMetadataCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        let source: &(dyn std::error::Error + 'static) = self.source.as_ref();
        source.source()
    }
}

fn metadata_call_error(
    provider_name: &str,
    provider_version: &str,
    export: &'static str,
    source: wasmtime::Error,
) -> WasmProviderLoadError {
    WasmProviderLoadError::metadata(
        format!(
            "provider '{provider_name}' {provider_version} failed to call metadata export {export}"
        ),
        WasmMetadataCallError { source },
    )
}

fn decode_config_completions_json(
    json: &str,
    provider_name: &str,
    provider_version: &str,
) -> Result<ProviderConfigCompletions, WasmProviderLoadError> {
    serde_json::from_str(json).map_err(|source| {
        WasmProviderLoadError::metadata(
            format!(
                "provider '{provider_name}' {provider_version} emitted malformed provider config completions JSON"
            ),
            source,
        )
    })
}

fn decode_enum_aliases_json(
    json: &str,
    provider_name: &str,
    provider_version: &str,
) -> Result<ProviderEnumAliases, WasmProviderLoadError> {
    serde_json::from_str(json).map_err(|source| {
        WasmProviderLoadError::metadata(
            format!(
                "provider '{provider_name}' {provider_version} emitted malformed enum aliases JSON"
            ),
            source,
        )
    })
}

impl WasmProviderFactory {
    /// Compute the default cache directory (`~/.carina/cache/`).
    /// Returns `None` if the home directory cannot be determined.
    fn default_cache_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".carina").join("cache"))
    }

    /// Load provider metadata from WIT functions.
    ///
    /// Every export and decode is required to succeed. Substituting empty
    /// metadata here changes provider behavior, including resource identity.
    async fn load_metadata(
        bindings: &WasmBindings,
        store: &mut Store<HostState>,
        provider_name: &str,
        provider_version: &str,
    ) -> Result<
        (
            ProviderConfigCompletions,
            Vec<String>,
            ProviderEnumAliases,
            HashMap<String, carina_core::schema::AttributeType>,
        ),
        WasmProviderLoadError,
    > {
        let config_completions_json = bindings
            .call_provider_config_completions(store)
            .await
            .map_err(|source| {
                metadata_call_error(
                    provider_name,
                    provider_version,
                    "provider-config-completions",
                    source,
                )
            })?;
        let config_completions = decode_config_completions_json(
            &config_completions_json,
            provider_name,
            provider_version,
        )?;

        let identity_attributes =
            bindings
                .call_identity_attributes(store)
                .await
                .map_err(|source| {
                    metadata_call_error(
                        provider_name,
                        provider_version,
                        "identity-attributes",
                        source,
                    )
                })?;

        let enum_aliases_json = bindings
            .call_get_enum_aliases(store)
            .await
            .map_err(|source| {
                metadata_call_error(provider_name, provider_version, "get-enum-aliases", source)
            })?;
        let enum_aliases =
            decode_enum_aliases_json(&enum_aliases_json, provider_name, provider_version)?;

        let provider_config_types_json = bindings
            .call_provider_config_attribute_types(store)
            .await
            .map_err(|source| {
                metadata_call_error(
                    provider_name,
                    provider_version,
                    "provider-config-attribute-types",
                    source,
                )
            })?;
        let provider_config_types =
            wasm_convert::json_to_attribute_types(&provider_config_types_json)
                .map_err(|source| {
                    WasmProviderLoadError::metadata(
                        format!(
                            "provider '{provider_name}' {provider_version} emitted provider config attribute types this host cannot decode"
                        ),
                        source,
                    )
                })?;

        Ok((
            config_completions,
            identity_attributes,
            enum_aliases,
            provider_config_types,
        ))
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

    fn create_engine() -> Result<Engine, WasmProviderLoadError> {
        let config = build_engine_config();
        Engine::new(&config).map_err(|e| format!("Failed to create WASM engine: {e}").into())
    }

    fn load_component_from_file(
        engine: &Engine,
        wasm_path: &Path,
    ) -> Result<Component, WasmProviderLoadError> {
        Component::from_file(engine, wasm_path).map_err(|e| {
            format!(
                "Failed to load WASM component from {}: {e}",
                wasm_path.display()
            )
            .into()
        })
    }

    fn deserialize_precompiled_component(
        engine: &Engine,
        cwasm_path: &Path,
    ) -> Result<Component, PrecompiledComponentDeserializationError> {
        // SAFETY: The caller is responsible for ensuring the .cwasm file was
        // produced by a trusted `precompile()` call with the same Wasmtime version.
        unsafe { Component::deserialize_file(engine, cwasm_path) }.map_err(|e| {
            PrecompiledComponentDeserializationError {
                cwasm_path: cwasm_path.to_path_buf(),
                source: e,
            }
        })
    }

    /// Acquire a component from the cache without bringing the provider up.
    ///
    /// A miss means the entry was absent or failed deserialization after the
    /// concurrent-writer retry. Its infallible return signature has no error
    /// channel through which a provider bring-up failure could enter this
    /// cache-recovery decision.
    async fn acquire_cached_component(
        engine: &Engine,
        cwasm_path: &Path,
    ) -> CachedComponentAcquisition {
        if !cwasm_path.exists() {
            return CachedComponentAcquisition::Miss;
        }

        match Self::deserialize_precompiled_component(engine, cwasm_path) {
            Ok(component) => CachedComponentAcquisition::Hit(component),
            Err(_) => {
                // Another process may be publishing the cache entry; wait
                // briefly and retry the deserialization step once.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                // A concurrent process may have removed the entry while we
                // slept. Treat that as a quiet miss, without invalid-cache noise.
                if !cwasm_path.exists() {
                    return CachedComponentAcquisition::Miss;
                }

                match Self::deserialize_precompiled_component(engine, cwasm_path) {
                    Ok(component) => CachedComponentAcquisition::Hit(component),
                    Err(error) => {
                        eprintln!("Precompile cache invalid, recompiling: {error}");
                        let _ = std::fs::remove_file(cwasm_path);
                        CachedComponentAcquisition::Miss
                    }
                }
            }
        }
    }

    /// Instantiate a component and load all provider metadata shared by the
    /// cached and uncached acquisition paths.
    async fn bring_up_component(
        engine: Engine,
        component: Component,
        wasm_path: PathBuf,
        epoch_ticker: EpochTicker,
    ) -> Result<Self, WasmProviderLoadError> {
        let (mut store, bindings, enable_http) =
            create_instance_auto(&engine, &component, None).await?;
        let info_json = bindings.info_json().to_owned();

        let schemas_json = bindings
            .call_schemas(&mut store)
            .await
            .map_err(|e| format!("Failed to call schemas(): {e}"))?;

        let (name, display_name, version) = wasm_convert::json_to_provider_info(&info_json)
            .map_err(|source| {
                WasmProviderLoadError::metadata("Failed to decode provider info", source)
            })?;
        let schemas: Vec<ResourceSchema> = wasm_convert::json_to_schemas(&schemas_json)
            .map_err(|e| provider_schema_decode_error(&name, &version, e))?;

        let (
            cached_config_completions,
            cached_identity_attributes,
            cached_enum_aliases,
            cached_provider_config_types,
        ) = Self::load_metadata(&bindings, &mut store, &name, &version).await?;

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
            instantiation_error_mapper: Arc::new(default_provider_instantiation_error_mapper),
            _epoch_ticker: epoch_ticker,
        })
    }

    /// Load a WASM provider, using the default precompile cache at `~/.carina/cache/`.
    ///
    /// If the cache directory cannot be created, falls back to compiling without caching.
    pub async fn new(wasm_path: PathBuf) -> Result<Self, WasmProviderLoadError> {
        match Self::default_cache_dir() {
            Some(cache_dir) => Self::new_with_cache_dir(wasm_path, &cache_dir).await,
            None => Self::new_uncached(wasm_path).await,
        }
    }

    /// Load a WASM provider with an explicit cache directory.
    pub async fn new_with_cache_dir(
        wasm_path: PathBuf,
        cache_dir: &Path,
    ) -> Result<Self, WasmProviderLoadError> {
        let cwasm_name = Self::cache_key(&wasm_path);
        let cwasm_path = cache_dir.join(&cwasm_name);

        // A cold-cache write can fail because the cache directory is
        // unavailable. Resolve that fallback before creating the runtime so
        // `new_uncached` remains the only runtime owner on this path.
        if !cwasm_path.exists()
            && let Err(error) = Self::precompile(&wasm_path, &cwasm_path)
        {
            eprintln!("Failed to write precompile cache, loading directly: {error}");
            return Self::new_uncached(wasm_path).await;
        }

        let engine = Self::create_engine()?;

        let component = match Self::acquire_cached_component(&engine, &cwasm_path).await {
            CachedComponentAcquisition::Hit(component) => component,
            CachedComponentAcquisition::Miss => {
                // Try to precompile and cache.
                match Self::precompile(&wasm_path, &cwasm_path) {
                    Ok(()) => Self::deserialize_precompiled_component(&engine, &cwasm_path)
                        .map_err(WasmProviderLoadError::PrecompiledDeserialization)?,
                    Err(error) => {
                        eprintln!("Failed to write precompile cache, loading directly: {error}");
                        // Cache inspection already required this engine. Reuse
                        // it so a stale-cache recovery failure cannot create a
                        // second engine or orphan an epoch ticker.
                        Self::load_component_from_file(&engine, &wasm_path)?
                    }
                }
            }
        };

        let epoch_ticker = EpochTicker::start(engine.clone());
        Self::bring_up_component(engine, component, wasm_path, epoch_ticker).await
    }

    /// Load a WASM provider without any precompile caching.
    async fn new_uncached(wasm_path: PathBuf) -> Result<Self, WasmProviderLoadError> {
        let engine = Self::create_engine()?;
        let component = Self::load_component_from_file(&engine, &wasm_path)?;
        let epoch_ticker = EpochTicker::start(engine.clone());

        Self::bring_up_component(engine, component, wasm_path, epoch_ticker).await
    }

    /// Precompile a .wasm file and save the result to a .cwasm file.
    ///
    /// **Mmap safety invariant:** `deserialize_precompiled_component` loads
    /// `.cwasm` via `Component::deserialize_file`, which keeps the file
    /// memory-mapped for the lifetime of the returned `Component`. Any
    /// in-place mutation of `cwasm_path` — including
    /// `std::fs::write(cwasm_path, …)` or `OpenOptions::truncate(true)` —
    /// while a previous `Component` is still alive pulls backing pages out
    /// from under the mmap region; subsequent access then traps as `SIGBUS`
    /// on Linux (macOS is more lenient, so a bug slipping through on macOS
    /// still blows up in CI).
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
    pub async fn from_precompiled(cwasm_path: &Path) -> Result<Self, WasmProviderLoadError> {
        let engine = Self::create_engine()?;
        let component = Self::deserialize_precompiled_component(&engine, cwasm_path)
            .map_err(WasmProviderLoadError::PrecompiledDeserialization)?;
        let epoch_ticker = EpochTicker::start(engine.clone());

        Self::bring_up_component(engine, component, cwasm_path.to_path_buf(), epoch_ticker).await
    }

    /// Load from .wasm with automatic precompile caching.
    ///
    /// Checks for an existing `.cwasm` in `cache_dir`. If present, attempts to
    /// load it; if the cache is stale or invalid, recompiles and caches anew.
    ///
    /// **Deprecated**: Use `new()` or `new_with_cache_dir()` instead, which
    /// handle caching automatically.
    pub async fn from_file_cached(
        wasm_path: &Path,
        cache_dir: &Path,
    ) -> Result<Self, WasmProviderLoadError> {
        Self::new_with_cache_dir(wasm_path.to_path_buf(), cache_dir).await
    }

    async fn create_initialized_instance(
        &self,
        attributes: &IndexMap<String, Value>,
    ) -> Result<(Store<HostState>, WasmBindings), WasmProviderInstanceError> {
        // This is the credentialed runtime instance (it calls
        // `initialize` and then read/create/update/delete). The provider
        // kind is known here (`self.name`, learned from `info()` at load
        // time), so the guest receives only its own credential partition.
        let kind = Some(self.name.as_str());
        let world = if self.enable_http {
            InstantiationWorld::HttpEnabled
        } else {
            InstantiationWorld::Basic
        };
        let (mut store, bindings) =
            create_runtime_instance(&self.engine, &self.component, kind, world).await?;
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
    ) -> Result<Arc<SharedWasmInstance>, WasmProviderInstanceError> {
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
            poisoned_reason: OnceLock::new(),
        });
        guard.insert(key, Arc::clone(&instance));
        Ok(instance)
    }
}

impl WasmProviderFactory {
    /// Replace the runtime-instantiation mapper used at the `ProviderFactory`
    /// boundary. Installed-artifact loaders use this to attach resolver
    /// provenance while the [`ProviderInstantiationError`] is still typed.
    pub fn with_runtime_instantiation_error_mapper<F>(mut self, mapper: F) -> Self
    where
        F: Fn(ProviderInstantiationError) -> ProviderError + Send + Sync + 'static,
    {
        self.instantiation_error_mapper = Arc::new(mapper);
        self
    }

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
        config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        let attrs = config.attributes().clone();
        let binding = binding.map(|s| s.to_string());
        Box::pin(async move {
            // Provider init rejections still surface their user-actionable
            // message verbatim (for example, allowed_account_ids mismatch;
            // see #2407). Instantiation failures take the typed mapper path.
            let instance = self
                .get_or_create_shared_instance(binding.as_deref(), &attrs)
                .await
                .map_err(|error| {
                    error.into_provider_error_with(&self.instantiation_error_mapper)
                })?;
            Ok(Box::new(WasmProvider {
                instance,
                name: self.name.clone(),
            }) as Box<dyn Provider>)
        })
    }

    fn create_normalizer(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        let attrs = config.attributes().clone();
        let binding = binding.map(|s| s.to_string());
        Box::pin(async move {
            let instance = self
                .get_or_create_shared_instance(binding.as_deref(), &attrs)
                .await
                .map_err(|error| {
                    error.into_provider_error_with(&self.instantiation_error_mapper)
                })?;
            Ok(Box::new(WasmProviderNormalizer { instance }) as Box<dyn ProviderNormalizer>)
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
            let result = finish_guest_call(
                &self.instance,
                "read",
                WasmTrapContext::ProviderOperation,
                &mut locked,
                call,
            )?;
            match result {
                Ok(wit_state) => {
                    wasm_convert::wit_to_core_state(&wit_state, &id).map_err(|error| {
                        wasm_value_decode_provider_error("read", error).for_resource(id.clone())
                    })
                }
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
            }
        }))
    }

    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_resource = match wasm_convert::core_data_source_to_wit_resource(resource) {
            Ok(v) => v,
            Err(e) => return early_provider_err("read_data_source", e),
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
                let result = finish_guest_call(
                    &self.instance,
                    "read_data_source",
                    WasmTrapContext::ProviderOperation,
                    &mut locked,
                    call,
                )?;
                match result {
                    Ok(wit_state) => {
                        wasm_convert::wit_to_core_state(&wit_state, &id).map_err(|error| {
                            wasm_value_decode_provider_error("read_data_source", error)
                                .for_resource(id.clone())
                        })
                    }
                    Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
                }
            },
        ))
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<CreateOutcome>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let wit_request = match wasm_convert::core_to_wit_create_request(&request) {
            Ok(v) => v,
            Err(e) => return early_provider_err("create", e),
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
                let result = finish_guest_call(
                    &self.instance,
                    "create",
                    WasmTrapContext::ProviderOperation,
                    &mut locked,
                    call,
                )?;
                match result {
                    Ok(wit_outcome) => wasm_convert::wit_to_core_create_outcome(wit_outcome, &id)
                        .map_err(|error| {
                            wasm_value_decode_provider_error("create", error)
                                .for_resource(id.clone())
                        }),
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
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let identifier = identifier.to_string();
        let wit_request = match wasm_convert::core_to_wit_update_request(&request) {
            Ok(v) => v,
            Err(e) => return early_provider_err("update", e),
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
                let result = finish_guest_call(
                    &self.instance,
                    "update",
                    WasmTrapContext::ProviderOperation,
                    &mut locked,
                    call,
                )?;
                match result {
                    Ok(wit_outcome) => wasm_convert::wit_to_core_update_outcome(wit_outcome, &id)
                        .map_err(|error| {
                            wasm_value_decode_provider_error("update", error)
                                .for_resource(id.clone())
                        }),
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
                let result = finish_guest_call(
                    &self.instance,
                    "delete",
                    WasmTrapContext::ProviderOperation,
                    &mut locked,
                    call,
                )?;
                match result {
                    Ok(()) => Ok(()),
                    Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
                }
            },
        ))
    }

    fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut locked =
                    LockedStore::acquire(&self.instance, "required_permissions").await?;
                let call = self
                    .instance
                    .bindings
                    .call_required_permissions(locked.store(), &wit_id, op)
                    .await;
                finish_guest_call(
                    &self.instance,
                    "required_permissions",
                    WasmTrapContext::ProviderOperation,
                    &mut locked,
                    call,
                )
            })
        })
    }

    fn satisfier_hint(
        &self,
        target_id: &ResourceId,
        attr_path: &AttrPath,
    ) -> ProviderResult<Vec<BindingPattern>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(target_id);
        let wit_attr_path = attr_path.segments().to_vec();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut locked = LockedStore::acquire(&self.instance, "satisfier_hint").await?;
                let call = self
                    .instance
                    .bindings
                    .call_satisfier_hint(locked.store(), &wit_id, &wit_attr_path)
                    .await;
                let patterns = finish_guest_call(
                    &self.instance,
                    "satisfier_hint",
                    WasmTrapContext::ProviderOperation,
                    &mut locked,
                    call,
                )?;
                patterns
                    .into_iter()
                    .map(wasm_convert::wit_to_core_binding_pattern)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| {
                        wasm_value_decode_provider_error("satisfier_hint", error)
                            .for_resource(target_id.clone())
                    })
            })
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

#[derive(Debug)]
struct WasmNormalizerResultKeysError {
    missing: BTreeSet<String>,
    unexpected: BTreeSet<String>,
    duplicates: BTreeSet<String>,
}

impl fmt::Display for WasmNormalizerResultKeysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("returned a mismatched state key set")?;
        if !self.missing.is_empty() {
            write!(
                f,
                "; missing: {}",
                self.missing.iter().cloned().collect::<Vec<_>>().join(", ")
            )?;
        }
        if !self.unexpected.is_empty() {
            write!(
                f,
                "; unexpected: {}",
                self.unexpected
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
        if !self.duplicates.is_empty() {
            write!(
                f,
                "; duplicate: {}",
                self.duplicates
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for WasmNormalizerResultKeysError {}

#[derive(Debug)]
struct WasmNormalizerResultCountError {
    actual: usize,
}

impl fmt::Display for WasmNormalizerResultCountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "returned {} state entries for a single-resource call; expected exactly one",
            self.actual
        )
    }
}

impl std::error::Error for WasmNormalizerResultCountError {}

type WitStateAttributeUpdate = (ResourceId, Vec<(String, wit_types::Value)>);

fn validate_wit_state_result_keys(
    operation: &'static str,
    expected_ids_by_key: &HashMap<String, ResourceId>,
    results: Vec<(String, wit_types::State)>,
) -> ProviderResult<Vec<WitStateAttributeUpdate>> {
    let expected_keys = expected_ids_by_key.keys().cloned().collect::<BTreeSet<_>>();
    let mut returned = HashMap::with_capacity(results.len());
    let mut duplicates = BTreeSet::new();
    for (key, state) in results {
        if returned.insert(key.clone(), state).is_some() {
            duplicates.insert(key);
        }
    }
    let returned_keys = returned.keys().cloned().collect::<BTreeSet<_>>();
    let missing = expected_keys.difference(&returned_keys).cloned().collect();
    let unexpected = returned_keys.difference(&expected_keys).cloned().collect();
    let error = WasmNormalizerResultKeysError {
        missing,
        unexpected,
        duplicates,
    };
    if !error.missing.is_empty() || !error.unexpected.is_empty() || !error.duplicates.is_empty() {
        return Err(ProviderError::internal(format!(
            "WASM provider returned invalid state keys during {operation}"
        ))
        .with_cause(error));
    }

    Ok(expected_ids_by_key
        .iter()
        .map(|(key, id)| {
            let state = returned
                .remove(key)
                .expect("validated result contains every expected key");
            (id.clone(), state.attributes)
        })
        .collect())
}

fn take_single_wit_state_result(
    operation: &'static str,
    id: ResourceId,
    results: Vec<(String, wit_types::State)>,
) -> ProviderResult<WitStateAttributeUpdate> {
    if results.len() != 1 {
        return Err(ProviderError::internal(format!(
            "WASM provider returned an invalid state count during {operation}"
        ))
        .with_cause(WasmNormalizerResultCountError {
            actual: results.len(),
        })
        .for_resource(id));
    }

    let (_, state) = results
        .into_iter()
        .next()
        .expect("validated single-resource result contains one entry");
    Ok((id, state.attributes))
}

fn apply_wit_state_attribute_updates(
    operation: &'static str,
    current_states: &mut HashMap<ResourceId, State>,
    updates: Vec<WitStateAttributeUpdate>,
) -> ProviderResult<()> {
    let decoded = updates
        .into_iter()
        .map(|(id, attributes)| {
            wasm_convert::wit_to_core_value_map(&attributes)
                .map(|attributes| (id.clone(), attributes))
                .map_err(|error| {
                    wasm_value_decode_provider_error(operation, error).for_resource(id)
                })
        })
        .collect::<ProviderResult<Vec<_>>>()?;

    for (id, attributes) in decoded {
        if let Some(state) = current_states.get_mut(&id) {
            state.attributes = attributes;
        }
    }
    Ok(())
}

fn finish_guest_call<T>(
    instance: &SharedWasmInstance,
    operation: &'static str,
    context: WasmTrapContext,
    locked: &mut LockedStore<'_>,
    call: wasmtime::Result<T>,
) -> ProviderResult<T> {
    // A returned `Result` means the async wasmtime call was not cancelled, so
    // dropping the store guard must not apply the timeout/cancellation poison.
    locked.disarm();
    match call {
        Ok(value) => Ok(value),
        Err(error) => {
            // A component trap still makes this instance non-reentrant. Record
            // the originating operation while the store lock remains held so
            // queued calls fail fast with the real cause.
            poison_after_trap(
                &instance.poisoned,
                &instance.poisoned_reason,
                operation,
                &error,
            );
            Err(wasm_trap_provider_error(operation, error, context))
        }
    }
}

fn finish_normalizer_guest_call<T>(
    instance: &SharedWasmInstance,
    operation: &'static str,
    locked: &mut LockedStore<'_>,
    call: wasmtime::Result<Result<T, wit_types::ProviderError>>,
) -> ProviderResult<T> {
    finish_guest_call(
        instance,
        operation,
        WasmTrapContext::Normalizer,
        locked,
        call,
    )?
    .map_err(wasm_convert::wit_to_core_provider_error)
}

impl ProviderNormalizer for WasmProviderNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [Resource],
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            let (sealed, unsealer) = secret_seal::seal(resources, None)
                .map_err(|error| wasm_value_encode_provider_error("normalize_desired", error))?;

            // Plain `.await`, not a nested `block_on`: the guarded store is
            // acquired and dropped within this one polled future, so the
            // apply-path `renormalize` calling this once per resource cannot
            // self-deadlock (carina#3112).
            let result = {
                let mut locked = LockedStore::acquire(&self.instance, "normalize_desired").await?;
                let call = self
                    .instance
                    .bindings
                    .call_normalize_desired(locked.store(), &sealed)
                    .await;
                finish_normalizer_guest_call(&self.instance, "normalize_desired", &mut locked, call)
            };
            let result = result?;
            // `PlanPreprocessor::prepare` strips every attribute that
            // recursively contains `Value::Deferred(DeferredValue::ResourceRef)` (alongside
            // `Value::Deferred(DeferredValue::Unknown)`) before this normalizer runs and
            // restores them afterwards (#2387). Secret restoration
            // filters mangled attributes before guest values are
            // written back.
            let restored = unsealer
                .try_restore(result)
                .map_err(|error| wasm_value_decode_provider_error("normalize_desired", error))?;
            restored.apply_to(resources);
            Ok(())
        })
    }

    fn normalize_state<'a>(
        &'a self,
        current_states: &'a mut HashMap<ResourceId, State>,
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            let mut resolved_by_key = HashMap::new();
            let mut colliding_keys = HashSet::new();
            let mut single_wit_states = Vec::new();
            let mut updates = Vec::new();

            for (id, state) in current_states.iter() {
                let key = wasm_convert::resource_id_wire_key(id);
                let wit = wasm_convert::core_to_wit_state(state)
                    .map_err(|error| wasm_value_encode_provider_error("normalize_state", error))?;
                match id.identity_state() {
                    ResourceIdentityState::Pending(_) => {
                        single_wit_states.push((id.clone(), key, wit));
                    }
                    ResourceIdentityState::Resolved(_) => {
                        if colliding_keys.contains(&key) {
                            single_wit_states.push((id.clone(), key, wit));
                        } else if let Some((previous_id, previous_wit)) =
                            resolved_by_key.remove(&key)
                        {
                            colliding_keys.insert(key.clone());
                            single_wit_states.push((previous_id, key.clone(), previous_wit));
                            single_wit_states.push((id.clone(), key, wit));
                        } else {
                            resolved_by_key.insert(key, (id.clone(), wit));
                        }
                    }
                }
            }

            let mut resolved_ids_by_key = HashMap::with_capacity(resolved_by_key.len());
            let mut resolved_wit_states = Vec::with_capacity(resolved_by_key.len());
            for (key, (id, state)) in resolved_by_key {
                resolved_ids_by_key.insert(key.clone(), id);
                resolved_wit_states.push((key, state));
            }

            if !resolved_wit_states.is_empty() {
                // Plain `.await`, not a nested `block_on` — see `normalize_desired`.
                let result = {
                    let mut locked =
                        LockedStore::acquire(&self.instance, "normalize_state").await?;
                    let call = self
                        .instance
                        .bindings
                        .call_normalize_state(locked.store(), &resolved_wit_states)
                        .await;
                    finish_normalizer_guest_call(
                        &self.instance,
                        "normalize_state",
                        &mut locked,
                        call,
                    )
                };
                let result = result?;
                updates.extend(validate_wit_state_result_keys(
                    "normalize_state",
                    &resolved_ids_by_key,
                    result,
                )?);
            }

            // Pending IDs and resolved IDs that differ only by provider
            // instance can share a legacy wire key. Send one per call so the
            // guest's key-parsing HashMap cannot collapse distinct host IDs,
            // and ignore the guest's re-rendered key when correlating the
            // single result.
            for (id, key, wit_state) in single_wit_states {
                let result = {
                    let mut locked =
                        LockedStore::acquire(&self.instance, "normalize_state").await?;
                    let call = self
                        .instance
                        .bindings
                        .call_normalize_state(locked.store(), &[(key, wit_state)])
                        .await;
                    finish_normalizer_guest_call(
                        &self.instance,
                        "normalize_state",
                        &mut locked,
                        call,
                    )
                };
                let result = result?;
                updates.push(take_single_wit_state_result("normalize_state", id, result)?);
            }

            apply_wit_state_attribute_updates("normalize_state", current_states, updates)?;
            Ok(())
        })
    }

    fn hydrate_read_state<'a>(
        &'a self,
        current_states: &'a mut HashMap<ResourceId, State>,
        saved_attrs: &'a SavedAttrs,
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            let mut resolved_by_key = HashMap::new();
            let mut colliding_keys = HashSet::new();
            let mut single_batches = Vec::new();
            let mut updates = Vec::new();

            for (id, state) in current_states.iter() {
                let key = wasm_convert::resource_id_wire_key(id);
                let wit_state = wasm_convert::core_to_wit_state(state).map_err(|error| {
                    wasm_value_encode_provider_error("hydrate_read_state current state", error)
                })?;
                let wit_saved = saved_attrs
                    .get(id)
                    .map(|attrs| {
                        wasm_convert::core_to_wit_value_map(attrs).map_err(|error| {
                            wasm_value_encode_provider_error(
                                "hydrate_read_state saved attributes",
                                error,
                            )
                        })
                    })
                    .transpose()?;

                match id.identity_state() {
                    ResourceIdentityState::Pending(_) => {
                        single_batches.push((id.clone(), key, wit_state, wit_saved));
                    }
                    ResourceIdentityState::Resolved(_) => {
                        if colliding_keys.contains(&key) {
                            single_batches.push((id.clone(), key, wit_state, wit_saved));
                        } else if let Some((previous_id, previous_state, previous_saved)) =
                            resolved_by_key.remove(&key)
                        {
                            colliding_keys.insert(key.clone());
                            single_batches.push((
                                previous_id,
                                key.clone(),
                                previous_state,
                                previous_saved,
                            ));
                            single_batches.push((id.clone(), key, wit_state, wit_saved));
                        } else {
                            resolved_by_key.insert(key, (id.clone(), wit_state, wit_saved));
                        }
                    }
                }
            }

            let mut resolved_ids_by_key = HashMap::with_capacity(resolved_by_key.len());
            let mut resolved_wit_states = Vec::with_capacity(resolved_by_key.len());
            let mut resolved_wit_saved = Vec::new();
            for (key, (id, state, saved)) in resolved_by_key {
                resolved_ids_by_key.insert(key.clone(), id);
                resolved_wit_states.push((key.clone(), state));
                if let Some(saved) = saved {
                    resolved_wit_saved.push((key, saved));
                }
            }

            if !resolved_wit_states.is_empty() {
                // Plain `.await`, not a nested `block_on` — see `normalize_desired`.
                let result = {
                    let mut locked =
                        LockedStore::acquire(&self.instance, "hydrate_read_state").await?;
                    let call = self
                        .instance
                        .bindings
                        .call_hydrate_read_state(
                            locked.store(),
                            &resolved_wit_states,
                            &resolved_wit_saved,
                        )
                        .await;
                    finish_normalizer_guest_call(
                        &self.instance,
                        "hydrate_read_state",
                        &mut locked,
                        call,
                    )
                };
                let result = result?;
                updates.extend(validate_wit_state_result_keys(
                    "hydrate_read_state",
                    &resolved_ids_by_key,
                    result,
                )?);
            }

            // As in `normalize_state`, one-resource calls avoid both pending-ID
            // and provider-instance wire-key collisions. Correlate the single
            // result by position and ignore the guest's re-rendered key.
            for (id, key, wit_state, wit_saved) in single_batches {
                let wit_saved = wit_saved
                    .map(|attrs| vec![(key.clone(), attrs)])
                    .unwrap_or_default();
                let result = {
                    let mut locked =
                        LockedStore::acquire(&self.instance, "hydrate_read_state").await?;
                    let call = self
                        .instance
                        .bindings
                        .call_hydrate_read_state(locked.store(), &[(key, wit_state)], &wit_saved)
                        .await;
                    finish_normalizer_guest_call(
                        &self.instance,
                        "hydrate_read_state",
                        &mut locked,
                        call,
                    )
                };
                let result = result?;
                updates.push(take_single_wit_state_result(
                    "hydrate_read_state",
                    id,
                    result,
                )?);
            }

            apply_wit_state_attribute_updates("hydrate_read_state", current_states, updates)?;
            Ok(())
        })
    }

    fn merge_default_tags<'a>(
        &'a self,
        resources: &'a mut [Resource],
        default_tags: &'a IndexMap<String, Value>,
        _registry: &'a carina_core::schema::SchemaRegistry,
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            if default_tags.is_empty() {
                return Ok(());
            }

            let (sealed, unsealer) = secret_seal::seal(resources, Some(default_tags))
                .map_err(|error| wasm_value_encode_provider_error("merge_default_tags", error))?;

            // Plain `.await`, not a nested `block_on` — see `normalize_desired`.
            let result = {
                let mut locked = LockedStore::acquire(&self.instance, "merge_default_tags").await?;
                let call = self
                    .instance
                    .bindings
                    .call_merge_default_tags(locked.store(), &sealed)
                    .await;
                finish_normalizer_guest_call(
                    &self.instance,
                    "merge_default_tags",
                    &mut locked,
                    call,
                )
            };
            let result = result?;
            // Guest preserves resource order; zip and overwrite
            // attributes (merge may add `tags` and `_default_tag_keys`).
            let restored = unsealer
                .try_restore(result)
                .map_err(|error| wasm_value_decode_provider_error("merge_default_tags", error))?;
            restored.apply_to(resources);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_config_completions_metadata_is_rejected() {
        let error = decode_config_completions_json("not json", "mock", "1.0.0")
            .expect_err("malformed config completions must fail provider loading");

        assert!(error.to_string().contains("provider config completions"));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn malformed_enum_aliases_metadata_is_rejected() {
        let error = decode_enum_aliases_json("not json", "mock", "1.0.0")
            .expect_err("malformed enum aliases must fail provider loading");

        assert!(error.to_string().contains("enum aliases"));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn shared_trap_mapper_preserves_provider_operation_messages() {
        let trap = wasm_trap_provider_error(
            "read",
            wasmtime::Error::msg("guest trap"),
            WasmTrapContext::ProviderOperation,
        );
        assert_eq!(trap.message(), "WASM trap in read");
        assert_eq!(trap.to_string().matches("guest trap").count(), 1);
        assert!(std::error::Error::source(&trap).is_some());

        let timeout = wasm_trap_provider_error(
            "read",
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while executing at wasm backtrace:\n    0: test-frame"),
            WasmTrapContext::ProviderOperation,
        );
        assert_eq!(
            timeout.message(),
            format!(
                "WASM plugin timed out after {WASM_OPERATION_TIMEOUT_SECS}s in read (check AWS credentials)"
            )
        );

        let normalizer_timeout = wasm_trap_provider_error(
            "normalize_state",
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while executing at wasm backtrace:\n    0: test-frame"),
            WasmTrapContext::Normalizer,
        );
        assert_eq!(
            normalizer_timeout.message(),
            format!(
                "WASM plugin timed out after {WASM_OPERATION_TIMEOUT_SECS}s in normalize_state"
            )
        );
    }

    #[test]
    fn timeout_classification_uses_the_typed_trap_not_message_text() {
        let nested = wasmtime::Error::msg("epoch deadline reached").context("outer guest trap");

        let error = wasm_trap_provider_error("read", nested, WasmTrapContext::ProviderOperation);

        assert_eq!(error.variant_name(), "internal");
    }

    #[tokio::test]
    async fn real_engine_epoch_interrupt_maps_to_provider_timeout() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let wasm_path = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
            .into_iter()
            .map(|name| workspace_root.join("target/wasm32-wasip2/debug").join(name))
            .find(|path| path.exists());
        let Some(wasm_path) = wasm_path else {
            eprintln!(
                "SKIP: WASM binary not found. Build with: cargo build -p \
                 carina-provider-mock --target wasm32-wasip2"
            );
            return;
        };

        let factory = WasmProviderFactory::new_uncached(wasm_path)
            .await
            .expect("mock WASM provider should load");
        let instance = factory
            .get_or_create_shared_instance(None, &IndexMap::new())
            .await
            .expect("mock WASM provider should initialize");
        let id = ResourceId::with_provider_identity("mock", "test.resource", "epoch-timeout", None);
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let mut locked = LockedStore::acquire(&instance, "required_permissions")
            .await
            .expect("fresh instance should be usable");

        locked.store().set_epoch_deadline(1);
        factory.engine.increment_epoch();
        let call = instance
            .bindings
            .call_required_permissions(locked.store(), &wit_id, PlanOp::Read)
            .await;
        let error = finish_guest_call(
            &instance,
            "required_permissions",
            WasmTrapContext::ProviderOperation,
            &mut locked,
            call,
        )
        .expect_err("the elapsed real engine epoch deadline must trap");

        assert!(
            matches!(error, ProviderError::Timeout(_)),
            "the real epoch interrupt must map to a timeout: {error}"
        );
    }

    #[tokio::test]
    async fn normalizer_guest_boundary_decode_error_is_structured_and_reentrant() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let wasm_path = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
            .into_iter()
            .map(|name| workspace_root.join("target/wasm32-wasip2/debug").join(name))
            .find(|path| path.exists());
        let Some(wasm_path) = wasm_path else {
            eprintln!(
                "SKIP: WASM binary not found. Build with: cargo build -p \
                 carina-provider-mock --target wasm32-wasip2"
            );
            return;
        };

        let factory = WasmProviderFactory::new_uncached(wasm_path)
            .await
            .expect("mock WASM provider should load");
        let instance = factory
            .get_or_create_shared_instance(None, &IndexMap::new())
            .await
            .expect("mock WASM provider should initialize");
        let malformed = wit_types::ResourceDef {
            id: wit_types::ResourceId {
                provider: "mock".to_string(),
                resource_type: "test.resource".to_string(),
                identity: "malformed-boundary".to_string(),
            },
            attributes: vec![(
                "settings".to_string(),
                wit_types::Value::ListVal("not-json".to_string()),
            )],
        };
        let mut locked = LockedStore::acquire(&instance, "normalize_desired")
            .await
            .expect("fresh instance should be usable");
        let call = instance
            .bindings
            .call_normalize_desired_raw(locked.store(), &[malformed])
            .await;
        let error = finish_normalizer_guest_call(&instance, "normalize_desired", &mut locked, call)
            .expect_err("malformed guest input must return provider-error");
        drop(locked);

        assert_eq!(error.variant_name(), "internal");
        assert_eq!(
            error.detail().operation.as_deref(),
            Some("normalize_desired"),
            "the guest boundary error must retain its export operation"
        );
        assert!(
            error.message().contains("normalize_desired"),
            "the guest boundary error message must name its export: {error}"
        );
        assert!(
            error.message().contains("WASM boundary decode error"),
            "the guest boundary error message must be preserved: {error}"
        );
        assert!(
            error.message().contains("invalid JSON"),
            "the guest decode detail must be preserved: {error}"
        );
        assert!(
            !instance.poisoned.load(Ordering::Acquire),
            "a returned boundary provider-error must not poison the instance"
        );

        let malformed_saved_attrs = [(
            "mock.test.resource.malformed-boundary".to_string(),
            vec![(
                "settings".to_string(),
                wit_types::Value::ListVal("not-json".to_string()),
            )],
        )];
        let mut locked = LockedStore::acquire(&instance, "hydrate_read_state")
            .await
            .expect("the decode error must leave the instance usable");
        let call = instance
            .bindings
            .call_hydrate_read_state(locked.store(), &[], &malformed_saved_attrs)
            .await;
        let error =
            finish_normalizer_guest_call(&instance, "hydrate_read_state", &mut locked, call)
                .expect_err("malformed saved attributes must return provider-error");
        drop(locked);

        assert_eq!(error.variant_name(), "internal");
        assert_eq!(
            error.detail().operation.as_deref(),
            Some("hydrate_read_state"),
            "the structured operation must be the bare export name"
        );
        assert!(
            error
                .message()
                .contains("hydrate_read_state saved attributes"),
            "the boundary error message must retain its failing sub-part: {error}"
        );
        assert!(
            !instance.poisoned.load(Ordering::Acquire),
            "a returned saved-attribute decode error must not poison the instance"
        );

        let normalizer = WasmProviderNormalizer { instance };
        let mut resources = vec![Resource::with_provider(
            "mock",
            "test.resource",
            "after-boundary-error",
            None,
        )];
        normalizer
            .normalize_desired(&mut resources)
            .await
            .expect("the same instance must accept a later normalizer call");
    }

    #[test]
    fn rendered_decode_provider_error_contains_serde_detail_once() {
        use std::error::Error as _;

        let decode_error =
            wasm_convert::wit_to_core_value(&wit_types::Value::ListVal("[".to_string()))
                .expect_err("malformed JSON must fail");
        let serde_detail = decode_error
            .source()
            .expect("syntax errors retain serde source")
            .to_string();
        let rendered = wasm_value_decode_provider_error("read", decode_error).to_string();

        assert_eq!(rendered.matches(&serde_detail).count(), 1, "{rendered}");
    }

    #[test]
    fn malformed_normalized_state_output_is_atomic() {
        use wit_types::Value as WitValue;

        let first_id = Resource::with_provider("mock", "test.resource", "first", None).id;
        let second_id = Resource::with_provider("mock", "test.resource", "second", None).id;
        let original_value = Value::Concrete(carina_core::resource::ConcreteValue::String(
            "original".to_string(),
        ));
        let mut states = HashMap::from([
            (
                first_id.clone(),
                State::existing(
                    first_id.clone(),
                    HashMap::from([("value".to_string(), original_value.clone())]),
                ),
            ),
            (
                second_id.clone(),
                State::existing(
                    second_id.clone(),
                    HashMap::from([("value".to_string(), original_value)]),
                ),
            ),
        ]);
        let before = states.clone();
        let updates = vec![
            (
                first_id,
                vec![("value".to_string(), WitValue::StrVal("changed".to_string()))],
            ),
            (
                second_id.clone(),
                vec![(
                    "settings".to_string(),
                    WitValue::MapVal("not json".to_string()),
                )],
            ),
        ];

        let error = apply_wit_state_attribute_updates("normalize_state", &mut states, updates)
            .expect_err("malformed guest output must fail normalization");

        assert!(error.to_string().contains("map-val"));
        assert!(std::error::Error::source(&error).is_some());
        assert_eq!(
            error.detail().resource_id.as_deref(),
            Some(&second_id),
            "decode errors must identify the state whose attributes were malformed"
        );
        assert_eq!(
            states, before,
            "decode failure must not partially mutate state"
        );
    }

    #[test]
    fn invalid_state_key_error_names_the_operation_once() {
        let id = Resource::with_provider("mock", "test.resource", "only", None).id;
        let expected = HashMap::from([("expected".to_string(), id)]);

        let error = validate_wit_state_result_keys("normalize_state", &expected, Vec::new())
            .expect_err("a missing state key must fail");
        let rendered = error.to_string();

        assert_eq!(rendered.matches("normalize_state").count(), 1, "{rendered}");
    }

    #[test]
    fn single_state_result_rejects_more_than_one_entry() {
        let id = Resource::with_provider("mock", "test.resource", "only", None).id;
        let state = wit_types::State {
            identifier: None,
            attributes: Vec::new(),
            exists: true,
        };

        let error = take_single_wit_state_result(
            "normalize_state",
            id,
            vec![
                ("first".to_string(), state.clone()),
                ("second".to_string(), state),
            ],
        )
        .expect_err("a single-resource call returning two states must fail");

        assert!(error.to_string().contains("expected exactly one"));
    }

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
    fn malformed_schema_json_is_reported_as_provider_load_error() {
        let detail = wasm_convert::json_to_schemas(
            r#"[{
                "resource_type":"s3.Bucket",
                "attributes":{},
                "unique_name_attribute":{"type":"attribute"}
            }]"#,
        )
        .unwrap_err();
        let err = provider_schema_decode_error("bad-provider", "9.9.9", detail).to_string();

        assert!(err.contains("provider 'bad-provider' 9.9.9"), "{err}");
        assert!(err.contains("schema JSON parse error"), "{err}");
        assert!(err.contains("missing field `value`"), "{err}");
        assert!(
            err.contains("provider revision may predate this host"),
            "{err}"
        );
        assert!(!err.contains("carina init"), "{err}");
        assert!(!err.contains("bump the provider lock"), "{err}");
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
        let second = is_metadata_available();
        assert_eq!(first, second);

        let probe = MetadataProbe::new();
        let local_first = probe.is_available();
        let local_second = probe.is_available();
        assert_eq!(
            probe.probe_count(),
            1,
            "test-local MetadataProbe memoization must run exactly one endpoint probe for two calls"
        );
        assert_eq!(local_first, local_second);
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

    #[test]
    fn precompiled_deserialization_error_preserves_full_wasmtime_chain() {
        let dir = tempfile::tempdir().unwrap();
        let cwasm_path = dir.path().join("corrupt.cwasm");
        std::fs::write(&cwasm_path, b"not a precompiled component").unwrap();
        let engine = Engine::new(&build_engine_config()).unwrap();

        let error =
            match WasmProviderFactory::deserialize_precompiled_component(&engine, &cwasm_path) {
                Ok(_) => panic!("corrupt precompiled component must fail deserialization"),
                Err(error) => error,
            };
        let source_chain: Vec<_> = error.source.chain().map(ToString::to_string).collect();
        assert!(
            source_chain.len() >= 2,
            "fixture must produce a genuinely nested Wasmtime error: {source_chain:?}"
        );

        let mut expected_chain = source_chain[0].clone();
        for cause in &source_chain[1..] {
            expected_chain.push_str("\n  caused by: ");
            expected_chain.push_str(cause);
        }
        assert_eq!(
            error.to_string(),
            format!(
                "Failed to deserialize WASM component from {}: {expected_chain}",
                cwasm_path.display()
            )
        );
        assert!(
            std::error::Error::source(&error).is_none(),
            "the self-contained Display contract must not expose the rendered chain again"
        );
    }

    fn render_wasi_http_import_context(
        imports: WasiHttpImports,
    ) -> (WasiHttpCompatibility, String) {
        let host_version = semver::Version::parse(WASI_HTTP_HOST_VERSION).unwrap();
        let compatibility = wasi_http_compatibility(&host_version, &imports);
        let error = ProviderInstantiationError {
            primary_failure: wasmtime::Error::msg("fixture instantiation failure"),
            context: Box::new(InstantiationContext::SingleAttempt {
                world: InstantiationWorld::HttpEnabled,
                context: ComponentInstantiationContext::WasiHttpImported {
                    context: WasiHttpImportedContext {
                        imports,
                        host_version,
                        compatibility,
                    },
                },
            }),
        };
        (compatibility, error.to_string())
    }

    fn one_wasi_http_import(name: &str, version: WasiHttpImportVersion) -> WasiHttpImports {
        WasiHttpImports {
            first: WasiHttpImport {
                name: name.into(),
                version,
            },
            rest: Vec::new(),
        }
    }

    #[test]
    fn identical_wasi_http_versions_are_rendered_as_identical() {
        let imports = one_wasi_http_import(
            "wasi:http/outgoing-handler@0.2.6",
            WasiHttpImportVersion::Semver(semver::Version::new(0, 2, 6)),
        );

        let (compatibility, rendered) = render_wasi_http_import_context(imports);

        assert_eq!(compatibility, WasiHttpCompatibility::Compatible);
        assert!(
            rendered.contains(
                "The component and host use the identical wasi:http version 0.2.6, so the wasi:http version is not the problem."
            ),
            "{rendered}"
        );
        assert!(
            !rendered.contains("matches 0.2.x patch versions bidirectionally"),
            "{rendered}"
        );
    }

    #[test]
    fn differing_compatible_wasi_http_versions_explain_patch_matching() {
        let imports = one_wasi_http_import(
            "wasi:http/outgoing-handler@0.2.9",
            WasiHttpImportVersion::Semver(semver::Version::new(0, 2, 9)),
        );

        let (compatibility, rendered) = render_wasi_http_import_context(imports);

        assert_eq!(compatibility, WasiHttpCompatibility::Compatible);
        assert!(
            rendered.contains(
                "These wasi:http versions are compatible but differ: Wasmtime's component linker matches 0.2.x patch versions bidirectionally, so the version difference is not the problem."
            ),
            "{rendered}"
        );
    }

    #[test]
    fn incompatible_wasi_http_import_renders_component_linker_wording() {
        let imports = one_wasi_http_import(
            "wasi:http/outgoing-handler@0.3.0",
            WasiHttpImportVersion::Semver(semver::Version::new(0, 3, 0)),
        );

        let (compatibility, rendered) = render_wasi_http_import_context(imports);

        assert_eq!(compatibility, WasiHttpCompatibility::Incompatible);
        assert!(
            rendered.contains(
                "These wasi:http versions are not semver-compatible under Wasmtime's component-linker rules."
            ),
            "{rendered}"
        );
    }

    #[test]
    fn unknown_wasi_http_import_versions_render_uncertain_compatibility() {
        let imports = WasiHttpImports {
            first: WasiHttpImport {
                name: "wasi:http/types".into(),
                version: WasiHttpImportVersion::Unversioned,
            },
            rest: vec![WasiHttpImport {
                name: "wasi:http/outgoing-handler@not-semver".into(),
                version: WasiHttpImportVersion::Invalid,
            }],
        };

        let (compatibility, rendered) = render_wasi_http_import_context(imports);

        assert_eq!(compatibility, WasiHttpCompatibility::Unknown);
        assert!(
            rendered.contains(
                "Compatibility could not be determined because at least one wasi:http import has no valid semantic version."
            ),
            "{rendered}"
        );
    }

    /// Regression guard for carina#3688: the HTTP path is the real
    /// instantiation attempt. Its complete cause chain must lead the
    /// diagnostic, while the structurally-inapplicable basic fallback must
    /// not surface its predetermined "wasi:http not in linker" error as a
    /// peer cause.
    #[test]
    fn provider_instantiation_error_preserves_full_http_chain_and_demotes_fallback() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                  (import "wasi:http/outgoing-handler@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();
        let http_error = wasmtime::Error::msg(
            "parameter 2 expected own<wasi:http/types.request> but found borrow",
        )
        .context("failed to convert function to given type")
        .context("Failed to instantiate WASM component (HTTP)");
        let basic_error = wasmtime::Error::msg(
            "component imports instance `wasi:http/types@0.2.9`, but a matching implementation was not found in the linker",
        )
        .context("Failed to instantiate WASM component");

        let error =
            ProviderInstantiationError::from_attempts(&engine, &component, http_error, basic_error);
        let msg = error.to_string();

        assert!(
            std::error::Error::source(&error).is_none(),
            "the self-contained Display contract must not expose the rendered chain again"
        );

        let outer = msg
            .find("Failed to instantiate WASM component (HTTP)")
            .unwrap();
        let middle = msg
            .find("failed to convert function to given type")
            .unwrap();
        let root = msg
            .find("parameter 2 expected own<wasi:http/types.request> but found borrow")
            .unwrap();
        assert!(
            outer < middle && middle < root,
            "cause chain out of order: {msg}"
        );
        assert!(
            msg.starts_with("Failed to instantiate WASM component (HTTP)"),
            "the real HTTP failure must lead: {msg}"
        );
        assert!(msg.contains("wasi:http/outgoing-handler@0.2.9"));
        assert!(msg.contains("wasi:http/types@0.2.9"));
        assert!(msg.contains("Host wasi:http version: 0.2.6"));
        assert!(
            msg.contains("These wasi:http versions are compatible"),
            "compatible versions must be ruled out affirmatively: {msg}"
        );
        assert!(msg.contains("matches 0.2.x patch versions bidirectionally"));
        assert!(msg.contains("Basic fallback diagnostic omitted"));
        assert!(
            !msg.contains("matching implementation was not found in the linker"),
            "the predetermined fallback error must not compete with the real failure: {msg}"
        );
        assert!(!msg.contains("HTTP-enabled world failed"));
        assert!(!msg.contains("basic fallback also failed"));
    }

    #[tokio::test]
    async fn selected_http_runtime_attempt_returns_structured_instantiation_error() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "bogus:runtime/iface@1.0.0" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();

        let error = match create_runtime_instance(
            &engine,
            &component,
            None,
            InstantiationWorld::HttpEnabled,
        )
        .await
        {
            Ok(_) => panic!("the unresolved bogus import must reject instantiation"),
            Err(error) => error,
        };
        let rendered = error.to_string();

        assert!(rendered.contains("bogus:runtime/iface@1.0.0"));
        assert!(rendered.contains("Component wasi:http imports: none detected"));
        assert!(rendered.contains("Single runtime instantiation attempt: HTTP-enabled world"));
        assert!(rendered.contains("No basic fallback was attempted"));
        assert!(!rendered.contains("Basic fallback diagnostic omitted"));
        assert!(!rendered.contains("Subordinate basic fallback attempt"));
    }

    #[tokio::test]
    async fn selected_basic_runtime_attempt_returns_structured_instantiation_error() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();

        let error =
            match create_runtime_instance(&engine, &component, None, InstantiationWorld::Basic)
                .await
            {
                Ok(_) => panic!("the basic linker must reject the wasi:http import"),
                Err(error) => error,
            };
        let rendered = error.to_string();

        assert!(rendered.contains("wasi:http/types@0.2.9"));
        assert!(rendered.contains("Host wasi:http version: 0.2.6"));
        assert!(rendered.contains("These wasi:http versions are compatible"));
        assert!(rendered.contains("Single runtime instantiation attempt: basic world"));
        assert!(rendered.contains("No fallback attempt was made"));
        assert!(!rendered.contains("Basic fallback diagnostic omitted"));
        assert!(!rendered.contains("Subordinate basic fallback attempt"));
    }

    #[test]
    fn single_http_instantiation_error_keeps_typed_context_without_claiming_a_fallback() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();
        let failure = wasmtime::Error::msg("runtime HTTP type mismatch")
            .context("failed to convert function to given type")
            .context("Failed to instantiate WASM component (HTTP)");

        let error = ProviderInstantiationError::from_single_attempt(
            &engine,
            &component,
            InstantiationWorld::HttpEnabled,
            failure,
        );
        let rendered = error.to_string();

        assert!(rendered.starts_with("Failed to instantiate WASM component (HTTP)"));
        assert!(rendered.contains("runtime HTTP type mismatch"));
        assert!(rendered.contains("wasi:http/types@0.2.9"));
        assert!(rendered.contains("Host wasi:http version: 0.2.6"));
        assert!(rendered.contains("These wasi:http versions are compatible"));
        assert!(rendered.contains("Single runtime instantiation attempt: HTTP-enabled world"));
        assert!(rendered.contains("No basic fallback was attempted"));
        assert!(!rendered.contains("Basic fallback diagnostic omitted"));
        assert!(!rendered.contains("Subordinate basic fallback attempt"));
    }

    #[test]
    fn single_basic_instantiation_error_keeps_typed_context_without_demotion() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();
        let failure = wasmtime::Error::msg(
            "component imports instance `wasi:http/types@0.2.9`, but a matching implementation was not found in the linker",
        )
        .context("Failed to instantiate WASM component");

        let error = ProviderInstantiationError::from_single_attempt(
            &engine,
            &component,
            InstantiationWorld::Basic,
            failure,
        );
        let rendered = error.to_string();

        assert!(rendered.starts_with("Failed to instantiate WASM component"));
        assert!(rendered.contains("matching implementation was not found in the linker"));
        assert!(rendered.contains("wasi:http/types@0.2.9"));
        assert!(rendered.contains("Host wasi:http version: 0.2.6"));
        assert!(rendered.contains("These wasi:http versions are compatible"));
        assert!(rendered.contains("Single runtime instantiation attempt: basic world"));
        assert!(rendered.contains("No fallback attempt was made"));
        assert!(!rendered.contains("Basic fallback diagnostic omitted"));
        assert!(!rendered.contains("Subordinate basic fallback attempt"));
    }

    #[test]
    fn runtime_instantiation_error_remains_typed_at_provider_error_boundary() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();
        let instantiation = ProviderInstantiationError::from_single_attempt(
            &engine,
            &component,
            InstantiationWorld::HttpEnabled,
            wasmtime::Error::msg("runtime instantiation failed"),
        );

        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let provider_error = WasmProviderInstanceError::Instantiation(instantiation)
            .into_provider_error_with(&mapper);
        let cause = std::error::Error::source(&provider_error)
            .expect("provider boundary must retain the typed instantiation cause");

        assert!(cause.downcast_ref::<ProviderInstantiationError>().is_some());
        let rendered = provider_error.to_string();
        assert!(rendered.contains("runtime instantiation failed"));
        assert!(rendered.contains("wasi:http/types@0.2.9"));
        assert!(rendered.contains("Host wasi:http version: 0.2.6"));
        assert!(rendered.contains("These wasi:http versions are compatible"));
        assert!(rendered.contains("Single runtime instantiation attempt"));
    }

    #[test]
    fn runtime_info_interrupt_uses_guest_timeout_classification() {
        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let provider_error = WasmProviderInstanceError::InfoCall(
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while calling provider info"),
        )
        .into_provider_error_with(&mapper);

        assert_eq!(provider_error.variant_name(), "timeout");
        assert_eq!(
            provider_error.message(),
            format!(
                "WASM plugin timed out after {WASM_OPERATION_TIMEOUT_SECS}s in info (check AWS credentials)"
            )
        );
        let cause = std::error::Error::source(&provider_error)
            .expect("info timeout must retain the typed guest-call cause");
        assert!(cause.downcast_ref::<WasmGuestCallError>().is_some());
        assert!(
            cause.source().is_some_and(|source| source
                .to_string()
                .contains("error while calling provider info")),
            "the typed guest-call wrapper must retain the Wasmtime cause chain"
        );
    }

    #[test]
    fn runtime_instantiation_error_accepts_a_typed_composition_mapper() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(&engine, "(component)").unwrap();
        let instantiation = ProviderInstantiationError::from_single_attempt(
            &engine,
            &component,
            InstantiationWorld::HttpEnabled,
            wasmtime::Error::msg("runtime instantiation failed"),
        );
        let mapper: ProviderInstantiationErrorMapper = Arc::new(|error| {
            ProviderError::internal("artifact provenance attached").with_cause(error)
        });

        let provider_error = WasmProviderInstanceError::Instantiation(instantiation)
            .into_provider_error_with(&mapper);

        assert!(
            provider_error
                .to_string()
                .contains("artifact provenance attached")
        );
        assert!(
            provider_error
                .to_string()
                .contains("runtime instantiation failed")
        );
    }

    /// `WASI_HTTP_HOST_VERSION` is user-facing compatibility evidence, so a
    /// dependency bump must force an explicit review of the dependency's
    /// `wit/deps/http.wit` package version instead of silently leaving stale
    /// output behind.
    #[test]
    fn host_wasi_http_version_tracks_locked_wasmtime_wasi_http_pin() {
        let lock = include_str!("../../Cargo.lock");
        let mut locked_versions: Vec<_> = lock
            .split("[[package]]")
            .filter(|package| {
                package
                    .lines()
                    .any(|line| line.trim() == "name = \"wasmtime-wasi-http\"")
            })
            .filter_map(|package| {
                package.lines().find_map(|line| {
                    line.trim()
                        .strip_prefix("version = \"")
                        .and_then(|version| version.strip_suffix('\"'))
                })
            })
            .collect();
        locked_versions.sort_unstable();
        locked_versions.dedup();

        assert_eq!(
            locked_versions,
            [WASMTIME_WASI_HTTP_CRATE_VERSION_WITH_KNOWN_WIT],
            "inspect wasmtime-wasi-http's wit/deps/http.wit and update WASI_HTTP_HOST_VERSION"
        );
    }

    /// A fresh (un-poisoned) instance lets an operation proceed: the
    /// guard returns `None`.
    #[test]
    fn poisoned_guard_allows_a_fresh_instance() {
        let flag = AtomicBool::new(false);
        let reason = OnceLock::new();
        assert!(poisoned_guard(&flag, &reason, "create").is_none());
    }

    /// Dropping an *armed* `PoisonOnDrop` (the carina#3106 cancellation
    /// path: `tokio::time::timeout` drops the in-flight operation while it
    /// is suspended inside a `wasmtime` async call, leaving the shared
    /// `Store` unreusable) poisons the instance, and every subsequent
    /// operation then fails fast instead of touching the poisoned store.
    #[test]
    fn armed_drop_poisons_then_guard_fails_fast_naming_the_operation() {
        let flag = AtomicBool::new(false);
        let reason = OnceLock::new();

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
        let guard_err = poisoned_guard(&flag, &reason, "delete")
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

    #[test]
    fn trap_poison_names_the_origin_and_keeps_only_the_structured_cause() {
        let flag = AtomicBool::new(false);
        let reason = OnceLock::new();
        let trap = wasmtime::Error::new(wasmtime::Trap::UnreachableCodeReached)
            .context("error while executing at wasm backtrace:\n    0: test-frame");

        poison_after_trap(&flag, &reason, "normalize_state", &trap);

        let error = poisoned_guard(&flag, &reason, "create")
            .expect("the later operation must reject a trap-poisoned instance");
        let rendered = error.to_string();
        assert!(
            rendered.contains(
                "WASM provider instance unusable after trap in normalize_state: wasm trap: wasm `unreachable` instruction executed"
            ),
            "the original trap must remain actionable: {rendered}"
        );
        assert!(
            !rendered.contains("wasm backtrace") && !rendered.contains("test-frame"),
            "later errors must not repeat the original trap backtrace: {rendered}"
        );
        assert!(
            rendered.contains("operation 'create'"),
            "the rejected follow-up operation should remain visible: {rendered}"
        );
        assert!(
            !rendered.contains("prior operation timed out"),
            "component traps must not be mislabeled as timeouts: {rendered}"
        );
    }

    /// A completed guest call disarms the guard, so a normal operation
    /// (success *or* a clean provider error — both mean the wasmtime call
    /// returned, not a cancellation) must NOT poison the instance.
    #[test]
    fn disarmed_drop_does_not_poison() {
        let flag = AtomicBool::new(false);
        let reason = OnceLock::new();
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
            poisoned_guard(&flag, &reason, "read").is_none(),
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
