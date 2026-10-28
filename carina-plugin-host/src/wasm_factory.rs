//! WasmProviderFactory loads a WASM component and implements ProviderFactory.

mod guest_store;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use indexmap::IndexMap;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use sha2::{Digest, Sha256};

use tokio::sync::Mutex;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::cli::{WasiCli, WasiCliView as _};
use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView as _};
use wasmtime_wasi::random::WasiRandom;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;

use carina_core::effect::PlanOp;
use carina_core::provider::{
    AppliedIdentifier, AppliedState, BoxFuture, CanonicalCustomTypeValue,
    CanonicalProviderConfigAttributes, CreateAttempt, CreateFailure, CreateOutcome, CreateRequest,
    DeleteRequest, Observation, PendingCreate as CorePendingCreate, Poll as CorePoll, Provider,
    ProviderError, ProviderFactory, ProviderNormalizer, ProviderReadyConfig, ProviderResult,
    ReadOutcome, ReadRequest, ResentPendingCreate, ResentPoll as CoreResentPoll,
    Resume as CoreResume, ResumeCreateRequest, ResumeFailure, ResumeOutcome, SavedAttrs, Sent,
    UpdateOutcome, UpdateRequest,
};
#[cfg(test)]
use carina_core::provider::{
    CreateFailureKind, InFlightCreate, InFlightPersistence, ResumeFailureKind,
};
use carina_core::resource::{
    Resource, ResourceId, ResourceIdentityState, State, Value, contains_resource_ref,
    value_contains_unknown,
};
use carina_core::schema::{CompletionValue, ResourceSchema, TypeIdentity};
use carina_core::value::SerializationError;
use carina_core::wait::BindingPattern;
use carina_core::wait::predicate::AttrPath;

use crate::{secret_seal, wasm_convert};
use carina_provider_protocol::types as proto;
#[cfg(any(test, feature = "test-utils"))]
pub use guest_store::GuestCallBudget;
#[cfg(not(any(test, feature = "test-utils")))]
use guest_store::GuestCallBudget;
use guest_store::{EPOCH_TICK_INTERVAL, GuestStore, HostState, WallBudgetExceeded};

/// Lift a `SerializationError` from a synchronous `core_to_wit_*` call into
/// the async `BoxFuture` shape used by `Provider`, preserving its typed cause.
fn early_provider_err<T: 'static>(
    operation: &'static str,
    error: SerializationError,
) -> BoxFuture<'static, ProviderResult<T>> {
    Box::pin(async move { Err(wasm_value_encode_provider_error(operation, error)) })
}

#[derive(Debug)]
struct WasmOperationError<E> {
    operation: &'static str,
    source: E,
}

impl<E: fmt::Display> fmt::Display for WasmOperationError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "in operation {}: {}", self.operation, self.source)
    }
}

impl<E> std::error::Error for WasmOperationError<E> where E: std::error::Error + 'static {}

#[derive(Debug)]
struct InvalidWasmCreateResult {
    detail: &'static str,
    source: ProviderError,
}

impl fmt::Display for InvalidWasmCreateResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail)
    }
}

impl std::error::Error for InvalidWasmCreateResult {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn wasm_value_decode_provider_error<E>(operation: &'static str, error: E) -> ProviderError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ProviderError::internal_caused_by(
        "WASM provider returned an invalid boundary value",
        WasmOperationError {
            operation,
            source: error,
        },
    )
}

fn wasm_value_encode_provider_error<E>(operation: &'static str, error: E) -> ProviderError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ProviderError::internal_caused_by(
        "failed to encode a WASM provider boundary value",
        WasmOperationError {
            operation,
            source: error,
        },
    )
}

#[derive(Debug)]
struct WasmGuestCallError {
    context: Option<WasmGuestCallContext>,
    source: wasmtime::Error,
}

#[derive(Clone, Copy, Debug)]
enum WasmGuestCallContext {
    Trap { operation: &'static str },
    ComputeBudgetExceeded { operation: &'static str },
}

impl fmt::Display for WasmGuestCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.context {
            None => f.write_str("WASM guest call failed"),
            Some(WasmGuestCallContext::Trap { operation }) => {
                write!(f, "{operation} call failed")
            }
            Some(WasmGuestCallContext::ComputeBudgetExceeded { operation }) => {
                write!(f, "{operation} exceeded the guest compute budget")
            }
        }?;
        f.write_str(": ")?;
        write_error_chain_inline(f, &self.source)
    }
}

impl std::error::Error for WasmGuestCallError {}

fn wasm_trap_provider_error(operation: &'static str, error: wasmtime::Error) -> ProviderError {
    let timed_out = error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt);
    if timed_out {
        ProviderError::timeout_caused_by(
            "WASM plugin timed out",
            WasmGuestCallError {
                context: Some(WasmGuestCallContext::ComputeBudgetExceeded { operation }),
                source: error,
            },
        )
    } else {
        ProviderError::internal_caused_by(
            "WASM guest trapped",
            WasmGuestCallError {
                context: Some(WasmGuestCallContext::Trap { operation }),
                source: error,
            },
        )
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
                write!(f, "{context}: {source}")
            }
            WasmProviderLoadError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for WasmProviderLoadError {}

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
/// Instantiation, `info` guest-call, and decoded initialization failures remain
/// typed until the `ProviderFactory` trait boundary.
#[derive(Debug)]
enum WasmProviderInstanceError {
    Instantiation(ProviderInstantiationError),
    InfoCall(wasmtime::Error),
    WallBudgetExceeded {
        operation: &'static str,
        source: WallBudgetExceeded,
    },
    InitializeValueEncode(SerializationError),
    InitializeCall(wasmtime::Error),
    Initialize(ProviderError),
    ProtocolVersion(String),
}

type ProviderInstantiationErrorMapper =
    Arc<dyn Fn(ProviderInstantiationError) -> ProviderError + Send + Sync>;

fn default_provider_instantiation_error_mapper(error: ProviderInstantiationError) -> ProviderError {
    ProviderError::internal_caused_by(
        "Failed to instantiate WASM provider runtime instance",
        error,
    )
}

impl WasmProviderInstanceError {
    fn into_provider_error_with(self, mapper: &ProviderInstantiationErrorMapper) -> ProviderError {
        match self {
            WasmProviderInstanceError::Instantiation(error) => mapper(error),
            WasmProviderInstanceError::InfoCall(error) => wasm_trap_provider_error("info", error),
            WasmProviderInstanceError::WallBudgetExceeded { operation, source } => {
                wall_budget_provider_error(operation, source)
            }
            WasmProviderInstanceError::InitializeValueEncode(error) => {
                wasm_value_encode_provider_error("initialize", error)
            }
            WasmProviderInstanceError::InitializeCall(error) => {
                wasm_trap_provider_error("initialize", error)
            }
            WasmProviderInstanceError::Initialize(error) => error,
            WasmProviderInstanceError::ProtocolVersion(message) => ProviderError::internal(message),
        }
    }
}

/// Exercise the exact guest-string initialization boundary for downstream
/// rendering tests.
///
/// The constructed WIT value matches the SDK's
/// `validate_string_to_provider_error`, then follows the live host path:
/// WIT → protocol → core → `WasmProviderInstanceError::Initialize` → public
/// `ProviderError`. Provider identity is intentionally left for the wiring
/// layer to attach.
#[cfg(any(test, feature = "test-utils"))]
pub fn decode_initialize_string_error_for_test(message: impl Into<String>) -> ProviderError {
    let wit_error = wit_types::ProviderError::InvalidInput(wit_types::ErrorDetail {
        message: message.into(),
        resource_id: None,
        cause: None,
        provider_name: None,
        operation: None,
        status: None,
        code: None,
        request_id: None,
    });
    let decoded = wasm_convert::decode_provider_initialization_error(
        wasm_convert::wit_to_wire_provider_error(wit_error),
    );
    let mapper: ProviderInstantiationErrorMapper =
        Arc::new(default_provider_instantiation_error_mapper);
    WasmProviderInstanceError::Initialize(decoded).into_provider_error_with(&mapper)
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

fn write_error_chain_inline(f: &mut fmt::Formatter<'_>, error: &wasmtime::Error) -> fmt::Result {
    for (index, cause) in error.chain().enumerate() {
        if index > 0 {
            f.write_str(": ")?;
        }
        write!(f, "{cause}")?;
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

/// Maximum duration applied independently to each capped phase of an
/// outbound guest HTTP request (connect, first byte, and between bytes).
const HTTP_API_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
                std::thread::sleep(EPOCH_TICK_INTERVAL);
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
        // Metadata gets 1s; all other requests get HTTP_API_REQUEST_TIMEOUT.
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
        .map_err(|_| ErrorCode::HttpRequestUriInvalid)?;

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

// -- Helper: create a new Store + CarinaProvider instance --

enum WasmBindingCreationError {
    Instantiation(wasmtime::Error),
    InfoCall(wasmtime::Error),
    WallBudgetExceeded {
        operation: &'static str,
        source: WallBudgetExceeded,
    },
    ProtocolVersion(String),
}

impl WasmBindingCreationError {
    fn instantiation_context(self, context: &'static str) -> Self {
        match self {
            Self::Instantiation(error) => Self::Instantiation(error.context(context)),
            Self::InfoCall(error) => Self::InfoCall(error),
            Self::WallBudgetExceeded { operation, source } => {
                Self::WallBudgetExceeded { operation, source }
            }
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
            store: &mut GuestStore,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let pre = linker
                .instantiate_pre(component)
                .map_err(WasmBindingCreationError::Instantiation)?;
            let instance = store
                .call(|store| pre.instantiate_async(store))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "instantiate",
                    source,
                })?
                .map_err(WasmBindingCreationError::Instantiation)?;
            let info = store
                .call(|store| std::future::ready(untyped_info_func(store, &instance)))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "info",
                    source,
                })?
                .map_err(WasmBindingCreationError::Instantiation)?;
            let (info_json,) = store
                .call(|store| info.call_async(store, ()))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "info",
                    source,
                })?
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
            store: &mut GuestStore,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let checked = ProtocolCheckedInstance::instantiate(store, component, linker).await?;
            let bindings = store
                .call(|store| std::future::ready(CarinaProvider::new(store, &checked.instance)))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "instantiate",
                    source,
                })?
                .map_err(WasmBindingCreationError::Instantiation)?;
            Ok(Self {
                raw: RawWasmBindings::Basic(bindings),
                info_json: checked.info_json,
            })
        }

        pub(super) async fn instantiate_http(
            store: &mut GuestStore,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let checked = ProtocolCheckedInstance::instantiate(store, component, linker).await?;
            let bindings = store
                .call(|store| {
                    std::future::ready(CarinaProviderWithHttp::new(store, &checked.instance))
                })
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "instantiate",
                    source,
                })?
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
            identifier: &str,
            request: wit_types::ReadRequest,
        ) -> wasmtime::Result<Result<wit_types::ReadOutcome, wit_types::ProviderError>> {
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
        ) -> wasmtime::Result<Result<wit_types::CreateStep, wit_types::CreateVerdict>> {
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

        pub(super) async fn call_poll_create(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            request: &wit_types::CreateRequest,
            pending: &wit_types::PendingCreate,
        ) -> wasmtime::Result<Result<wit_types::CreateStep, wit_types::PollVerdict>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_poll_create(store, id, request, pending)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_poll_create(store, id, request, pending)
                        .await
                }
            }
        }

        pub(super) async fn call_resume_create(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            request: &wit_types::CreateRequest,
            pending: &wit_types::ResumePending,
        ) -> wasmtime::Result<Result<wit_types::ResumeStep, wit_types::ResumeVerdict>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_resume_create(store, id, request, pending)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_resume_create(store, id, request, pending)
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

        pub(super) async fn call_poll_update(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: &str,
            request: &wit_types::UpdateRequest,
            pending: &wit_types::PendingOperation,
        ) -> wasmtime::Result<Result<wit_types::UpdateOutcome, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_poll_update(store, id, identifier, request, pending)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_poll_update(store, id, identifier, request, pending)
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
        ) -> wasmtime::Result<Result<wit_types::DeleteOutcome, wit_types::ProviderError>> {
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

        pub(super) async fn call_poll_delete(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: &str,
            request: wit_types::DeleteRequest,
            pending: &wit_types::PendingOperation,
        ) -> wasmtime::Result<Result<wit_types::DeleteOutcome, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_poll_delete(store, id, identifier, request, pending)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_poll_delete(store, id, identifier, request, pending)
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
                .send(|resources| async move {
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
            states: &[(String, wit_types::PersistedState)],
        ) -> wasmtime::Result<
            Result<Vec<(String, wit_types::PersistedState)>, wit_types::ProviderError>,
        > {
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
            states: &[(String, wit_types::PersistedState)],
            saved_attrs: &[(String, Vec<(String, wit_types::PersistedValue)>)],
        ) -> wasmtime::Result<
            Result<Vec<(String, wit_types::PersistedState)>, wit_types::ProviderError>,
        > {
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
    budget: GuestCallBudget,
) -> Result<(GuestStore, WasmBindings), WasmBindingCreationError> {
    let wasi_ctx = build_sandboxed_wasi_ctx(provider_kind);
    let mut store = GuestStore::new(engine, wasi_ctx, None, build_store_limits(), budget);

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
    budget: GuestCallBudget,
) -> Result<(GuestStore, WasmBindings), WasmBindingCreationError> {
    let wasi_ctx = build_sandboxed_wasi_ctx(provider_kind);
    let mut store = GuestStore::new(
        engine,
        wasi_ctx,
        Some(WasiHttpCtx::new()),
        build_store_limits(),
        budget,
    );

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
    budget: GuestCallBudget,
) -> Result<(GuestStore, WasmBindings), WasmProviderInstanceError> {
    let result = match world {
        InstantiationWorld::HttpEnabled => {
            create_instance_with_http(engine, component, provider_kind, budget).await
        }
        InstantiationWorld::Basic => {
            create_instance(engine, component, provider_kind, budget).await
        }
    };
    result.map_err(|failure| match failure {
        WasmBindingCreationError::Instantiation(failure) => {
            WasmProviderInstanceError::Instantiation(
                ProviderInstantiationError::from_single_attempt(engine, component, world, failure),
            )
        }
        WasmBindingCreationError::InfoCall(error) => WasmProviderInstanceError::InfoCall(error),
        WasmBindingCreationError::WallBudgetExceeded { operation, source } => {
            WasmProviderInstanceError::WallBudgetExceeded { operation, source }
        }
        WasmBindingCreationError::ProtocolVersion(message) => {
            WasmProviderInstanceError::ProtocolVersion(message)
        }
    })
}

/// Output of `create_instance_auto`: the instantiated store, the
/// bindings, and whether the HTTP-enabled world was used.
type CreateInstanceResult = Result<(GuestStore, WasmBindings, bool), WasmProviderLoadError>;

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
    budget: GuestCallBudget,
) -> BoxFuture<'a, CreateInstanceResult> {
    Box::pin(async move {
        match create_instance_with_http(engine, component, provider_kind, budget).await {
            Ok((store, bindings)) => Ok((store, bindings, true)),
            Err(WasmBindingCreationError::InfoCall(error)) => Err(WasmProviderLoadError::metadata(
                "Failed to call info()",
                WasmGuestCallError {
                    context: None,
                    source: error,
                },
            )),
            Err(WasmBindingCreationError::WallBudgetExceeded { operation, source }) => {
                Err(wall_budget_load_error(operation, source))
            }
            Err(WasmBindingCreationError::ProtocolVersion(message)) => Err(message.into()),
            Err(WasmBindingCreationError::Instantiation(http_err)) => {
                match create_instance(engine, component, provider_kind, budget).await {
                    Ok((store, bindings)) => Ok((store, bindings, false)),
                    Err(WasmBindingCreationError::InfoCall(error)) => {
                        Err(WasmProviderLoadError::metadata(
                            "Failed to call info()",
                            WasmGuestCallError {
                                context: None,
                                source: error,
                            },
                        ))
                    }
                    Err(WasmBindingCreationError::WallBudgetExceeded { operation, source }) => {
                        Err(wall_budget_load_error(operation, source))
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

// -- Shared WASM instance state --

/// Why a component instance can no longer be entered safely.
///
/// The originating operation and typed Wasmtime trap classification are kept
/// separately from the rejected follow-up operation, so fail-fast diagnostics
/// never misattribute an earlier failure to a later guest call.
#[derive(Clone, Debug)]
enum PoisonReason {
    /// A guest future was cancelled before Wasmtime returned a result.
    Cancelled,
    /// The host observed the wall-clock deadline while the guest was in flight.
    WallClockTimeout { operation: &'static str },
    /// Wasmtime returned a component trap.
    Trap {
        operation: &'static str,
        trap: Option<wasmtime::Trap>,
        root_cause: Box<str>,
    },
}

impl PoisonReason {
    fn from_trap(operation: &'static str, error: &wasmtime::Error) -> Self {
        let root_cause = error.root_cause().to_string();
        let root_cause = root_cause
            .lines()
            .next()
            .unwrap_or("unknown WASM trap")
            .into();
        Self::Trap {
            operation,
            trap: error.downcast_ref::<wasmtime::Trap>().copied(),
            root_cause,
        }
    }

    fn fail_fast_error(&self, operation: &'static str) -> ProviderError {
        ProviderError::internal(format!(
            "{self} (operation '{operation}'); re-run the command"
        ))
    }
}

impl fmt::Display for PoisonReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str(
                "WASM provider is unusable: a prior operation was cancelled and left the plugin \
                 instance in an undefined state",
            ),
            Self::WallClockTimeout { operation } => write!(
                f,
                "WASM provider instance unusable after wall-clock timeout in {operation}"
            ),
            Self::Trap {
                operation,
                trap,
                root_cause,
            } => {
                write!(
                    f,
                    "WASM provider instance unusable after trap in {operation}: "
                )?;
                match trap {
                    Some(trap) if trap.to_string() == root_cause.as_ref() => trap.fmt(f),
                    Some(trap) => write!(f, "{trap}: {root_cause}"),
                    None => f.write_str(root_cause),
                }
            }
        }
    }
}

/// A callable component instance. Store and bindings live and die together,
/// so a poisoned [`InstanceSlot`] has neither object available for a guest call.
struct LiveInstance {
    store: GuestStore,
    bindings: WasmBindings,
}

impl LiveInstance {
    fn new(store: GuestStore, bindings: WasmBindings) -> Self {
        Self { store, bindings }
    }
}

/// The complete health state protected by each instance mutex.
///
/// There is deliberately no representation containing a poisoned store or
/// bindings: poisoning replaces `Live` wholesale and drops both while the
/// mutex remains held.
enum InstanceSlot {
    Live(Box<LiveInstance>),
    Poisoned { reason: PoisonReason },
}

/// Exclusive access to a [`LiveInstance`]. This guard can only be constructed
/// after matching [`InstanceSlot::Live`].
///
/// The one-call shape is enforced by ownership: [`LiveInstanceGuard::call`]
/// consumes this armed guard, classifies the Wasmtime result, and releases the
/// lock before returning the guest value. A well-typed caller therefore cannot
/// start a second guest call, forget to classify a returned trap, or retain a
/// guard while decoding the guest value.
///
/// Cancellation replaces the slot with `Poisoned` from `Drop` before the mutex
/// guard is released. An observed timeout or returned trap replaces the slot
/// immediately. This preserves the carina#3106 guarantee that a queued sibling
/// can never acquire and enter the dead store.
struct LiveInstanceGuard<'a> {
    slot: Option<tokio::sync::MutexGuard<'a, InstanceSlot>>,
}

impl<'a> LiveInstanceGuard<'a> {
    fn from_slot(
        slot: tokio::sync::MutexGuard<'a, InstanceSlot>,
        operation: &'static str,
    ) -> ProviderResult<Self> {
        match &*slot {
            InstanceSlot::Live(_) => {}
            InstanceSlot::Poisoned { reason } => {
                return Err(reason.fail_fast_error(operation));
            }
        }
        Ok(Self { slot: Some(slot) })
    }

    /// Replace the live slot while its mutex is held, dropping the unusable
    /// store and bindings before any queued operation can acquire the lock.
    fn poison(mut self, reason: PoisonReason) {
        let mut slot = self
            .slot
            .take()
            .expect("an armed LiveInstanceGuard always owns its slot");
        *slot = InstanceSlot::Poisoned { reason };
    }

    /// Run one guest call under the live store's wall-clock budget.
    async fn call<U, F>(mut self, operation: &'static str, f: F) -> ProviderResult<U>
    where
        U: 'static,
        F: AsyncFnOnce(&WasmBindings, &mut Store<HostState>) -> wasmtime::Result<U>,
    {
        let slot = self
            .slot
            .as_mut()
            .expect("an armed LiveInstanceGuard always owns its slot");
        let InstanceSlot::Live(live) = &mut **slot else {
            unreachable!("LiveInstanceGuard can only hold a live slot");
        };
        let (store, bindings) = (&mut live.store, &live.bindings);
        let call = match store.call(|store| f(bindings, store)).await {
            Ok(call) => call,
            Err(error) => {
                self.poison(PoisonReason::WallClockTimeout { operation });
                return Err(wall_budget_provider_error(operation, error));
            }
        };
        match call {
            Ok(value) => {
                drop(
                    self.slot
                        .take()
                        .expect("a successful guest call must still own its slot"),
                );
                Ok(value)
            }
            Err(error) => {
                let reason = PoisonReason::from_trap(operation, &error);
                self.poison(reason);
                Err(wasm_trap_provider_error(operation, error))
            }
        }
    }
}

impl Drop for LiveInstanceGuard<'_> {
    fn drop(&mut self) {
        if let Some(slot) = &mut self.slot {
            **slot = InstanceSlot::Poisoned {
                reason: PoisonReason::Cancelled,
            };
        }
    }
}

/// Shared configured provider/normalizer instance with fail-fast poisoning.
///
/// A trap, timeout, or cancellation permanently changes its slot to
/// [`InstanceSlot::Poisoned`]. Later plan/apply operations receive the original
/// reason and never continue on a freshly configured instance (carina#3106).
struct FailFastInstance {
    slot: Mutex<InstanceSlot>,
}

impl FailFastInstance {
    fn new(store: GuestStore, bindings: WasmBindings) -> Self {
        Self {
            slot: Mutex::new(InstanceSlot::Live(Box::new(LiveInstance::new(
                store, bindings,
            )))),
        }
    }

    async fn acquire(&self, operation: &'static str) -> ProviderResult<LiveInstanceGuard<'_>> {
        LiveInstanceGuard::from_slot(self.slot.lock().await, operation)
    }
}

struct RebuildContext {
    engine: Engine,
    component: Component,
    budget: GuestCallBudget,
}

/// Factory-validation instance that rebuilds after poisoning.
///
/// Acquisition holds the slot mutex while recreating the instance from the
/// already verified component and the same engine and call budget. A failed
/// rebuild leaves the slot poisoned, preserves the typed load error, and is
/// retried by the next validation call.
struct RebuildableInstance {
    slot: Mutex<InstanceSlot>,
    rebuild: RebuildContext,
}

impl RebuildableInstance {
    fn new(
        store: GuestStore,
        bindings: WasmBindings,
        engine: Engine,
        component: Component,
        budget: GuestCallBudget,
    ) -> Self {
        Self {
            slot: Mutex::new(InstanceSlot::Live(Box::new(LiveInstance::new(
                store, bindings,
            )))),
            rebuild: RebuildContext {
                engine,
                component,
                budget,
            },
        }
    }

    async fn acquire(&self, operation: &'static str) -> ProviderResult<LiveInstanceGuard<'_>> {
        let mut slot = self.slot.lock().await;
        let poison_reason = match &*slot {
            InstanceSlot::Live(_) => None,
            InstanceSlot::Poisoned { reason } => Some(reason.clone()),
        };
        if let Some(reason) = poison_reason {
            let (store, bindings, _enable_http) = create_instance_auto(
                &self.rebuild.engine,
                &self.rebuild.component,
                None,
                self.rebuild.budget,
            )
            .await
            .map_err(|error| {
                ProviderError::internal_caused_by(
                    "Failed to rebuild WASM provider validation instance",
                    error,
                )
            })?;
            *slot = InstanceSlot::Live(Box::new(LiveInstance::new(store, bindings)));
            tracing::warn!(
                reason = %reason,
                "rebuilt poisoned WASM provider validation instance"
            );
        }
        LiveInstanceGuard::from_slot(slot, operation)
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn set_budget(&mut self, budget: GuestCallBudget) {
        self.rebuild.budget = budget;
        match self.slot.get_mut() {
            InstanceSlot::Live(live) => live.store.set_budget(budget),
            InstanceSlot::Poisoned { reason: _ } => {}
        }
    }
}

// -- WasmProviderFactory --

enum CachedComponentAcquisition {
    Hit(Component),
    Miss,
}

type ProviderConfigCompletions = HashMap<String, Vec<CompletionValue>>;

pub struct WasmProviderFactory {
    engine: Engine,
    component: Component,
    budget: GuestCallBudget,
    name: String,
    display_name: String,
    version: String,
    schemas: Arc<Vec<ResourceSchema>>,
    cached_config_completions: ProviderConfigCompletions,
    cached_identity_attributes: Vec<String>,
    /// Provider config attribute types (e.g., `region` → `AttributeType::Enum`).
    /// Used by `ProviderFactory::provider_config_attribute_types()` so the host
    /// validates provider attributes against these types using its own carina-core,
    /// catching format bugs without requiring a provider rebuild.
    cached_provider_config_types: HashMap<String, carina_core::schema::AttributeType>,
    enable_http: bool,
    /// Reusable, rebuildable WASM instance for editor-facing validation.
    /// A poisoned slot is recreated from `engine` + `component` before the
    /// next `validate_config()` or `validate_custom_type()` call.
    init_instance: RebuildableInstance,
    /// Lazily created shared instances for provider + normalizer, keyed
    /// by binding name. `None` is the kind's default instance;
    /// `Some(name)` is a named instance (`let <name> = provider <kind>
    /// { ... }`). The first call for a given key creates the instance;
    /// subsequent calls reuse it via `Arc`. Keeping a per-binding entry
    /// is what makes carina#2191 routing work end-to-end — collapsing
    /// every binding onto a single shared instance would pin each kind
    /// to the first instance's attributes (e.g. region) regardless of
    /// what later instances configure.
    shared_instances: Mutex<HashMap<Option<String>, Arc<FailFastInstance>>>,
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
        write_error_chain(f, &self.source)
    }
}

impl std::error::Error for WasmMetadataCallError {}

#[derive(Debug)]
struct WasmWallBudgetError {
    operation: &'static str,
    source: WallBudgetExceeded,
}

impl fmt::Display for WasmWallBudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WASM guest call '{}' timed out", self.operation)
    }
}

impl std::error::Error for WasmWallBudgetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn wall_budget_provider_error(
    operation: &'static str,
    source: WallBudgetExceeded,
) -> ProviderError {
    ProviderError::timeout_caused_by(
        "WASM plugin timed out",
        WasmWallBudgetError { operation, source },
    )
}

fn wall_budget_load_error(
    operation: &'static str,
    source: WallBudgetExceeded,
) -> WasmProviderLoadError {
    WasmProviderLoadError::metadata(
        "WASM provider load timed out",
        WasmWallBudgetError { operation, source },
    )
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
        store: &mut GuestStore,
        provider_name: &str,
        provider_version: &str,
    ) -> Result<
        (
            ProviderConfigCompletions,
            Vec<String>,
            HashMap<String, carina_core::schema::AttributeType>,
        ),
        WasmProviderLoadError,
    > {
        let config_completions_json = store
            .call(|store| bindings.call_provider_config_completions(store))
            .await
            .map_err(|source| wall_budget_load_error("provider-config-completions", source))?
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

        let identity_attributes = store
            .call(|store| bindings.call_identity_attributes(store))
            .await
            .map_err(|source| wall_budget_load_error("identity-attributes", source))?
            .map_err(|source| {
                metadata_call_error(
                    provider_name,
                    provider_version,
                    "identity-attributes",
                    source,
                )
            })?;

        let provider_config_types_json = store
            .call(|store| bindings.call_provider_config_attribute_types(store))
            .await
            .map_err(|source| wall_budget_load_error("provider-config-attribute-types", source))?
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
            provider_config_types,
        ))
    }

    /// Compute a cache-safe filename for the exact provider bytes.
    fn cache_key(wasm_bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(wasm_bytes);
        // Include the crate version so cache is invalidated on host upgrades.
        hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        format!("provider-{}.cwasm", &hash[..16])
    }

    fn create_engine() -> Result<Engine, WasmProviderLoadError> {
        let config = build_engine_config();
        Engine::new(&config).map_err(|e| format!("Failed to create WASM engine: {e}").into())
    }

    fn load_component_from_bytes(
        engine: &Engine,
        wasm_bytes: &[u8],
    ) -> Result<Component, WasmProviderLoadError> {
        Component::new(engine, wasm_bytes)
            .map_err(|e| format!("Failed to load WASM component: {e}").into())
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
        epoch_ticker: EpochTicker,
    ) -> Result<Self, WasmProviderLoadError> {
        let budget = GuestCallBudget::PRODUCTION;
        let (mut store, bindings, enable_http) =
            create_instance_auto(&engine, &component, None, budget).await?;
        let info_json = bindings.info_json().to_owned();

        let schemas_json = store
            .call(|store| bindings.call_schemas(store))
            .await
            .map_err(|source| wall_budget_load_error("schemas", source))?
            .map_err(|e| format!("Failed to call schemas(): {e}"))?;

        let (name, display_name, version) = wasm_convert::json_to_provider_info(&info_json)
            .map_err(|source| {
                WasmProviderLoadError::metadata("Failed to decode provider info", source)
            })?;
        let schemas = wasm_convert::json_to_schemas(&schemas_json)
            .map_err(|e| provider_schema_decode_error(&name, &version, e))?;

        let (cached_config_completions, cached_identity_attributes, cached_provider_config_types) =
            Self::load_metadata(&bindings, &mut store, &name, &version).await?;
        let init_instance =
            RebuildableInstance::new(store, bindings, engine.clone(), component.clone(), budget);

        Ok(Self {
            engine,
            component,
            budget,
            name,
            display_name,
            version,
            schemas: Arc::new(schemas),
            cached_config_completions,
            cached_identity_attributes,
            cached_provider_config_types,
            enable_http,
            init_instance,
            shared_instances: Mutex::new(HashMap::new()),
            instantiation_error_mapper: Arc::new(default_provider_instantiation_error_mapper),
            _epoch_ticker: epoch_ticker,
        })
    }

    /// Load provider bytes using the default precompile cache at `~/.carina/cache/`.
    ///
    /// If the cache directory cannot be created, falls back to compiling without caching.
    pub async fn from_bytes(wasm_bytes: &[u8]) -> Result<Self, WasmProviderLoadError> {
        let load: BoxFuture<'_, Result<Self, WasmProviderLoadError>> = Box::pin(async move {
            match Self::default_cache_dir() {
                Some(cache_dir) => Self::from_bytes_with_cache_dir(wasm_bytes, &cache_dir).await,
                None => Self::new_uncached(wasm_bytes).await,
            }
        });
        load.await
    }

    /// Load provider bytes with an explicit precompile cache directory.
    pub async fn from_bytes_with_cache_dir(
        wasm_bytes: &[u8],
        cache_dir: &Path,
    ) -> Result<Self, WasmProviderLoadError> {
        let cwasm_name = Self::cache_key(wasm_bytes);
        let cwasm_path = cache_dir.join(&cwasm_name);

        // A cold-cache write can fail because the cache directory is
        // unavailable. Resolve that fallback before creating the runtime so
        // `new_uncached` remains the only runtime owner on this path.
        if !cwasm_path.exists()
            && let Err(error) = Self::precompile(wasm_bytes, &cwasm_path)
        {
            eprintln!("Failed to write precompile cache, loading directly: {error}");
            return Self::new_uncached(wasm_bytes).await;
        }

        let engine = Self::create_engine()?;

        // On resolver-backed loads, verified provider bytes select the
        // `.cwasm` by content hash. The `.cwasm` itself is native code
        // deserialized unsafely and trusted as user-owned cache state under
        // the user's home; it is not verified against the provider lock.
        let component = match Self::acquire_cached_component(&engine, &cwasm_path).await {
            CachedComponentAcquisition::Hit(component) => component,
            CachedComponentAcquisition::Miss => {
                // Try to precompile and cache.
                match Self::precompile(wasm_bytes, &cwasm_path) {
                    Ok(()) => Self::deserialize_precompiled_component(&engine, &cwasm_path)
                        .map_err(WasmProviderLoadError::PrecompiledDeserialization)?,
                    Err(error) => {
                        eprintln!("Failed to write precompile cache, loading directly: {error}");
                        // Cache inspection already required this engine. Reuse
                        // it so a stale-cache recovery failure cannot create a
                        // second engine or orphan an epoch ticker.
                        Self::load_component_from_bytes(&engine, wasm_bytes)?
                    }
                }
            }
        };

        let epoch_ticker = EpochTicker::start(engine.clone());
        Self::bring_up_component(engine, component, epoch_ticker).await
    }

    /// Load provider bytes without any precompile caching.
    async fn new_uncached(wasm_bytes: &[u8]) -> Result<Self, WasmProviderLoadError> {
        let engine = Self::create_engine()?;
        let component = Self::load_component_from_bytes(&engine, wasm_bytes)?;
        let epoch_ticker = EpochTicker::start(engine.clone());

        Self::bring_up_component(engine, component, epoch_ticker).await
    }

    /// Precompile provider bytes and save the result to a .cwasm file.
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
    pub fn precompile(wasm_bytes: &[u8], cwasm_path: &Path) -> Result<(), String> {
        let config = build_engine_config();
        let engine = Engine::new(&config).map_err(|e| format!("Engine error: {e}"))?;
        let serialized = engine
            .precompile_component(wasm_bytes)
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

        Self::bring_up_component(engine, component, epoch_ticker).await
    }

    async fn create_initialized_instance(
        &self,
        attributes: &IndexMap<String, Value>,
    ) -> Result<(GuestStore, WasmBindings), WasmProviderInstanceError> {
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
            create_runtime_instance(&self.engine, &self.component, kind, world, self.budget)
                .await?;
        let wit_attrs = wasm_convert::core_to_wit_value_map(attributes)
            .map_err(WasmProviderInstanceError::InitializeValueEncode)?;
        store
            .call(|store| bindings.call_initialize(store, &wit_attrs))
            .await
            .map_err(|source| WasmProviderInstanceError::WallBudgetExceeded {
                operation: "initialize",
                source,
            })?
            .map_err(WasmProviderInstanceError::InitializeCall)?
            .map_err(|e| {
                WasmProviderInstanceError::Initialize(
                    wasm_convert::decode_provider_initialization_error(
                        wasm_convert::wit_to_wire_provider_error(e),
                    ),
                )
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
    ) -> Result<Arc<FailFastInstance>, WasmProviderInstanceError> {
        let key: Option<String> = binding.map(|s| s.to_string());
        let mut guard = self.shared_instances.lock().await;
        if let Some(instance) = guard.get(&key) {
            return Ok(Arc::clone(instance));
        }
        let (store, bindings) = self.create_initialized_instance(attributes).await?;
        let instance = Arc::new(FailFastInstance::new(store, bindings));
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

    /// Override the per-guest-call compute and wall-clock budgets.
    ///
    /// Production callers use the default. Integration tests inject short
    /// budgets to prove the compute and wall-clock semantics.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_guest_call_budget(mut self, budget: GuestCallBudget) -> Self {
        self.budget = budget;
        self.init_instance.set_budget(budget);
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

    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
        let wit_attrs = wasm_convert::core_to_wit_value_map(attributes.attributes())
            .map_err(|e| e.to_string())?;

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async {
                    let instance = &self.init_instance;
                    let locked = instance.acquire("validate_config").await?;
                    let result = locked
                        .call("validate_config", async |bindings, store| {
                            bindings.call_validate_config(store, &wit_attrs).await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
                })
                .map_err(|error: ProviderError| error.to_string())
        })
    }

    fn validate_custom_type(
        &self,
        identity: &TypeIdentity,
        value: &CanonicalCustomTypeValue,
    ) -> Result<(), String> {
        let wit_identity = wasm_convert::core_type_identity_to_wit(identity);
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async {
                    let instance = &self.init_instance;
                    let locked = instance.acquire("validate_custom_type").await?;
                    let result = locked
                        .call("validate_custom_type", async |bindings, store| {
                            bindings
                                .call_validate_custom_type(store, &wit_identity, value.as_str())
                                .await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
                })
                .map_err(|error: ProviderError| error.to_string())
        })
    }

    fn extract_region(&self, attributes: &IndexMap<String, Value>) -> String {
        // Provider-ready config enums are already canonical host values;
        // strings remain literal and are never shape-decoded here.
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
                schemas: Arc::clone(&self.schemas),
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
        self.schemas.as_ref().clone()
    }
}

// -- WasmProvider --

pub struct WasmProvider {
    instance: Arc<FailFastInstance>,
    name: String,
    schemas: Arc<Vec<ResourceSchema>>,
}

struct PendingCreate {
    id: ResourceId,
    wit_id: wit_types::ResourceId,
    wit_request: wit_types::CreateRequest,
    pending: CorePendingCreate,
}

enum PendingResume {
    Resume {
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        resume: CoreResume,
        poll_after_secs: u32,
    },
    Resent(PendingResentCreate),
}

struct PendingResentCreate {
    id: ResourceId,
    wit_id: wit_types::ResourceId,
    wit_request: wit_types::CreateRequest,
    pending: ResentPendingCreate,
}

impl PendingResume {
    fn poll_after_secs(&self) -> u32 {
        match self {
            Self::Resume {
                id: _id,
                wit_id: _wit_id,
                wit_request: _wit_request,
                resume: _resume,
                poll_after_secs,
            } => *poll_after_secs,
            Self::Resent(pending) => pending.pending.poll_after_secs(),
        }
    }

    fn into_failure(self, error: ProviderError) -> ResumeFailure {
        match self {
            Self::Resume {
                id: _id,
                wit_id: _wit_id,
                wit_request: _wit_request,
                resume,
                poll_after_secs: _poll_after_secs,
            } => resume.preserve(error),
            Self::Resent(pending) => pending.pending.preserve(error),
        }
    }
}

#[derive(Clone)]
struct PendingUpdate {
    id: ResourceId,
    wit_id: wit_types::ResourceId,
    identifier: String,
    wit_request: wit_types::UpdateRequest,
    operation: wasm_convert::UpdatePendingOperation,
}

#[derive(Clone)]
struct PendingDelete {
    id: ResourceId,
    wit_id: wit_types::ResourceId,
    identifier: String,
    wit_request: wit_types::DeleteRequest,
    operation: wasm_convert::DeletePendingOperation,
}

enum UpdateProgress {
    Done(Box<UpdateOutcome>),
    Pending(Box<PendingUpdate>),
}

enum CreateProgress {
    Done(Box<CreateOutcome>),
    Verdict(Box<CreateFailure>),
    Pending(Box<PendingCreate>),
}

enum ResumeProgress {
    Done(ResumeOutcome),
    Verdict(Box<ResumeFailure>),
    Pending(Box<PendingResume>),
}

impl OperationProgress for ResumeProgress {
    type Output = ResumeOutcome;
    type Verdict = Box<ResumeFailure>;
    type PendingState = Box<PendingResume>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<Self::Output, Box<ResumeFailure>, Self::PendingState>
    {
        match self {
            Self::Done(outcome) => carina_provider_protocol::OperationStep::Done(outcome),
            Self::Verdict(verdict) => carina_provider_protocol::OperationStep::Verdict(verdict),
            Self::Pending(pending) => {
                let poll_after_secs = pending.poll_after_secs();
                carina_provider_protocol::OperationStep::Pending {
                    pending,
                    poll_after_secs,
                }
            }
        }
    }
}

impl OperationProgress for CreateProgress {
    type Output = CreateOutcome;
    type Verdict = Box<CreateFailure>;
    type PendingState = Box<PendingCreate>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<Self::Output, Box<CreateFailure>, Self::PendingState>
    {
        match self {
            Self::Done(outcome) => carina_provider_protocol::OperationStep::Done(*outcome),
            Self::Verdict(verdict) => carina_provider_protocol::OperationStep::Verdict(verdict),
            Self::Pending(pending) => carina_provider_protocol::OperationStep::Pending {
                poll_after_secs: pending.pending.poll_after_secs(),
                pending,
            },
        }
    }
}

enum DeleteProgress {
    Deleted,
    Pending(Box<PendingDelete>),
}

trait OperationProgress {
    type Output;
    type Verdict;
    type PendingState;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<Self::Output, Self::Verdict, Self::PendingState>;
}

impl OperationProgress for UpdateProgress {
    type Output = UpdateOutcome;
    type Verdict = std::convert::Infallible;
    type PendingState = Box<PendingUpdate>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<
        Self::Output,
        std::convert::Infallible,
        Self::PendingState,
    > {
        match self {
            Self::Done(outcome) => carina_provider_protocol::OperationStep::Done(*outcome),
            Self::Pending(pending) => carina_provider_protocol::OperationStep::Pending {
                poll_after_secs: pending.operation.poll_after_secs(),
                pending,
            },
        }
    }
}

impl OperationProgress for DeleteProgress {
    type Output = ();
    type Verdict = std::convert::Infallible;
    type PendingState = Box<PendingDelete>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<
        Self::Output,
        std::convert::Infallible,
        Self::PendingState,
    > {
        match self {
            Self::Deleted => carina_provider_protocol::OperationStep::Done(()),
            Self::Pending(pending) => carina_provider_protocol::OperationStep::Pending {
                poll_after_secs: pending.operation.poll_after_secs(),
                pending,
            },
        }
    }
}

type GuestOperationFuture<'a, T> = Pin<
    Box<dyn Future<Output = wasmtime::Result<Result<T, wit_types::ProviderError>>> + Send + 'a>,
>;

impl WasmProvider {
    fn operation_budget(
        &self,
        id: &ResourceId,
        kind: carina_provider_protocol::OperationKind,
    ) -> std::time::Duration {
        self.schemas
            .iter()
            .find(|schema| schema.resource_type() == id.resource_type)
            .and_then(|schema| schema.operation_config())
            .map_or(
                carina_provider_protocol::DEFAULT_OPERATION_BUDGET,
                |config| config.budget(kind),
            )
    }

    fn over_budget(operation: &'static str, id: ResourceId) -> ProviderError {
        ProviderError::timeout(format!(
            "WASM provider {operation} operation exceeded its schema budget"
        ))
        .for_resource(id)
    }

    async fn call_operation<WitOutcome, Progress, DecodeError, GuestCall, Decode>(
        &self,
        operation: &'static str,
        id: ResourceId,
        guest_call: GuestCall,
        decode: Decode,
    ) -> ProviderResult<Progress>
    where
        WitOutcome: 'static,
        GuestCall: for<'a> FnOnce(
            &'a WasmBindings,
            &'a mut Store<HostState>,
        ) -> GuestOperationFuture<'a, WitOutcome>,
        Decode: FnOnce(WitOutcome, ResourceId) -> Result<Progress, DecodeError>,
        DecodeError: std::error::Error + Send + Sync + 'static,
    {
        let locked = self.instance.acquire(operation).await?;
        let outcome = locked
            .call(operation, async move |bindings, store| {
                guest_call(bindings, store).await
            })
            .await?;
        match outcome {
            Ok(outcome) => decode(outcome, id.clone()).map_err(|error| {
                wasm_value_decode_provider_error(operation, error).for_resource(id)
            }),
            Err(error) => Err(wasm_convert::wit_to_core_provider_error(error)),
        }
    }

    async fn drive_operation<Progress, Initial, Poll, PollFuture>(
        operation: &'static str,
        id: ResourceId,
        budget: std::time::Duration,
        initial: Initial,
        mut poll: Poll,
    ) -> ProviderResult<Progress::Output>
    where
        Progress: OperationProgress<Verdict = std::convert::Infallible>,
        Progress::PendingState: Clone,
        Initial: Future<Output = ProviderResult<Progress>>,
        Poll: FnMut(Progress::PendingState) -> PollFuture,
        PollFuture: Future<Output = ProviderResult<Progress>>,
    {
        let outcome = carina_provider_protocol::drive_pending(
            budget,
            async move { initial.await.map(OperationProgress::into_step) },
            |pending| {
                let next = poll(pending);
                async move { next.await.map(OperationProgress::into_step) }
            },
            tokio::time::sleep,
        )
        .await
        .map_err(|error| match error {
            carina_provider_protocol::DriveError::Initial(error) => error,
            carina_provider_protocol::DriveError::Poll {
                error,
                last_pending: _last_pending,
            } => error,
        })?;
        match outcome {
            carina_provider_protocol::DriveOutcome::Done(output) => Ok(output),
            carina_provider_protocol::DriveOutcome::Verdict(verdict) => match verdict {},
            carina_provider_protocol::DriveOutcome::OverBudget {
                last_pending: _last_pending,
            } => Err(Self::over_budget(operation, id)),
        }
    }

    fn create_request_attribute_names(request: &wit_types::CreateRequest) -> BTreeSet<String> {
        request
            .res
            .attributes
            .iter()
            .map(|(name, _value)| name.clone())
            .collect()
    }

    fn create_boundary_error(id: &ResourceId, detail: &str) -> ProviderError {
        ProviderError::internal(format!("invalid WASM create result: {detail}"))
            .for_resource(id.clone())
    }

    fn create_boundary_error_with_source(
        id: &ResourceId,
        detail: &'static str,
        source: ProviderError,
    ) -> ProviderError {
        ProviderError::internal_caused_by(
            "invalid WASM create result",
            InvalidWasmCreateResult { detail, source },
        )
        .for_resource(id.clone())
    }

    /// Turn a boundary decode failure that happened after a valid create
    /// identifier into the only safe classification: the resource was
    /// created, but its post-create observation could not be decoded.
    fn created_read_decode_failure(
        operation: &'static str,
        id: &ResourceId,
        failure: wasm_convert::CreatedReadDecodeFailure,
    ) -> (AppliedIdentifier, Option<Sent>, Observation, ProviderError) {
        let (identifier, sent, declared_error, decode_error) = failure.into_parts();
        let read_error =
            wasm_value_decode_provider_error(operation, decode_error).for_resource(id.clone());
        let error =
            declared_error.unwrap_or_else(|| ProviderError::from_wire(read_error.to_wire()));
        (identifier, sent, Observation::ReadFailed(read_error), error)
    }

    fn initial_created_read_decode_failure(
        id: ResourceId,
        wit_request: &wit_types::CreateRequest,
        attempt: CreateAttempt,
        failure: wasm_convert::CreatedReadDecodeFailure,
    ) -> CreateProgress {
        let (identifier, sent, observation, error) =
            Self::created_read_decode_failure("create", &id, failure);
        Self::initial_create_verdict(
            id,
            wit_request,
            attempt,
            proto::CreateVerdict::Created(proto::CreatedFailure {
                identifier,
                observation,
                sent: sent.unwrap_or(Sent::AllRequestAttributes),
                error,
            }),
        )
    }

    fn polled_step_created_read_decode_failure(
        starting: PendingCreate,
        failure: wasm_convert::CreatedReadDecodeFailure,
    ) -> CreateProgress {
        let (identifier, _sent, observation, error) =
            Self::created_read_decode_failure("poll_create", &starting.id, failure);
        let poll = starting.pending.into_poll();
        CreateProgress::Verdict(Box::new(match poll {
            CorePoll::Unissued(attempt) => attempt.issued(identifier).failed(observation, error),
            CorePoll::Issued(issued) => issued.canonicalize(identifier).failed(observation, error),
        }))
    }

    fn polled_verdict_created_read_decode_failure(
        starting: PendingCreate,
        failure: wasm_convert::CreatedReadDecodeFailure,
    ) -> CreateProgress {
        let (identifier, sent, observation, error) =
            Self::created_read_decode_failure("poll_create", &starting.id, failure);
        let sent = sent.unwrap_or_else(|| starting.pending.sent().clone());
        Self::polled_create_verdict(
            starting,
            proto::PollVerdict::Created(proto::CreatedFailure {
                identifier,
                observation,
                sent,
                error,
            }),
        )
    }

    fn resumed_created_read_decode_failure(
        id: ResourceId,
        wit_request: wit_types::CreateRequest,
        starting: CoreResume,
        failure: wasm_convert::CreatedReadDecodeFailure,
    ) -> ResumeProgress {
        let (identifier, sent, observation, error) =
            Self::created_read_decode_failure("resume_create", &id, failure);
        match sent {
            None => Self::fail_resumed_create(starting, identifier, observation, error),
            Some(sent) => Self::resumed_create_verdict(
                id,
                &wit_request,
                starting,
                proto::ResumeVerdict::Resent(proto::CreateVerdict::Created(
                    proto::CreatedFailure {
                        identifier,
                        observation,
                        sent,
                        error,
                    },
                )),
            ),
        }
    }

    fn initial_create_step(
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        attempt: CreateAttempt,
        step: wasm_convert::DecodedCreateStep,
    ) -> CreateProgress {
        match step {
            proto::CreateStep::Success {
                identifier,
                attributes,
            } => CreateProgress::Done(Box::new(
                attempt
                    .issued(identifier, Sent::AllRequestAttributes)
                    .succeeded(attributes),
            )),
            proto::CreateStep::PartialSuccess {
                identifier,
                attributes,
                diagnostic,
            } => CreateProgress::Done(Box::new(
                attempt
                    .issued(identifier, Sent::AllRequestAttributes)
                    .partial(attributes, diagnostic),
            )),
            proto::CreateStep::Pending(pending) => {
                let request_names = Self::create_request_attribute_names(&wit_request);
                let (token, identifier, mut sent, poll_after_secs) = pending.into_parts();
                let invalid_sent = !sent.is_valid_for(&request_names);
                if invalid_sent {
                    sent.clip_to(&request_names);
                }
                let pending = match identifier {
                    Some(identifier) => attempt
                        .issued(identifier, sent)
                        .pending(token, poll_after_secs),
                    None => attempt.pending(token, sent, poll_after_secs),
                };
                if invalid_sent {
                    return CreateProgress::Verdict(Box::new(pending.preserve(
                        Self::create_boundary_error(
                            &id,
                            "pending sent attributes are not a subset of the request",
                        ),
                    )));
                }
                CreateProgress::Pending(Box::new(PendingCreate {
                    id,
                    wit_id,
                    wit_request,
                    pending,
                }))
            }
        }
    }

    fn initial_create_verdict(
        id: ResourceId,
        wit_request: &wit_types::CreateRequest,
        attempt: CreateAttempt,
        verdict: wasm_convert::DecodedCreateVerdict,
    ) -> CreateProgress {
        match verdict {
            proto::CreateVerdict::Untracked(error) | proto::CreateVerdict::Discarded(error) => {
                CreateProgress::Verdict(Box::new(attempt.untracked(error)))
            }
            proto::CreateVerdict::Created(created) => {
                let (identifier, observation, mut sent, mut error) = created.into_parts();
                let request_names = Self::create_request_attribute_names(wit_request);
                if !sent.is_valid_for(&request_names) {
                    sent.clip_to(&request_names);
                    error = Self::create_boundary_error_with_source(
                        &id,
                        "created verdict sent attributes are not a subset of the request",
                        error,
                    );
                }
                CreateProgress::Verdict(Box::new(
                    attempt.issued(identifier, sent).failed(observation, error),
                ))
            }
        }
    }

    fn preserve_polled_create(pending: PendingCreate, error: ProviderError) -> CreateProgress {
        CreateProgress::Verdict(Box::new(pending.pending.preserve(error)))
    }

    fn polled_create_step(
        starting: PendingCreate,
        step: wasm_convert::DecodedCreateStep,
    ) -> CreateProgress {
        match step {
            proto::CreateStep::Success {
                identifier,
                attributes,
            } => {
                let poll = starting.pending.into_poll();
                CreateProgress::Done(Box::new(match poll {
                    CorePoll::Unissued(attempt) => attempt.issued(identifier).succeeded(attributes),
                    CorePoll::Issued(issued) => {
                        issued.canonicalize(identifier).succeeded(attributes)
                    }
                }))
            }
            proto::CreateStep::PartialSuccess {
                identifier,
                attributes,
                diagnostic,
            } => {
                let poll = starting.pending.into_poll();
                CreateProgress::Done(Box::new(match poll {
                    CorePoll::Unissued(attempt) => {
                        attempt.issued(identifier).partial(attributes, diagnostic)
                    }
                    CorePoll::Issued(issued) => issued
                        .canonicalize(identifier)
                        .partial(attributes, diagnostic),
                }))
            }
            proto::CreateStep::Pending(next) => {
                let request_names = Self::create_request_attribute_names(&starting.wit_request);
                let same_sent = next.sent() == starting.pending.sent();
                if !same_sent || !next.sent().is_valid_for(&request_names) {
                    let error = Self::create_boundary_error(
                        &starting.id,
                        "pending changed its sent attributes",
                    );
                    return Self::preserve_polled_create(starting, error);
                }
                let poll = starting.pending.into_poll();
                let (token, identifier, sent, poll_after_secs) = next.into_parts();
                let pending = match poll {
                    CorePoll::Unissued(attempt) => match identifier {
                        Some(identifier) => {
                            attempt.issued(identifier).pending(token, poll_after_secs)
                        }
                        None => attempt.pending(token, poll_after_secs),
                    },
                    CorePoll::Issued(issued) => {
                        drop(identifier);
                        drop(sent);
                        issued.pending(token, poll_after_secs)
                    }
                };
                CreateProgress::Pending(Box::new(PendingCreate {
                    id: starting.id,
                    wit_id: starting.wit_id,
                    wit_request: starting.wit_request,
                    pending,
                }))
            }
        }
    }

    fn polled_create_verdict(
        starting: PendingCreate,
        verdict: wasm_convert::DecodedPollVerdict,
    ) -> CreateProgress {
        match verdict {
            proto::PollVerdict::Untracked(error) => {
                CreateProgress::Verdict(Box::new(starting.pending.untracked(error)))
            }
            proto::PollVerdict::Created(created) => {
                let (identifier, observation, sent, error) = created.into_parts();
                if &sent != starting.pending.sent() {
                    let error = Self::create_boundary_error(
                        &starting.id,
                        "created verdict changed the sent attributes",
                    );
                    return Self::preserve_polled_create(starting, error);
                }
                let poll = starting.pending.into_poll();
                CreateProgress::Verdict(Box::new(match poll {
                    CorePoll::Unissued(attempt) => {
                        attempt.issued(identifier).failed(observation, error)
                    }
                    CorePoll::Issued(issued) => {
                        issued.canonicalize(identifier).failed(observation, error)
                    }
                }))
            }
            proto::PollVerdict::Discarded(error) => {
                let poll = starting.pending.into_poll();
                CreateProgress::Verdict(Box::new(match poll {
                    CorePoll::Unissued(attempt) => attempt.untracked(error),
                    CorePoll::Issued(issued) => issued.discarded(error),
                }))
            }
            proto::PollVerdict::Undetermined(error) => {
                Self::preserve_polled_create(starting, error)
            }
        }
    }

    fn preserve_resent_create(
        starting: PendingResentCreate,
        error: ProviderError,
    ) -> ResumeProgress {
        ResumeProgress::Verdict(Box::new(starting.pending.preserve(error)))
    }

    fn polled_resent_step(
        starting: PendingResentCreate,
        step: wasm_convert::DecodedCreateStep,
    ) -> ResumeProgress {
        match step {
            proto::CreateStep::Success {
                identifier,
                attributes,
            } => {
                let poll = starting.pending.into_poll();
                ResumeProgress::Done(match poll {
                    CoreResentPoll::Unissued(attempt) => {
                        attempt.issued(identifier).succeeded(attributes)
                    }
                    CoreResentPoll::Issued(issued) => {
                        issued.canonicalize(identifier).succeeded(attributes)
                    }
                })
            }
            proto::CreateStep::PartialSuccess {
                identifier,
                attributes,
                diagnostic,
            } => {
                let poll = starting.pending.into_poll();
                ResumeProgress::Done(match poll {
                    CoreResentPoll::Unissued(attempt) => {
                        attempt.issued(identifier).partial(attributes, diagnostic)
                    }
                    CoreResentPoll::Issued(issued) => issued
                        .canonicalize(identifier)
                        .partial(attributes, diagnostic),
                })
            }
            proto::CreateStep::Pending(next) => {
                let request_names = Self::create_request_attribute_names(&starting.wit_request);
                let same_sent = next.sent() == starting.pending.sent();
                if !same_sent || !next.sent().is_valid_for(&request_names) {
                    let error = Self::create_boundary_error(
                        &starting.id,
                        "resent pending changed its sent attributes",
                    );
                    return Self::preserve_resent_create(starting, error);
                }
                let poll = starting.pending.into_poll();
                let (token, identifier, sent, poll_after_secs) = next.into_parts();
                let pending = match poll {
                    CoreResentPoll::Unissued(attempt) => match identifier {
                        Some(identifier) => {
                            attempt.issued(identifier).pending(token, poll_after_secs)
                        }
                        None => attempt.pending(token, poll_after_secs),
                    },
                    CoreResentPoll::Issued(issued) => {
                        drop(identifier);
                        drop(sent);
                        issued.pending(token, poll_after_secs)
                    }
                };
                ResumeProgress::Pending(Box::new(PendingResume::Resent(PendingResentCreate {
                    id: starting.id,
                    wit_id: starting.wit_id,
                    wit_request: starting.wit_request,
                    pending,
                })))
            }
        }
    }

    fn polled_resent_verdict(
        starting: PendingResentCreate,
        verdict: wasm_convert::DecodedPollVerdict,
    ) -> ResumeProgress {
        match verdict {
            proto::PollVerdict::Untracked(error) => {
                ResumeProgress::Verdict(Box::new(starting.pending.untracked(error)))
            }
            proto::PollVerdict::Created(created) => {
                let (identifier, observation, sent, error) = created.into_parts();
                if &sent != starting.pending.sent() {
                    let error = Self::create_boundary_error(
                        &starting.id,
                        "resent created verdict changed the sent attributes",
                    );
                    return Self::preserve_resent_create(starting, error);
                }
                let poll = starting.pending.into_poll();
                ResumeProgress::Verdict(Box::new(match poll {
                    CoreResentPoll::Unissued(attempt) => {
                        attempt.issued(identifier).failed(observation, error)
                    }
                    CoreResentPoll::Issued(issued) => {
                        issued.canonicalize(identifier).failed(observation, error)
                    }
                }))
            }
            proto::PollVerdict::Discarded(error) => {
                let poll = starting.pending.into_poll();
                ResumeProgress::Verdict(Box::new(match poll {
                    CoreResentPoll::Unissued(attempt) => attempt.untracked(error),
                    CoreResentPoll::Issued(issued) => issued.discarded(error),
                }))
            }
            proto::PollVerdict::Undetermined(error) => {
                Self::preserve_resent_create(starting, error)
            }
        }
    }

    fn polled_resent_created_read_decode_failure(
        starting: PendingResentCreate,
        failure: wasm_convert::CreatedReadDecodeFailure,
    ) -> ResumeProgress {
        let (identifier, sent, observation, error) =
            Self::created_read_decode_failure("poll_create", &starting.id, failure);
        let sent = sent.unwrap_or_else(|| starting.pending.sent().clone());
        Self::polled_resent_verdict(
            starting,
            proto::PollVerdict::Created(proto::CreatedFailure {
                identifier,
                observation,
                sent,
                error,
            }),
        )
    }

    fn preserve_resumed_create(resume: CoreResume, error: ProviderError) -> ResumeProgress {
        ResumeProgress::Verdict(Box::new(resume.preserve(error)))
    }

    fn fail_resumed_create(
        resume: CoreResume,
        identifier: AppliedIdentifier,
        observation: Observation,
        error: ProviderError,
    ) -> ResumeProgress {
        let failure = match resume {
            CoreResume::Unissued(attempt) => attempt.issued(identifier).failed(observation, error),
            CoreResume::Issued(resumed) => {
                resumed.canonicalize(identifier).failed(observation, error)
            }
        };
        ResumeProgress::Verdict(Box::new(failure))
    }

    fn resumed_create_step(
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        starting: CoreResume,
        step: wasm_convert::DecodedResumeStep,
    ) -> ResumeProgress {
        match step {
            proto::ResumeStep::Completed {
                identifier,
                attributes,
            } => {
                let state = AppliedState::from_identifier(id, identifier.clone(), attributes);
                let outcome = match starting {
                    CoreResume::Unissued(attempt) => attempt.issued(identifier).completed(state),
                    CoreResume::Issued(resumed) => {
                        resumed.canonicalize(identifier).completed(state)
                    }
                };
                ResumeProgress::Done(outcome)
            }
            proto::ResumeStep::Pending(next) => {
                let (token, identifier, poll_after_secs) = next.into_parts();
                let resume = match starting {
                    CoreResume::Unissued(attempt) => match identifier {
                        Some(identifier) => attempt.issued(identifier).repend(token),
                        None => attempt.repend(token),
                    },
                    CoreResume::Issued(resumed) => {
                        drop(identifier);
                        resumed.repend(token)
                    }
                };
                ResumeProgress::Pending(Box::new(PendingResume::Resume {
                    id,
                    wit_id,
                    wit_request,
                    resume,
                    poll_after_secs,
                }))
            }
            proto::ResumeStep::Resent(step) => {
                Self::resent_create_step(id, wit_id, wit_request, starting, step)
            }
        }
    }

    fn resent_create_step(
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        starting: CoreResume,
        step: wasm_convert::DecodedCreateStep,
    ) -> ResumeProgress {
        let attempt = match starting {
            CoreResume::Unissued(attempt) => attempt.resend(),
            CoreResume::Issued(resumed) => {
                return Self::preserve_resumed_create(
                    CoreResume::Issued(resumed),
                    Self::create_boundary_error(
                        &id,
                        "an issued resume attempted to resend the create",
                    ),
                );
            }
        };
        match step {
            proto::CreateStep::Success {
                identifier,
                attributes,
            } => ResumeProgress::Done(
                attempt
                    .issued(identifier, Sent::AllRequestAttributes)
                    .succeeded(attributes),
            ),
            proto::CreateStep::PartialSuccess {
                identifier,
                attributes,
                diagnostic,
            } => ResumeProgress::Done(
                attempt
                    .issued(identifier, Sent::AllRequestAttributes)
                    .partial(attributes, diagnostic),
            ),
            proto::CreateStep::Pending(pending) => {
                let request_names = Self::create_request_attribute_names(&wit_request);
                if !pending.sent().is_valid_for(&request_names) {
                    return ResumeProgress::Verdict(Box::new(attempt.preserve(
                        Self::create_boundary_error(
                            &id,
                            "resent pending attributes are not a subset of the request",
                        ),
                    )));
                }
                let (token, identifier, sent, poll_after_secs) = pending.into_parts();
                let pending = match identifier {
                    Some(identifier) => attempt
                        .issued(identifier, sent)
                        .pending(token, poll_after_secs),
                    None => attempt.pending(token, sent, poll_after_secs),
                };
                ResumeProgress::Pending(Box::new(PendingResume::Resent(PendingResentCreate {
                    id,
                    wit_id,
                    wit_request,
                    pending,
                })))
            }
        }
    }

    fn resumed_create_verdict(
        id: ResourceId,
        wit_request: &wit_types::CreateRequest,
        starting: CoreResume,
        verdict: wasm_convert::DecodedResumeVerdict,
    ) -> ResumeProgress {
        match verdict {
            proto::ResumeVerdict::Untracked(error) => {
                ResumeProgress::Verdict(Box::new(starting.untracked(error)))
            }
            proto::ResumeVerdict::Created(created) => {
                let (identifier, observation, error) = created.into_parts();
                Self::fail_resumed_create(starting, identifier, observation, error)
            }
            proto::ResumeVerdict::Discarded(error) => {
                ResumeProgress::Verdict(Box::new(starting.discarded(error)))
            }
            proto::ResumeVerdict::Undetermined(error) => {
                Self::preserve_resumed_create(starting, error)
            }
            proto::ResumeVerdict::Resent(verdict) => {
                let attempt = match starting {
                    CoreResume::Unissued(attempt) => attempt.resend(),
                    CoreResume::Issued(resumed) => {
                        return Self::preserve_resumed_create(
                            CoreResume::Issued(resumed),
                            Self::create_boundary_error(
                                &id,
                                "an issued resume attempted to resend the create",
                            ),
                        );
                    }
                };
                match verdict {
                    proto::CreateVerdict::Untracked(error)
                    | proto::CreateVerdict::Discarded(error) => {
                        ResumeProgress::Verdict(Box::new(attempt.untracked(error)))
                    }
                    proto::CreateVerdict::Created(created) => {
                        let (identifier, observation, sent, error) = created.into_parts();
                        let request_names = Self::create_request_attribute_names(wit_request);
                        if !sent.is_valid_for(&request_names) {
                            return ResumeProgress::Verdict(Box::new(attempt.preserve(
                                Self::create_boundary_error(
                                    &id,
                                    "resent verdict attributes are not a subset of the request",
                                ),
                            )));
                        }
                        ResumeProgress::Verdict(Box::new(
                            attempt.issued(identifier, sent).failed(observation, error),
                        ))
                    }
                }
            }
        }
    }

    async fn call_resume_record(
        &self,
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        resume: CoreResume,
    ) -> ResumeProgress {
        let wit_pending =
            wasm_convert::core_to_wit_resume_pending(resume.token(), resume.identifier());
        let call_id = wit_id.clone();
        let call_request = wit_request.clone();
        let call_result = async {
            let locked = self.instance.acquire("resume_create").await?;
            locked
                .call("resume_create", async |bindings, store| {
                    bindings
                        .call_resume_create(store, &call_id, &call_request, &wit_pending)
                        .await
                })
                .await
        }
        .await;
        let result = match call_result {
            Ok(result) => result,
            Err(error) => return Self::preserve_resumed_create(resume, error),
        };
        match result {
            Ok(step) => match wasm_convert::wit_to_core_resume_step(step, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(step)) => {
                    Self::resumed_create_step(id, wit_id, wit_request, resume, step)
                }
                Ok(wasm_convert::DecodedIssued::CreatedReadFailed(failure)) => {
                    Self::resumed_created_read_decode_failure(id, wit_request, resume, failure)
                }
                Err(error) => Self::preserve_resumed_create(
                    resume,
                    wasm_value_decode_provider_error("resume_create", error).for_resource(id),
                ),
            },
            Err(verdict) => match wasm_convert::wit_to_core_resume_verdict(verdict, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(verdict)) => {
                    Self::resumed_create_verdict(id, &wit_request, resume, verdict)
                }
                Ok(wasm_convert::DecodedIssued::CreatedReadFailed(failure)) => {
                    Self::resumed_created_read_decode_failure(id, wit_request, resume, failure)
                }
                Err(error) => Self::preserve_resumed_create(
                    resume,
                    wasm_value_decode_provider_error("resume_create", error).for_resource(id),
                ),
            },
        }
    }

    async fn poll_resume_state(&self, pending: PendingResume) -> ResumeProgress {
        match pending {
            PendingResume::Resume {
                id,
                wit_id,
                wit_request,
                resume,
                poll_after_secs: _poll_after_secs,
            } => {
                self.call_resume_record(id, wit_id, wit_request, resume)
                    .await
            }
            PendingResume::Resent(pending) => self.call_poll_resent_create(pending).await,
        }
    }

    async fn call_create(
        &self,
        id: ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> Result<CreateProgress, CreateFailure> {
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let wit_request = match wasm_convert::core_to_wit_create_request(&request) {
            Ok(request) => request,
            Err(error) => {
                return Err(attempt.untracked(
                    wasm_value_encode_provider_error("create", error).for_resource(id),
                ));
            }
        };
        let call_id = wit_id.clone();
        let call_request = wit_request.clone();
        let locked = match self.instance.acquire("create").await {
            Ok(locked) => locked,
            Err(error) => return Err(attempt.untracked(error.for_resource(id))),
        };
        let result = match locked
            .call("create", async |bindings, store| {
                bindings.call_create(store, &call_id, &call_request).await
            })
            .await
        {
            Ok(result) => result,
            Err(error) => return Err(attempt.untracked(error.for_resource(id))),
        };
        match result {
            Ok(step) => match wasm_convert::wit_to_core_create_step(step, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(step)) => Ok(Self::initial_create_step(
                    id,
                    wit_id,
                    wit_request,
                    attempt,
                    step,
                )),
                Ok(wasm_convert::DecodedIssued::CreatedReadFailed(failure)) => Ok(
                    Self::initial_created_read_decode_failure(id, &wit_request, attempt, failure),
                ),
                Err(error) => Err(attempt
                    .untracked(wasm_value_decode_provider_error("create", error).for_resource(id))),
            },
            Err(verdict) => match wasm_convert::wit_to_core_create_verdict(verdict, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(verdict)) => Ok(
                    Self::initial_create_verdict(id, &wit_request, attempt, verdict),
                ),
                Ok(wasm_convert::DecodedIssued::CreatedReadFailed(failure)) => Ok(
                    Self::initial_created_read_decode_failure(id, &wit_request, attempt, failure),
                ),
                Err(error) => Err(attempt
                    .untracked(wasm_value_decode_provider_error("create", error).for_resource(id))),
            },
        }
    }

    async fn call_poll_create(&self, starting: PendingCreate) -> CreateProgress {
        let id = starting.id.clone();
        let wit_pending = wasm_convert::core_to_wit_pending_create(&starting.pending);
        let call_id = starting.wit_id.clone();
        let call_request = starting.wit_request.clone();
        let call_result = async {
            let locked = self.instance.acquire("poll_create").await?;
            locked
                .call("poll_create", async |bindings, store| {
                    bindings
                        .call_poll_create(store, &call_id, &call_request, &wit_pending)
                        .await
                })
                .await
        }
        .await;
        let result = match call_result {
            Ok(result) => result,
            Err(error) => return Self::preserve_polled_create(starting, error),
        };
        match result {
            Ok(step) => match wasm_convert::wit_to_core_create_step(step, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(step)) => {
                    Self::polled_create_step(starting, step)
                }
                Ok(wasm_convert::DecodedIssued::CreatedReadFailed(failure)) => {
                    Self::polled_step_created_read_decode_failure(starting, failure)
                }
                Err(error) => Self::preserve_polled_create(
                    starting,
                    wasm_value_decode_provider_error("poll_create", error).for_resource(id),
                ),
            },
            Err(verdict) => match wasm_convert::wit_to_core_poll_verdict(verdict, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(verdict)) => {
                    Self::polled_create_verdict(starting, verdict)
                }
                Ok(wasm_convert::DecodedIssued::CreatedReadFailed(failure)) => {
                    Self::polled_verdict_created_read_decode_failure(starting, failure)
                }
                Err(error) => Self::preserve_polled_create(
                    starting,
                    wasm_value_decode_provider_error("poll_create", error).for_resource(id),
                ),
            },
        }
    }

    async fn call_poll_resent_create(&self, starting: PendingResentCreate) -> ResumeProgress {
        let wit_pending = wasm_convert::core_to_wit_pending_create_parts(
            starting.pending.token(),
            starting.pending.identifier(),
            starting.pending.sent(),
            starting.pending.poll_after_secs(),
        );
        let call_id = starting.wit_id.clone();
        let call_request = starting.wit_request.clone();
        let call_result = async {
            let locked = self.instance.acquire("poll_create").await?;
            locked
                .call("poll_create", async |bindings, store| {
                    bindings
                        .call_poll_create(store, &call_id, &call_request, &wit_pending)
                        .await
                })
                .await
        }
        .await;
        let result = match call_result {
            Ok(result) => result,
            Err(error) => return Self::preserve_resent_create(starting, error),
        };
        match result {
            Ok(step) => match wasm_convert::wit_to_core_create_step(step, &starting.id) {
                Ok(wasm_convert::DecodedIssued::Decoded(step)) => {
                    Self::polled_resent_step(starting, step)
                }
                Ok(wasm_convert::DecodedIssued::CreatedReadFailed(failure)) => {
                    Self::polled_resent_created_read_decode_failure(starting, failure)
                }
                Err(error) => {
                    let id = starting.id.clone();
                    Self::preserve_resent_create(
                        starting,
                        wasm_value_decode_provider_error("poll_create", error).for_resource(id),
                    )
                }
            },
            Err(verdict) => match wasm_convert::wit_to_core_poll_verdict(verdict, &starting.id) {
                Ok(wasm_convert::DecodedIssued::Decoded(verdict)) => {
                    Self::polled_resent_verdict(starting, verdict)
                }
                Ok(wasm_convert::DecodedIssued::CreatedReadFailed(failure)) => {
                    Self::polled_resent_created_read_decode_failure(starting, failure)
                }
                Err(error) => {
                    let id = starting.id.clone();
                    Self::preserve_resent_create(
                        starting,
                        wasm_value_decode_provider_error("poll_create", error).for_resource(id),
                    )
                }
            },
        }
    }

    async fn drive_create(
        &self,
        id: ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
        budget: std::time::Duration,
    ) -> Result<CreateOutcome, CreateFailure> {
        let initial = self.call_create(id.clone(), request, attempt);
        let outcome = carina_provider_protocol::drive_pending_infallible(
            budget,
            async move {
                match initial.await {
                    Ok(progress) => progress,
                    Err(failure) => CreateProgress::Verdict(Box::new(failure)),
                }
                .into_step()
            },
            |pending| async move { self.call_poll_create(*pending).await.into_step() },
            tokio::time::sleep,
        )
        .await;
        match outcome {
            carina_provider_protocol::DriveOutcome::Done(outcome) => Ok(outcome),
            carina_provider_protocol::DriveOutcome::Verdict(failure) => Err(*failure),
            carina_provider_protocol::DriveOutcome::OverBudget { last_pending } => {
                Err(last_pending
                    .pending
                    .preserve(Self::over_budget("create", id)))
            }
        }
    }

    async fn drive_resume_create(
        &self,
        request: ResumeCreateRequest,
        budget: std::time::Duration,
    ) -> Result<ResumeOutcome, ResumeFailure> {
        let (resume, create_request) = request.into_parts();
        let id = resume.id().clone();
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let wit_request = match wasm_convert::core_to_wit_create_request(&create_request) {
            Ok(request) => request,
            Err(error) => {
                return Err(resume.preserve(
                    wasm_value_encode_provider_error("resume_create", error).for_resource(id),
                ));
            }
        };
        let initial = self.call_resume_record(id.clone(), wit_id, wit_request, resume);
        let outcome = carina_provider_protocol::drive_pending_infallible(
            budget,
            async move { initial.await.into_step() },
            |pending| async move { self.poll_resume_state(*pending).await.into_step() },
            tokio::time::sleep,
        )
        .await;
        match outcome {
            carina_provider_protocol::DriveOutcome::Done(outcome) => Ok(outcome),
            carina_provider_protocol::DriveOutcome::Verdict(failure) => Err(*failure),
            carina_provider_protocol::DriveOutcome::OverBudget { last_pending } => {
                Err((*last_pending).into_failure(Self::over_budget("resume_create", id)))
            }
        }
    }

    async fn call_update(
        &self,
        id: ResourceId,
        identifier: String,
        request: UpdateRequest,
    ) -> ProviderResult<UpdateProgress> {
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let wit_request = wasm_convert::core_to_wit_update_request(&request).map_err(|error| {
            wasm_value_encode_provider_error("update", error).for_resource(id.clone())
        })?;
        let call_id = wit_id.clone();
        let call_identifier = identifier.clone();
        let call_request = wit_request.clone();
        self.call_operation(
            "update",
            id,
            move |bindings, store| {
                Box::pin(async move {
                    bindings
                        .call_update(store, &call_id, &call_identifier, &call_request)
                        .await
                })
            },
            move |outcome, id| {
                let outcome = wasm_convert::wit_to_core_update_outcome(outcome, &id)?;
                Ok::<_, wasm_convert::WasmAppliedStateDecodeError>(match outcome {
                    wasm_convert::WasmUpdateOutcome::Done(outcome) => UpdateProgress::Done(outcome),
                    wasm_convert::WasmUpdateOutcome::Pending(operation) => {
                        UpdateProgress::Pending(Box::new(PendingUpdate {
                            id,
                            wit_id,
                            identifier,
                            wit_request,
                            operation,
                        }))
                    }
                })
            },
        )
        .await
    }

    async fn call_poll_update(&self, pending: PendingUpdate) -> ProviderResult<UpdateProgress> {
        let PendingUpdate {
            id,
            wit_id,
            identifier,
            wit_request,
            operation,
        } = pending;
        let wit_pending = operation.into_wit();
        let call_id = wit_id.clone();
        let call_identifier = identifier.clone();
        let call_request = wit_request.clone();
        self.call_operation(
            "poll_update",
            id,
            move |bindings, store| {
                Box::pin(async move {
                    bindings
                        .call_poll_update(
                            store,
                            &call_id,
                            &call_identifier,
                            &call_request,
                            &wit_pending,
                        )
                        .await
                })
            },
            move |outcome, id| {
                let outcome = wasm_convert::wit_to_core_update_outcome(outcome, &id)?;
                Ok::<_, wasm_convert::WasmAppliedStateDecodeError>(match outcome {
                    wasm_convert::WasmUpdateOutcome::Done(outcome) => UpdateProgress::Done(outcome),
                    wasm_convert::WasmUpdateOutcome::Pending(operation) => {
                        UpdateProgress::Pending(Box::new(PendingUpdate {
                            id,
                            wit_id,
                            identifier,
                            wit_request,
                            operation,
                        }))
                    }
                })
            },
        )
        .await
    }

    async fn call_delete(
        &self,
        id: ResourceId,
        identifier: String,
        request: DeleteRequest,
    ) -> ProviderResult<DeleteProgress> {
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let wit_request = wasm_convert::core_to_wit_delete_request(&request);
        let call_id = wit_id.clone();
        let call_identifier = identifier.clone();
        let call_request = wit_request;
        self.call_operation(
            "delete",
            id,
            move |bindings, store| {
                Box::pin(async move {
                    bindings
                        .call_delete(store, &call_id, &call_identifier, call_request)
                        .await
                })
            },
            move |outcome, id| {
                Ok::<_, std::convert::Infallible>(
                    match wasm_convert::wit_to_core_delete_outcome(outcome) {
                        wasm_convert::WasmDeleteOutcome::Deleted => DeleteProgress::Deleted,
                        wasm_convert::WasmDeleteOutcome::Pending(operation) => {
                            DeleteProgress::Pending(Box::new(PendingDelete {
                                id,
                                wit_id,
                                identifier,
                                wit_request,
                                operation,
                            }))
                        }
                    },
                )
            },
        )
        .await
    }

    async fn call_poll_delete(&self, pending: PendingDelete) -> ProviderResult<DeleteProgress> {
        let PendingDelete {
            id,
            wit_id,
            identifier,
            wit_request,
            operation,
        } = pending;
        let wit_pending = operation.into_wit();
        let call_id = wit_id.clone();
        let call_identifier = identifier.clone();
        let call_request = wit_request;
        self.call_operation(
            "poll_delete",
            id,
            move |bindings, store| {
                Box::pin(async move {
                    bindings
                        .call_poll_delete(
                            store,
                            &call_id,
                            &call_identifier,
                            call_request,
                            &wit_pending,
                        )
                        .await
                })
            },
            move |outcome, id| {
                Ok::<_, std::convert::Infallible>(
                    match wasm_convert::wit_to_core_delete_outcome(outcome) {
                        wasm_convert::WasmDeleteOutcome::Deleted => DeleteProgress::Deleted,
                        wasm_convert::WasmDeleteOutcome::Pending(operation) => {
                            DeleteProgress::Pending(Box::new(PendingDelete {
                                id,
                                wit_id,
                                identifier,
                                wit_request,
                                operation,
                            }))
                        }
                    },
                )
            },
        )
        .await
    }
}

impl Provider for WasmProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let wit_request = wasm_convert::core_to_wit_read_request(&request);
        let identifier = identifier.as_str().to_string();
        let id = id.clone();
        Box::pin(async move {
            let locked = self.instance.acquire("read").await?;
            let result = locked
                .call("read", async |bindings, store| {
                    bindings
                        .call_read(store, &wit_id, &identifier, wit_request)
                        .await
                })
                .await?;
            match result {
                Ok(wit_outcome) => wasm_convert::wit_to_core_read_outcome(wit_outcome, &id)
                    .map_err(|error| {
                        wasm_value_decode_provider_error("read", error).for_resource(id.clone())
                    }),
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
            }
        })
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
        Box::pin(async move {
            let locked = self.instance.acquire("read_data_source").await?;
            let result = locked
                .call("read_data_source", async |bindings, store| {
                    bindings.call_read_data_source(store, &wit_resource).await
                })
                .await?;
            match result {
                Ok(wit_state) => {
                    wasm_convert::wit_to_core_state(&wit_state, &id).map_err(|error| {
                        wasm_value_decode_provider_error("read_data_source", error)
                            .for_resource(id.clone())
                    })
                }
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
            }
        })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let id = id.clone();
        let budget = self.operation_budget(&id, carina_provider_protocol::OperationKind::Create);
        Box::pin(async move { self.drive_create(id, request, attempt, budget).await })
    }

    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        let budget = self.operation_budget(
            request.id(),
            carina_provider_protocol::OperationKind::Create,
        );
        Box::pin(async move { self.drive_resume_create(request, budget).await })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.as_str().to_string();
        let budget = self.operation_budget(&id, carina_provider_protocol::OperationKind::Update);
        Box::pin(async move {
            let initial = self.call_update(id.clone(), identifier, request);
            Self::drive_operation("update", id, budget, initial, |pending| {
                self.call_poll_update(*pending)
            })
            .await
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let identifier = identifier.as_str().to_string();
        let budget = self.operation_budget(&id, carina_provider_protocol::OperationKind::Delete);
        Box::pin(async move {
            let initial = self.call_delete(id.clone(), identifier, request);
            Self::drive_operation("delete", id, budget, initial, |pending| {
                self.call_poll_delete(*pending)
            })
            .await
        })
    }

    fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let locked = self.instance.acquire("required_permissions").await?;
                locked
                    .call("required_permissions", async |bindings, store| {
                        bindings.call_required_permissions(store, &wit_id, op).await
                    })
                    .await
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
                let locked = self.instance.acquire("satisfier_hint").await?;
                let patterns = locked
                    .call("satisfier_hint", async |bindings, store| {
                        bindings
                            .call_satisfier_hint(store, &wit_id, &wit_attr_path)
                            .await
                    })
                    .await?;
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
    instance: Arc<FailFastInstance>,
}

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

type WitStateAttributeUpdate = (ResourceId, Vec<(String, wit_types::PersistedValue)>);

trait StateValueAccess {
    fn state_mut(&mut self, id: &ResourceId) -> Option<&mut State>;
}

impl StateValueAccess for carina_core::provider::StateTransformMap {
    fn state_mut(&mut self, id: &ResourceId) -> Option<&mut State> {
        self.get_mut(id)
    }
}

#[cfg(test)]
impl StateValueAccess for HashMap<ResourceId, State> {
    fn state_mut(&mut self, id: &ResourceId) -> Option<&mut State> {
        self.get_mut(id)
    }
}

fn validate_wit_state_result_keys(
    operation: &'static str,
    expected_ids_by_key: &HashMap<String, ResourceId>,
    results: Vec<(String, wit_types::PersistedState)>,
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
        return Err(ProviderError::internal_caused_by(
            "WASM provider returned invalid state keys",
            WasmOperationError {
                operation,
                source: error,
            },
        ));
    }

    let mut updates = Vec::with_capacity(expected_ids_by_key.len());
    for (key, id) in expected_ids_by_key {
        let Some(state) = returned.remove(key) else {
            return Err(ProviderError::internal_caused_by(
                "WASM provider returned invalid state keys",
                WasmOperationError {
                    operation,
                    source: WasmNormalizerResultKeysError {
                        missing: BTreeSet::from([key.clone()]),
                        unexpected: BTreeSet::new(),
                        duplicates: BTreeSet::new(),
                    },
                },
            ));
        };
        updates.push((id.clone(), state.attributes));
    }
    Ok(updates)
}

fn take_single_wit_state_result(
    operation: &'static str,
    id: ResourceId,
    results: Vec<(String, wit_types::PersistedState)>,
) -> ProviderResult<WitStateAttributeUpdate> {
    if results.len() != 1 {
        return Err(ProviderError::internal_caused_by(
            "WASM provider returned an invalid state count",
            WasmOperationError {
                operation,
                source: WasmNormalizerResultCountError {
                    actual: results.len(),
                },
            },
        )
        .for_resource(id));
    }

    let Some((_, state)) = results.into_iter().next() else {
        return Err(ProviderError::internal_caused_by(
            "WASM provider returned an invalid state count",
            WasmOperationError {
                operation,
                source: WasmNormalizerResultCountError { actual: 0 },
            },
        )
        .for_resource(id));
    };
    Ok((id, state.attributes))
}

fn apply_wit_state_attribute_updates<M: StateValueAccess>(
    operation: &'static str,
    current_states: &mut M,
    updates: Vec<WitStateAttributeUpdate>,
) -> ProviderResult<()> {
    let decoded = updates
        .into_iter()
        .map(|(id, attributes)| {
            wasm_convert::wit_to_core_persisted_value_map(&attributes)
                .map(|attributes| (id.clone(), attributes))
                .map_err(|error| {
                    wasm_value_decode_provider_error(operation, error).for_resource(id)
                })
        })
        .collect::<ProviderResult<Vec<_>>>()?;

    for (id, attributes) in decoded {
        if let Some(state) = current_states.state_mut(&id) {
            state.attributes = attributes;
        }
    }
    Ok(())
}

/// One stripped attribute retained so it can be reinserted at its original
/// position after WASM desired normalization returns.
struct StrippedAttribute {
    insert_index: usize,
    key: String,
    value: Value,
}

/// Attributes removed from a resource slice, grouped by slice position.
struct StrippedAttributes(Vec<Vec<StrippedAttribute>>);

/// Strip every resource attribute that the WASM provider boundary refuses to
/// serialize, retaining the original `IndexMap` positions for restoration.
fn strip_provider_boundary_attributes(resources: &mut [Resource]) -> StrippedAttributes {
    let mut out = Vec::with_capacity(resources.len());
    for resource in resources.iter_mut() {
        let mut stripped = Vec::new();
        let to_remove: Vec<(usize, String)> = resource
            .attributes
            .iter()
            .enumerate()
            .filter(|(_, (_, value))| value_contains_unknown(value) || contains_resource_ref(value))
            .map(|(i, (key, _))| (i, key.clone()))
            .collect();

        for (insert_index, key) in to_remove.into_iter().rev() {
            if let Some(value) = resource.attributes.shift_remove(&key) {
                stripped.push(StrippedAttribute {
                    insert_index,
                    key,
                    value,
                });
            }
        }

        stripped.sort_by_key(|entry| entry.insert_index);
        out.push(stripped);
    }
    StrippedAttributes(out)
}

/// Reinsert attributes removed by [`strip_provider_boundary_attributes`] at
/// their original positions.
fn restore_stripped_attributes(resources: &mut [Resource], stripped: StrippedAttributes) {
    for (resource, entries) in resources.iter_mut().zip(stripped.0) {
        for entry in entries {
            let target = entry.insert_index.min(resource.attributes.len());
            resource
                .attributes
                .shift_insert(target, entry.key, entry.value);
        }
    }
}

/// Run one WASM desired-normalization boundary call with unsupported
/// attributes temporarily removed.
///
/// The higher-ranked callback keeps the stripped slice scoped to this one
/// operation. Attributes are restored before returning on both success and
/// error.
async fn with_wasm_serializable_resources<F>(
    resources: &mut [Resource],
    operation: F,
) -> ProviderResult<()>
where
    F: for<'a> FnOnce(&'a mut [Resource]) -> BoxFuture<'a, ProviderResult<()>>,
{
    let stripped = strip_provider_boundary_attributes(resources);
    let result = operation(resources).await;
    restore_stripped_attributes(resources, stripped);
    result
}

impl ProviderNormalizer for WasmProviderNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [Resource],
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        let instance = Arc::clone(&self.instance);
        Box::pin(async move {
            with_wasm_serializable_resources(resources, move |resources| {
                Box::pin(async move {
                    let (sealed, unsealer) = secret_seal::seal(resources).map_err(|error| {
                        wasm_value_encode_provider_error("normalize_desired", error)
                    })?;

                    // Plain `.await`, not a nested `block_on`: the guarded store is
                    // acquired and dropped within this one polled future, so the
                    // apply-path `renormalize` calling this once per resource cannot
                    // self-deadlock (carina#3112).
                    let result = {
                        let locked = instance.acquire("normalize_desired").await?;
                        let result = locked
                            .call("normalize_desired", async |bindings, store| {
                                bindings.call_normalize_desired(store, &sealed).await
                            })
                            .await?;
                        result.map_err(wasm_convert::wit_to_core_provider_error)
                    }?;

                    // The scoped host helper hides ref/Unknown-bearing attributes only
                    // while this WASM transport call is in flight and restores them on
                    // every return path. Secret restoration filters mangled attributes
                    // before guest values are written back.
                    let restored = unsealer.try_restore(result).map_err(|error| {
                        wasm_value_decode_provider_error("normalize_desired", error)
                    })?;
                    restored.apply_to(resources);
                    Ok(())
                })
            })
            .await
        })
    }

    fn normalize_state<'a>(
        &'a self,
        current_states: &'a mut carina_core::provider::StateTransformMap,
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            let mut resolved_by_key = HashMap::new();
            let mut colliding_keys = HashSet::new();
            let mut single_wit_states = Vec::new();
            let mut updates = Vec::new();

            for (id, state) in current_states.iter() {
                let key = wasm_convert::resource_id_wire_key(id);
                let wit = wasm_convert::core_to_wit_persisted_state(state)
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
                    let locked = self.instance.acquire("normalize_state").await?;
                    let result = locked
                        .call("normalize_state", async |bindings, store| {
                            bindings
                                .call_normalize_state(store, &resolved_wit_states)
                                .await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
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
                let wit_states = [(key, wit_state)];
                let result = {
                    let locked = self.instance.acquire("normalize_state").await?;
                    let result = locked
                        .call("normalize_state", async |bindings, store| {
                            bindings.call_normalize_state(store, &wit_states).await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
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
        current_states: &'a mut carina_core::provider::StateTransformMap,
        saved_attrs: &'a SavedAttrs,
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            let mut resolved_by_key = HashMap::new();
            let mut colliding_keys = HashSet::new();
            let mut single_batches = Vec::new();
            let mut updates = Vec::new();

            for (id, state) in current_states.iter() {
                let key = wasm_convert::resource_id_wire_key(id);
                let wit_state =
                    wasm_convert::core_to_wit_persisted_state(state).map_err(|error| {
                        wasm_value_encode_provider_error("hydrate_read_state current state", error)
                    })?;
                let wit_saved =
                    wasm_convert::core_saved_attrs_to_wit(saved_attrs, id).map_err(|error| {
                        wasm_value_encode_provider_error(
                            "hydrate_read_state saved attributes",
                            error,
                        )
                    })?;

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
                    let locked = self.instance.acquire("hydrate_read_state").await?;
                    let result = locked
                        .call("hydrate_read_state", async |bindings, store| {
                            bindings
                                .call_hydrate_read_state(
                                    store,
                                    &resolved_wit_states,
                                    &resolved_wit_saved,
                                )
                                .await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
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
                let wit_states = [(key.clone(), wit_state)];
                let wit_saved = wit_saved
                    .map(|attrs| vec![(key, attrs)])
                    .unwrap_or_default();
                let result = {
                    let locked = self.instance.acquire("hydrate_read_state").await?;
                    let result = locked
                        .call("hydrate_read_state", async |bindings, store| {
                            bindings
                                .call_hydrate_read_state(store, &wit_states, &wit_saved)
                                .await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::provider::SentRecord;
    use carina_core::resource::{
        AccessPath, ConcreteValue, Current, DeferredValue, InterpolationPart, UnknownReason,
    };

    #[test]
    fn protocol_v12_wit_surface_compiles() {
        fn assert_type<T>() {}

        assert_type::<wit_types::Observation>();
        assert_type::<wit_types::SentAttributes>();
        assert_type::<wit_types::PendingCreate>();
        assert_type::<wit_types::CreateStep>();
        assert_type::<wit_types::CreateVerdict>();
        assert_type::<wit_types::PollVerdict>();
        assert_type::<wit_types::ResumePending>();
        assert_type::<wit_types::ResumeStep>();
        assert_type::<wit_types::ResumeVerdict>();
    }

    fn test_string_value(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string().into()))
    }

    fn boundary_resource() -> (ResourceId, wit_types::ResourceId, wit_types::CreateRequest) {
        let id = ResourceId::with_provider_identity("mock", "test.Resource", "main", None);
        let mut resource = Resource::from_id(id.clone());
        resource.set_attr("name", test_string_value("example"));
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let wit_request = wit_types::CreateRequest {
            res: wasm_convert::core_to_wit_resource(&resource)
                .expect("boundary fixture must encode"),
        };
        (id, wit_id, wit_request)
    }

    fn sent_name_only() -> Sent {
        Sent::Only(["name".to_string()].into_iter().collect())
    }

    fn malformed_wit_attributes() -> Vec<(String, wit_types::Value)> {
        vec![(
            "broken".to_string(),
            wit_types::Value::ListVal("not-json".to_string()),
        )]
    }

    fn decode_failed_create_step(id: &ResourceId) -> wasm_convert::CreatedReadDecodeFailure {
        let step = wit_types::CreateStep::Success(wit_types::AppliedState {
            identifier: "decoded-created-id".to_string(),
            attributes: malformed_wit_attributes(),
        });
        match wasm_convert::wit_to_core_create_step(step, id)
            .expect("the valid identifier makes the attribute failure recoverable")
        {
            wasm_convert::DecodedIssued::CreatedReadFailed(failure) => failure,
            wasm_convert::DecodedIssued::Decoded(_) => {
                panic!("malformed attributes unexpectedly decoded")
            }
        }
    }

    fn decode_failed_created_verdict(id: &ResourceId) -> wasm_convert::CreatedReadDecodeFailure {
        let error = ProviderError::internal("guest create failure");
        let verdict = wit_types::PollVerdict::Created(wit_types::CreatedFailure {
            identifier: "decoded-created-id".to_string(),
            read: wit_types::Observation::Read(wit_types::ReadOutcome::Found(
                wit_types::AppliedState {
                    identifier: "decoded-created-id".to_string(),
                    attributes: malformed_wit_attributes(),
                },
            )),
            sent: wit_types::SentAttributes::Only(vec!["name".to_string()]),
            error: wasm_convert::wire_to_wit_provider_error(error.to_wire()),
        });
        match wasm_convert::wit_to_core_poll_verdict(verdict, id)
            .expect("the valid identifier makes the observation failure recoverable")
        {
            wasm_convert::DecodedIssued::CreatedReadFailed(failure) => failure,
            wasm_convert::DecodedIssued::Decoded(_) => {
                panic!("malformed observation unexpectedly decoded")
            }
        }
    }

    fn assert_created_after_decode_failure(progress: CreateProgress) {
        let CreateProgress::Verdict(failure) = progress else {
            panic!("post-identifier decode failure must be a created verdict")
        };
        match failure.into_kind() {
            CreateFailureKind::Created { created, error: _ } => {
                assert_eq!(created.identifier().as_str(), "decoded-created-id");
                assert!(matches!(created.observation(), Observation::ReadFailed(_)));
            }
            CreateFailureKind::Untracked(error) => {
                panic!("post-identifier decode failure became untracked: {error}")
            }
            CreateFailureKind::InFlight { error, .. } => {
                panic!("post-identifier decode failure stayed in flight: {error}")
            }
        }
    }

    fn assert_created_after_resume_decode_failure(progress: ResumeProgress) {
        let ResumeProgress::Verdict(failure) = progress else {
            panic!("post-identifier resume decode failure must be a created verdict")
        };
        match failure.into_kind() {
            ResumeFailureKind::Created { created, error: _ } => {
                assert_eq!(created.identifier().as_str(), "decoded-created-id");
                assert!(matches!(created.observation(), Observation::ReadFailed(_)));
            }
            ResumeFailureKind::Ended(error) => {
                panic!("post-identifier resume decode failure ended the record: {error}")
            }
            ResumeFailureKind::InFlight { error, .. } => {
                panic!("post-identifier resume decode failure stayed in flight: {error}")
            }
        }
    }

    struct PendingFixtureProvider;

    impl Provider for PendingFixtureProvider {
        fn name(&self) -> &str {
            "pending-fixture"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            let id = id.clone();
            Box::pin(async move { Ok(ReadOutcome::not_found(id)) })
        }

        fn read_data_source(
            &self,
            resource: &carina_core::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            attempt: CreateAttempt,
        ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
            Box::pin(async move {
                let pending = attempt
                    .issued(
                        AppliedIdentifier::new("issued-id").expect("valid identifier"),
                        sent_name_only(),
                    )
                    .pending(proto::PendingToken::new("original-token"), 1);
                Err(pending.preserve(ProviderError::timeout("fixture remains pending")))
            })
        }

        fn resume_create(
            &self,
            request: ResumeCreateRequest,
        ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
            Box::pin(async move {
                let (resume, create_request) = request.into_parts();
                drop(create_request);
                Err(resume.preserve(ProviderError::internal(
                    "resume is not used by this fixture",
                )))
            })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            Box::pin(async {
                Err(ProviderError::internal(
                    "update is not used by this fixture",
                ))
            })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async {
                Err(ProviderError::internal(
                    "delete is not used by this fixture",
                ))
            })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    async fn pending_create_with_issued_identifier() -> PendingCreate {
        let (id, wit_id, wit_request) = boundary_resource();
        let mut resource = Resource::from_id(id.clone());
        resource.set_attr("name", test_string_value("example"));
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let module_gate = carina_core::executor::ModuleConstraintGate::new(&[]);
        let schemas = carina_core::schema::SchemaRegistry::new();
        let preparation = carina_core::executor::ProviderPreparationContext::new(
            &bindings,
            &module_gate,
            &[],
            &carina_core::provider::NoopNormalizer,
            &[],
            &schemas,
        );
        let request = carina_core::executor::prepare_create_request(resource, &preparation)
            .await
            .expect("fixture resource passes provider preparation");
        let failure = carina_core::executor::dispatch_create(&PendingFixtureProvider, &id, request)
            .await
            .expect_err("fixture create remains pending");
        let pending = match failure.into_kind() {
            CreateFailureKind::InFlight {
                pending,
                error: _error,
            } => pending
                .into_fresh()
                .unwrap_or_else(|_| panic!("fixture provider returned a persisted continuation")),
            CreateFailureKind::Created {
                created: _created,
                error,
            } => {
                panic!("fixture create was created: {error}")
            }
            CreateFailureKind::Untracked(error) => {
                panic!("fixture create was not pending: {error}")
            }
        };
        PendingCreate {
            id,
            wit_id,
            wit_request,
            pending,
        }
    }

    fn persisted_resume_with_issued_identifier() -> InFlightCreate {
        let (id, _wit_id, _wit_request) = boundary_resource();
        let identity = id
            .identity()
            .cloned()
            .expect("resume fixture identity is resolved");
        let mut row = serde_json::to_value(carina_state::ResourceState::new(
            &id.resource_type,
            identity,
            &id.provider,
        ))
        .expect("fixture row serializes");
        row["current"] = serde_json::json!({
            "kind": "create_in_flight",
            "envelope": {
                "token": "original-token",
                "identifier": "issued-id",
                "write_only": {},
                "secrets": {},
            },
            "started_at": "2026-10-23T12:34:56Z",
        });
        let mut document =
            serde_json::to_value(carina_state::StateFile::new()).expect("fixture state serializes");
        document["resources"] = serde_json::json!([row]);
        let loaded = carina_state::state::check_and_migrate(&document.to_string())
            .expect("v15 fixture passes the public state loader")
            .into_state();
        match loaded.build_state_for_resource(&id) {
            Current::CreateInFlight(pending) => pending,
            Current::Absent(state) => panic!("resume fixture loaded as absent: {state:?}"),
            Current::Present(state) => panic!("resume fixture loaded as present: {state:?}"),
            Current::MissingIdentifier(state) => {
                panic!("resume fixture loaded without an identifier: {state:?}")
            }
        }
    }

    async fn resume_witness(pending: InFlightCreate) -> CoreResume {
        let mut resource = Resource::from_id(pending.id().clone());
        resource.set_attr("name", test_string_value("example"));
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let gate = carina_core::executor::ModuleConstraintGate::new(&[]);
        let provider_configs = [];
        let normalizer = carina_core::provider::NoopNormalizer;
        let factories = [];
        let schemas = carina_core::schema::SchemaRegistry::new();
        let context = carina_core::executor::ProviderPreparationContext::new(
            &bindings,
            &gate,
            &provider_configs,
            &normalizer,
            &factories,
            &schemas,
        );
        let request = carina_core::executor::prepare_create_request(resource, &context)
            .await
            .expect("resume fixture request must pass provider preparation");
        let (resume, _request) = ResumeCreateRequest::for_test(pending, request)
            .expect("resume fixture route must match")
            .into_parts();
        resume
    }

    fn assert_original_polled_record(
        progress: CreateProgress,
        expected_id: &ResourceId,
        expected_started_at: &carina_core::provider::Timestamp,
    ) {
        match progress {
            CreateProgress::Verdict(failure) => match failure.into_kind() {
                CreateFailureKind::InFlight { pending, error } => {
                    assert_eq!(pending.id(), expected_id);
                    match pending.persistence() {
                        InFlightPersistence::Fresh(fresh) => {
                            assert_eq!(fresh.token().expose_for_provider_call(), "original-token");
                            assert_eq!(
                                fresh.identifier().map(AppliedIdentifier::as_str),
                                Some("issued-id")
                            );
                            assert_eq!(fresh.sent(), &sent_name_only());
                            assert_eq!(fresh.started_at(), expected_started_at);
                        }
                        InFlightPersistence::Persisted => {
                            panic!("fresh polling unexpectedly produced a persisted envelope")
                        }
                    }
                    assert_eq!(pending.token().expose_for_provider_call(), "original-token");
                    assert_eq!(
                        pending.identifier().map(AppliedIdentifier::as_str),
                        Some("issued-id")
                    );
                    match pending.sent() {
                        SentRecord::Request(Sent::Only(attributes)) => {
                            assert_eq!(attributes, ["name".to_string()].into_iter().collect());
                        }
                        SentRecord::Request(Sent::AllRequestAttributes) => {
                            panic!("the starting sent subset was replaced with all attributes")
                        }
                        SentRecord::Persisted(envelope) => {
                            panic!(
                                "fresh polling unexpectedly produced a persisted envelope: {envelope:?}"
                            )
                        }
                    }
                    assert!(!error.to_string().is_empty());
                }
                CreateFailureKind::Untracked(error) => {
                    panic!("issued create was downgraded to untracked: {error}")
                }
                CreateFailureKind::Created { created, error } => {
                    panic!(
                        "invalid boundary response was accepted as created for {}: {error}",
                        created.identifier()
                    )
                }
            },
            CreateProgress::Done(outcome) => {
                panic!("invalid boundary response unexpectedly completed: {outcome:?}")
            }
            CreateProgress::Pending(pending) => {
                panic!(
                    "invalid boundary response unexpectedly replaced token with {:?}",
                    pending.pending.token()
                )
            }
        }
    }

    fn assert_original_resumed_record(progress: ResumeProgress, expected: &InFlightCreate) {
        match progress {
            ResumeProgress::Verdict(failure) => match failure.into_kind() {
                ResumeFailureKind::InFlight { pending, error } => {
                    assert_eq!(&pending, expected);
                    assert!(!error.to_string().is_empty());
                }
                ResumeFailureKind::Ended(error) => {
                    panic!("issued resume was incorrectly ended: {error}")
                }
                ResumeFailureKind::Created { created, error } => {
                    panic!(
                        "invalid resume boundary response was accepted as created for {}: {error}",
                        created.identifier()
                    )
                }
            },
            ResumeProgress::Done(outcome) => {
                panic!("invalid resume boundary response unexpectedly completed: {outcome:?}")
            }
            ResumeProgress::Pending(pending) => match pending.as_ref() {
                PendingResume::Resume {
                    id,
                    wit_id,
                    wit_request,
                    resume,
                    poll_after_secs,
                } => panic!(
                    "invalid resume boundary response unexpectedly replaced the record for {id} ({wit_id:?}, {wit_request:?}, {resume:?}, {poll_after_secs})"
                ),
                PendingResume::Resent(pending) => panic!(
                    "invalid resume boundary response unexpectedly resent create for {}",
                    pending.id
                ),
            },
        }
    }

    fn assert_issued_poll_repended_with_original_identifier(progress: CreateProgress) {
        let CreateProgress::Pending(pending) = progress else {
            panic!("issued poll must accept the replacement token")
        };
        assert_eq!(
            pending.pending.identifier().map(AppliedIdentifier::as_str),
            Some("issued-id")
        );
        assert_eq!(
            pending.pending.token().expose_for_provider_call(),
            "replacement-token"
        );
    }

    fn assert_issued_resume_repended_with_original_identifier(progress: ResumeProgress) {
        let ResumeProgress::Pending(pending) = progress else {
            panic!("issued resume must accept the replacement token")
        };
        let PendingResume::Resume { resume, .. } = *pending else {
            panic!("issued resume unexpectedly changed into a resent create")
        };
        assert_eq!(
            resume.identifier().map(AppliedIdentifier::as_str),
            Some("issued-id")
        );
        assert_eq!(
            resume.token().expose_for_provider_call(),
            "replacement-token"
        );
    }

    #[tokio::test]
    async fn issued_poll_boundary_never_drops_or_mutates_the_starting_record() {
        let starting = pending_create_with_issued_identifier().await;
        let expected_id = starting.id.clone();
        let expected_started_at = starting.pending.started_at().clone();
        let verdict: wasm_convert::DecodedPollVerdict =
            proto::PollVerdict::Untracked(ProviderError::internal("untracked"));
        assert_original_polled_record(
            WasmProvider::polled_create_verdict(starting, verdict),
            &expected_id,
            &expected_started_at,
        );

        let starting = pending_create_with_issued_identifier().await;
        let expected_id = starting.id.clone();
        let expected_started_at = starting.pending.started_at().clone();
        let verdict: wasm_convert::DecodedPollVerdict =
            proto::PollVerdict::Undetermined(ProviderError::internal("undetermined"));
        assert_original_polled_record(
            WasmProvider::polled_create_verdict(starting, verdict),
            &expected_id,
            &expected_started_at,
        );

        let identifier_variants = [
            proto::PendingCreate {
                token: proto::PendingToken::new("replacement-token"),
                identifier: None,
                sent: sent_name_only(),
                poll_after_secs: 2,
            },
            proto::PendingCreate {
                token: proto::PendingToken::new("replacement-token"),
                identifier: Some(AppliedIdentifier::new("changed-id").expect("valid identifier")),
                sent: sent_name_only(),
                poll_after_secs: 2,
            },
        ];
        for next in identifier_variants {
            let starting = pending_create_with_issued_identifier().await;
            let step: wasm_convert::DecodedCreateStep = proto::CreateStep::Pending(next);
            assert_issued_poll_repended_with_original_identifier(WasmProvider::polled_create_step(
                starting, step,
            ));
        }

        let starting = pending_create_with_issued_identifier().await;
        let expected_id = starting.id.clone();
        let expected_started_at = starting.pending.started_at().clone();
        let changed_sent: wasm_convert::DecodedCreateStep =
            proto::CreateStep::Pending(proto::PendingCreate {
                token: proto::PendingToken::new("replacement-token"),
                identifier: Some(AppliedIdentifier::new("issued-id").expect("valid identifier")),
                sent: Sent::AllRequestAttributes,
                poll_after_secs: 2,
            });
        assert_original_polled_record(
            WasmProvider::polled_create_step(starting, changed_sent),
            &expected_id,
            &expected_started_at,
        );
    }

    #[tokio::test]
    async fn poll_decode_failure_after_identifier_is_created_not_untracked() {
        let starting = pending_create_with_issued_identifier().await;
        let failure = decode_failed_create_step(&starting.id);
        assert_created_after_decode_failure(WasmProvider::polled_step_created_read_decode_failure(
            starting, failure,
        ));

        let starting = pending_create_with_issued_identifier().await;
        let failure = decode_failed_created_verdict(&starting.id);
        assert_created_after_decode_failure(
            WasmProvider::polled_verdict_created_read_decode_failure(starting, failure),
        );
    }

    #[tokio::test]
    async fn resume_decode_failure_after_identifier_is_created_not_ended() {
        let (id, _wit_id, wit_request) = boundary_resource();
        let starting = persisted_resume_with_issued_identifier();
        let resume = resume_witness(starting).await;
        let step = wit_types::ResumeStep::Completed(wit_types::AppliedState {
            identifier: "decoded-created-id".to_string(),
            attributes: malformed_wit_attributes(),
        });
        let failure = match wasm_convert::wit_to_core_resume_step(step, &id)
            .expect("the valid identifier makes the attribute failure recoverable")
        {
            wasm_convert::DecodedIssued::CreatedReadFailed(failure) => failure,
            wasm_convert::DecodedIssued::Decoded(_) => {
                panic!("malformed attributes unexpectedly decoded")
            }
        };

        let progress =
            WasmProvider::resumed_created_read_decode_failure(id, wit_request, resume, failure);
        assert_created_after_resume_decode_failure(progress);

        let (id, _wit_id, wit_request) = boundary_resource();
        let starting = persisted_resume_with_issued_identifier();
        let resume = resume_witness(starting).await;
        let declared_error = ProviderError::internal("guest resume failure");
        let verdict = wit_types::ResumeVerdict::Created(wit_types::ResumedFailure {
            identifier: "decoded-created-id".to_string(),
            read: wit_types::Observation::Read(wit_types::ReadOutcome::Found(
                wit_types::AppliedState {
                    identifier: "decoded-created-id".to_string(),
                    attributes: malformed_wit_attributes(),
                },
            )),
            error: wasm_convert::wire_to_wit_provider_error(declared_error.to_wire()),
        });
        let failure = match wasm_convert::wit_to_core_resume_verdict(verdict, &id)
            .expect("the valid identifier makes the verdict observation failure recoverable")
        {
            wasm_convert::DecodedIssued::CreatedReadFailed(failure) => failure,
            wasm_convert::DecodedIssued::Decoded(_) => {
                panic!("malformed resume verdict observation unexpectedly decoded")
            }
        };
        let progress =
            WasmProvider::resumed_created_read_decode_failure(id, wit_request, resume, failure);
        assert_created_after_resume_decode_failure(progress);
    }

    #[tokio::test]
    async fn issued_resume_boundary_never_drops_or_mutates_the_starting_record() {
        let (id, _wit_id, wit_request) = boundary_resource();
        let starting = persisted_resume_with_issued_identifier();
        let resume = resume_witness(starting.clone()).await;
        let verdict: wasm_convert::DecodedResumeVerdict =
            proto::ResumeVerdict::Untracked(ProviderError::internal("untracked"));
        assert_original_resumed_record(
            WasmProvider::resumed_create_verdict(id, &wit_request, resume, verdict),
            &starting,
        );

        let (id, _wit_id, wit_request) = boundary_resource();
        let starting = persisted_resume_with_issued_identifier();
        let resume = resume_witness(starting.clone()).await;
        let verdict: wasm_convert::DecodedResumeVerdict =
            proto::ResumeVerdict::Undetermined(ProviderError::internal("undetermined"));
        assert_original_resumed_record(
            WasmProvider::resumed_create_verdict(id, &wit_request, resume, verdict),
            &starting,
        );

        let identifier_variants = [
            proto::ResumePending {
                token: proto::PendingToken::new("replacement-token"),
                identifier: None,
                poll_after_secs: 2,
            },
            proto::ResumePending {
                token: proto::PendingToken::new("replacement-token"),
                identifier: Some(AppliedIdentifier::new("changed-id").expect("valid identifier")),
                poll_after_secs: 2,
            },
        ];
        for next in identifier_variants {
            let (id, wit_id, wit_request) = boundary_resource();
            let resume = resume_witness(persisted_resume_with_issued_identifier()).await;
            let step: wasm_convert::DecodedResumeStep = proto::ResumeStep::Pending(next);
            assert_issued_resume_repended_with_original_identifier(
                WasmProvider::resumed_create_step(id, wit_id, wit_request, resume, step),
            );
        }
    }

    fn migrate_legacy_state_attribute(
        value: serde_json::Value,
        display_type: &str,
        identity: &str,
        key: &str,
    ) -> serde_json::Value {
        let mut attributes = serde_json::Map::new();
        attributes.insert(key.to_string(), value);
        let mut document = serde_json::json!({
            "version": 13,
            "resources": [{
                "provider": "",
                "resource_type": display_type,
                "identity": identity,
                "attributes": attributes,
                "deposed": [],
            }],
        });
        carina_core::resource::legacy_secret_migration::migrate_state_document_v13(&mut document)
            .expect("migrate legacy test state document");
        document["resources"][0]["attributes"]
            .as_object_mut()
            .and_then(|attributes| attributes.remove(key))
            .expect("migrated test document must retain its attribute")
    }

    fn assert_value_structure_eq(actual: &Value, expected: &Value) {
        match (actual, expected) {
            (
                Value::Deferred(DeferredValue::Unknown(actual)),
                Value::Deferred(DeferredValue::Unknown(expected)),
            ) => assert_eq!(actual, expected),
            (
                Value::Concrete(ConcreteValue::List(actual)),
                Value::Concrete(ConcreteValue::List(expected)),
            ) => {
                assert_eq!(actual.len(), expected.len());
                for (actual, expected) in actual.iter().zip(expected) {
                    assert_value_structure_eq(actual, expected);
                }
            }
            (
                Value::Concrete(ConcreteValue::Map(actual)),
                Value::Concrete(ConcreteValue::Map(expected)),
            ) => {
                assert_eq!(
                    actual.keys().collect::<Vec<_>>(),
                    expected.keys().collect::<Vec<_>>()
                );
                for (key, expected) in expected {
                    assert_value_structure_eq(&actual[key], expected);
                }
            }
            (
                Value::Deferred(DeferredValue::Interpolation(actual)),
                Value::Deferred(DeferredValue::Interpolation(expected)),
            ) => {
                assert_eq!(actual.len(), expected.len());
                for (actual, expected) in actual.iter().zip(expected) {
                    match (actual, expected) {
                        (
                            InterpolationPart::Literal(actual),
                            InterpolationPart::Literal(expected),
                        ) => assert_eq!(actual, expected),
                        (InterpolationPart::Expr(actual), InterpolationPart::Expr(expected)) => {
                            assert_value_structure_eq(actual, expected)
                        }
                        _ => panic!("interpolation part variants differ"),
                    }
                }
            }
            (
                Value::Deferred(DeferredValue::FunctionCall {
                    name: actual_name,
                    args: actual_args,
                    ..
                }),
                Value::Deferred(DeferredValue::FunctionCall {
                    name: expected_name,
                    args: expected_args,
                    ..
                }),
            ) => {
                assert_eq!(actual_name, expected_name);
                assert_eq!(actual_args.len(), expected_args.len());
                for (actual, expected) in actual_args.iter().zip(expected_args) {
                    assert_value_structure_eq(
                        actual.value().expect("serialized function argument value"),
                        expected
                            .value()
                            .expect("serialized function argument value"),
                    );
                }
            }
            (
                Value::Deferred(DeferredValue::Access(actual_access)),
                Value::Deferred(DeferredValue::Access(expected_access)),
            ) => {
                assert_eq!(actual_access.step(), expected_access.step());
                assert_value_structure_eq(actual_access.base(), expected_access.base());
            }
            (
                Value::Deferred(DeferredValue::Secret(actual)),
                Value::Deferred(DeferredValue::Secret(expected)),
            ) => assert_value_structure_eq(actual, expected),
            _ => assert_eq!(actual, expected),
        }
    }

    #[tokio::test]
    async fn wasm_serializable_resources_restores_original_attribute_order() {
        let mut resource = Resource::new("test.t", "n");
        resource.set_attr("before", test_string_value("before"));
        resource.set_attr(
            "pending",
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex)),
        );
        resource.set_attr("middle", test_string_value("middle"));
        resource.set_attr("peer", Value::resource_ref("peer", "identifier", vec![]));
        resource.set_attr("after", test_string_value("after"));
        let mut resources = vec![resource];
        let original_attributes = resources[0].attributes.clone();

        with_wasm_serializable_resources(&mut resources, |resources| {
            Box::pin(async move {
                assert_eq!(
                    resources[0].attributes.keys().collect::<Vec<_>>(),
                    vec!["before", "middle", "after"]
                );
                resources[0].set_attr("guest_added", test_string_value("new"));
                Ok(())
            })
        })
        .await
        .unwrap();

        assert_eq!(
            resources[0].attributes.keys().collect::<Vec<_>>(),
            vec![
                "before",
                "pending",
                "middle",
                "peer",
                "after",
                "guest_added"
            ],
            "restoration must preserve original positions and trail guest-added attributes"
        );
        for (key, value) in original_attributes {
            assert_value_structure_eq(
                resources[0]
                    .get_attr(&key)
                    .unwrap_or_else(|| panic!("attribute {key} must be restored")),
                &value,
            );
        }
    }

    #[tokio::test]
    async fn wasm_serializable_resources_restores_duplicate_resource_ids_by_position() {
        let first_hidden = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex));
        let second_hidden = Value::resource_ref("peer", "identifier", vec![]);

        let mut first = Resource::new("test.t", "shared");
        first.set_attr("visible", test_string_value("first"));
        first.set_attr("hidden", first_hidden.clone());

        let mut second = Resource::new("test.t", "shared");
        second.set_attr("visible", test_string_value("second"));
        second.set_attr("hidden", second_hidden.clone());

        assert_eq!(first.id, second.id, "the regression requires duplicate IDs");
        let mut resources = vec![first, second];

        with_wasm_serializable_resources(&mut resources, |resources| {
            Box::pin(async move {
                for resource in resources {
                    assert_eq!(
                        resource.attributes.keys().collect::<Vec<_>>(),
                        vec!["visible"]
                    );
                }
                Ok(())
            })
        })
        .await
        .unwrap();

        assert_value_structure_eq(
            resources[0]
                .get_attr("hidden")
                .expect("the first resource's attribute must be restored"),
            &first_hidden,
        );
        assert_value_structure_eq(
            resources[1]
                .get_attr("hidden")
                .expect("the second resource's attribute must be restored"),
            &second_hidden,
        );
    }

    #[tokio::test]
    async fn wasm_serializable_resources_strips_values_in_nested_wrappers() {
        let upstream_path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        let unknown = || {
            Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
                path: upstream_path.clone(),
            }))
        };
        let resource_ref = || Value::resource_ref("admins", "group_id", vec![]);

        let mut resource = Resource::new("test.t", "n");
        resource.set_attr("stable", test_string_value("visible"));
        resource.set_attr(
            "unknown_list",
            Value::Concrete(ConcreteValue::List(vec![unknown()])),
        );
        resource.set_attr(
            "unknown_map",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "nested".to_string(),
                unknown(),
            )]))),
        );
        resource.set_attr(
            "unknown_interpolation",
            Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
                unknown(),
            )])),
        );
        resource.set_attr(
            "unknown_function",
            Value::Deferred(DeferredValue::FunctionCall {
                name: carina_core::builtins::BuiltinFunction::upper,
                args: vec![unknown().into()],
                source_line: 0.into(),
            }),
        );
        resource.set_attr(
            "unknown_secret",
            Value::Deferred(DeferredValue::Secret(Box::new(unknown()))),
        );
        resource.set_attr(
            "ref_map",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "nested".to_string(),
                resource_ref(),
            )]))),
        );
        resource.set_attr(
            "ref_interpolation",
            Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
                resource_ref(),
            )])),
        );
        resource.set_attr(
            "ref_function",
            Value::Deferred(DeferredValue::FunctionCall {
                name: carina_core::builtins::BuiltinFunction::upper,
                args: vec![resource_ref().into()],
                source_line: 0.into(),
            }),
        );
        resource.set_attr(
            "ref_secret",
            Value::Deferred(DeferredValue::Secret(Box::new(resource_ref()))),
        );
        let mut resources = vec![resource];
        let original_attributes = resources[0].attributes.clone();

        with_wasm_serializable_resources(&mut resources, |resources| {
            Box::pin(async move {
                assert_eq!(
                    resources[0].attributes.keys().collect::<Vec<_>>(),
                    vec!["stable"],
                    "the WASM transport operation must see only serializable attributes"
                );
                Ok(())
            })
        })
        .await
        .unwrap();

        assert_eq!(
            resources[0].attributes.keys().collect::<Vec<_>>(),
            original_attributes.keys().collect::<Vec<_>>()
        );
        for (key, value) in original_attributes {
            assert_value_structure_eq(
                resources[0]
                    .get_attr(&key)
                    .unwrap_or_else(|| panic!("attribute {key} must be restored")),
                &value,
            );
        }
    }

    #[tokio::test]
    async fn wasm_serializable_resources_restores_attributes_on_error() {
        let mut resource = Resource::new("test.t", "n");
        resource.set_attr("stable", test_string_value("visible"));
        resource.set_attr("peer", Value::resource_ref("peer", "identifier", vec![]));
        resource.set_attr(
            "pending",
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex)),
        );
        let mut resources = vec![resource];
        let before = resources[0].attributes.clone();

        let error = with_wasm_serializable_resources(&mut resources, |resources| {
            Box::pin(async move {
                assert_eq!(
                    resources[0].attributes.keys().collect::<Vec<_>>(),
                    vec!["stable"]
                );
                Err(ProviderError::internal("boundary failed"))
            })
        })
        .await
        .expect_err("the boundary error must propagate");

        assert_eq!(error.detail().context(), "boundary failed");
        assert_eq!(
            resources[0].attributes.keys().collect::<Vec<_>>(),
            before.keys().collect::<Vec<_>>(),
            "restored attributes must retain their original order"
        );
        for (key, value) in before {
            assert_value_structure_eq(
                resources[0]
                    .get_attr(&key)
                    .unwrap_or_else(|| panic!("attribute {key} must be restored")),
                &value,
            );
        }
    }

    #[test]
    fn malformed_config_completions_metadata_is_rejected() {
        let error = decode_config_completions_json("not json", "mock", "1.0.0")
            .expect_err("malformed config completions must fail provider loading");

        let rendered = error.to_string();
        assert!(
            rendered.contains("provider config completions"),
            "{rendered}"
        );
        assert!(rendered.contains("expected ident"), "{rendered}");
        assert!(rendered.contains("line 1 column 2"), "{rendered}");
        assert_eq!(rendered.matches("expected ident").count(), 1, "{rendered}");
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn shared_trap_mapper_preserves_provider_operation_messages() {
        let trap = wasm_trap_provider_error("read", wasmtime::Error::msg("low-level trap"));
        assert_eq!(trap.detail().context(), "WASM guest trapped");
        assert_eq!(
            trap.to_string(),
            "WASM guest trapped: read call failed: low-level trap"
        );
        assert!(trap.detail().cause().is_some());
        assert!(std::error::Error::source(&trap).is_none());

        let timeout = wasm_trap_provider_error(
            "read",
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while executing at wasm backtrace:\n    0: test-frame"),
        );
        assert_eq!(timeout.detail().context(), "WASM plugin timed out");
        let rendered = timeout.to_string();
        assert!(rendered.contains("read exceeded the guest compute budget"));
        assert!(!rendered.to_lowercase().contains("credential"));

        let normalizer_timeout = wasm_trap_provider_error(
            "normalize_state",
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while executing at wasm backtrace:\n    0: test-frame"),
        );
        assert_eq!(
            normalizer_timeout.detail().context(),
            "WASM plugin timed out"
        );
        let rendered = normalizer_timeout.to_string();
        assert!(rendered.contains("normalize_state exceeded the guest compute budget"));
        assert!(!rendered.to_lowercase().contains("credential"));
    }

    #[test]
    fn timeout_classification_uses_the_typed_trap_not_message_text() {
        let nested = wasmtime::Error::msg("epoch deadline reached").context("outer guest trap");

        let error = wasm_trap_provider_error("read", nested);

        assert_eq!(error.variant_name(), "internal");
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

        let wasm_bytes = std::fs::read(wasm_path).expect("read mock WASM provider");
        let factory = WasmProviderFactory::new_uncached(&wasm_bytes)
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
        let locked = instance
            .acquire("normalize_desired")
            .await
            .expect("fresh instance should be usable");
        let malformed = [malformed];
        let result = locked
            .call("normalize_desired", async |bindings, store| {
                bindings.call_normalize_desired_raw(store, &malformed).await
            })
            .await
            .expect("the guest call should return its provider error");
        let error = result
            .map_err(wasm_convert::wit_to_core_provider_error)
            .expect_err("malformed guest input must return provider-error");

        assert_eq!(error.variant_name(), "internal");
        assert_eq!(
            error.detail().operation.as_deref(),
            Some("normalize_desired"),
            "the guest boundary error must retain its export operation"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains("normalize_desired"),
            "the guest boundary error message must name its export: {error}"
        );
        assert!(
            rendered.contains("WASM boundary decode error"),
            "the guest boundary error message must be preserved: {error}"
        );
        assert!(
            rendered.contains("invalid JSON"),
            "the guest decode detail must be preserved: {error}"
        );
        assert!(
            matches!(&*instance.slot.lock().await, InstanceSlot::Live(_)),
            "a returned boundary provider-error must not poison the instance"
        );

        let malformed_saved_attrs = [(
            "mock.test.resource.malformed-boundary".to_string(),
            vec![(
                "settings".to_string(),
                wit_types::PersistedValue::ListVal("not-json".to_string()),
            )],
        )];
        let locked = instance
            .acquire("hydrate_read_state")
            .await
            .expect("the decode error must leave the instance usable");
        let result = locked
            .call("hydrate_read_state", async |bindings, store| {
                bindings
                    .call_hydrate_read_state(store, &[], &malformed_saved_attrs)
                    .await
            })
            .await
            .expect("the guest call should return its provider error");
        let error = result
            .map_err(wasm_convert::wit_to_core_provider_error)
            .expect_err("malformed saved attributes must return provider-error");

        assert_eq!(error.variant_name(), "internal");
        assert_eq!(
            error.detail().operation.as_deref(),
            Some("hydrate_read_state"),
            "the structured operation must be the bare export name"
        );
        assert!(
            error
                .to_string()
                .contains("hydrate_read_state saved attributes"),
            "the boundary error message must retain its failing sub-part: {error}"
        );
        assert!(
            matches!(&*instance.slot.lock().await, InstanceSlot::Live(_)),
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
        let decode_error =
            wasm_convert::wit_to_core_value(&wit_types::Value::ListVal("[".to_string()))
                .expect_err("malformed JSON must fail");
        let serde_detail = "EOF while parsing a list at line 1 column 1";
        let rendered = wasm_value_decode_provider_error("read", decode_error).to_string();

        assert!(rendered.contains("in operation read"), "{rendered}");
        assert_eq!(rendered.matches(&serde_detail).count(), 1, "{rendered}");
    }

    #[test]
    fn malformed_normalized_state_output_is_atomic() {
        use wit_types::PersistedValue as WitValue;

        let first_id = Resource::with_provider("mock", "test.resource", "first", None).id;
        let second_id = Resource::with_provider("mock", "test.resource", "second", None).id;
        let original_value = Value::Concrete(carina_core::resource::ConcreteValue::String(
            "original".into(),
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
        assert!(error.detail().cause().is_some());
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
    fn normalized_state_roundtrip_preserves_self_contained_legacy_secret() {
        use argon2::Argon2;
        use wit_types::PersistedValue as WitValue;

        let id = Resource::with_provider("mock", "test.resource", "legacy", None).id;
        let material = b"\"wire-legacy\"";
        let salt = b"carina:mock.test.resource:legacy:comment";
        let mut output = [0_u8; 32];
        Argon2::default()
            .hash_password_into(material, salt, &mut output)
            .expect("hash legacy state fixture");
        let encoded = migrate_legacy_state_attribute(
            serde_json::json!(format!("_secret:argon2:{}", hex::encode(output))),
            "mock.test.resource",
            "legacy",
            "comment",
        );
        let digest = encoded[carina_core::value::PERSISTED_SECRET_KEY]
            .as_str()
            .expect("migration produces a structural secret")
            .to_string();
        let hash = carina_core::resource::RedactedSecretHash::from_encoded_digest(&digest)
            .expect("migration produces a valid digest");
        let attributes = HashMap::from([(
            "comment".to_string(),
            Value::Deferred(DeferredValue::RedactedSecret { hash }),
        )]);
        let mut states = HashMap::from([(id.clone(), State::existing(id.clone(), attributes))]);
        apply_wit_state_attribute_updates(
            "normalize_state",
            &mut states,
            vec![(
                id.clone(),
                vec![("comment".to_string(), WitValue::RedactedSecretVal(digest))],
            )],
        )
        .expect("apply normalized state");

        let Some(Value::Deferred(DeferredValue::RedactedSecret { hash })) =
            states[&id].attributes.get("comment")
        else {
            panic!("normalized legacy marker must remain a typed secret hash")
        };
        assert!(
            hash.verify(material),
            "the normalizer roundtrip must retain the marker's explicit salt"
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
        let state = wit_types::PersistedState {
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
        let error = provider_schema_decode_error("bad-provider", "9.9.9", detail);
        let err = error.to_string();

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
        let mut store = GuestStore::new(
            &engine,
            wasi_ctx,
            None,
            build_store_limits(),
            GuestCallBudget::PRODUCTION,
        );
        // This will panic if epoch_interruption is not enabled on the engine.
        store.set_epoch_deadline_for_test(1);
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
        let mut store = GuestStore::new(
            &engine,
            wasi_ctx,
            None,
            build_store_limits(),
            GuestCallBudget::PRODUCTION,
        );
        store.set_epoch_deadline_for_test(1);

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
    fn test_guest_compute_budget_is_reasonable() {
        // The budget should be long enough for normal operations but short
        // enough to prevent indefinite hangs.
        const {
            assert!(
                GuestCallBudget::PRODUCTION.compute().as_secs() >= 10,
                "compute budget too short for normal operations"
            );
            assert!(
                GuestCallBudget::PRODUCTION.compute().as_secs() <= 120,
                "compute budget too long to be useful"
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
        let key1 = WasmProviderFactory::cache_key(b"content_v1");
        let key2 = WasmProviderFactory::cache_key(b"content_v2");

        assert_ne!(
            key1, key2,
            "cache key should change when file content changes"
        );
    }

    #[test]
    fn test_cache_key_stable_for_same_content() {
        let key1 = WasmProviderFactory::cache_key(b"same_content");
        let key2 = WasmProviderFactory::cache_key(b"same_content");

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
            GuestCallBudget::PRODUCTION,
        )
        .await
        {
            Ok(_) => panic!("the unresolved bogus import must reject instantiation"),
            Err(error) => error,
        };
        let WasmProviderInstanceError::Instantiation(instantiation) = error else {
            panic!("expected instantiation error")
        };
        let rendered = instantiation.to_string();

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

        let error = match create_runtime_instance(
            &engine,
            &component,
            None,
            InstantiationWorld::Basic,
            GuestCallBudget::PRODUCTION,
        )
        .await
        {
            Ok(_) => panic!("the basic linker must reject the wasi:http import"),
            Err(error) => error,
        };
        let WasmProviderInstanceError::Instantiation(instantiation) = error else {
            panic!("expected instantiation error")
        };
        let rendered = instantiation.to_string();

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
        let cause = provider_error
            .detail()
            .cause()
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
        assert_eq!(provider_error.detail().context(), "WASM plugin timed out");
        let provider_rendered = provider_error.to_string();
        assert!(provider_rendered.contains("info exceeded the guest compute budget"));
        assert!(!provider_rendered.to_lowercase().contains("credential"));
        let cause = provider_error
            .detail()
            .cause()
            .expect("info timeout must retain the typed guest-call cause");
        assert!(cause.downcast_ref::<WasmGuestCallError>().is_some());
        let rendered = cause.to_string();
        assert!(
            rendered.contains("error while calling provider info"),
            "{rendered}"
        );
        assert_eq!(
            rendered
                .matches("error while calling provider info")
                .count(),
            1,
            "{rendered}"
        );
        assert!(cause.source().is_none());
    }

    #[tokio::test]
    async fn runtime_wall_timeout_keeps_typed_budget_source_without_duplicate_text() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let wasi_ctx = WasiCtxBuilder::new().build();
        let mut store = GuestStore::new(
            &engine,
            wasi_ctx,
            None,
            build_store_limits(),
            GuestCallBudget::new(
                std::time::Duration::from_secs(30),
                std::time::Duration::ZERO,
            ),
        );
        let source = store
            .call(|_store| std::future::pending::<()>())
            .await
            .expect_err("a zero wall budget must time out");
        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let provider_error = WasmProviderInstanceError::WallBudgetExceeded {
            operation: "initialize",
            source,
        }
        .into_provider_error_with(&mapper);

        assert_eq!(provider_error.variant_name(), "timeout");
        let operation_cause = provider_error
            .detail()
            .cause()
            .expect("wall timeout must retain an operation-aware typed cause");
        assert!(
            operation_cause
                .downcast_ref::<WasmWallBudgetError>()
                .is_some(),
            "unexpected cause type: {operation_cause:?}"
        );
        let budget_cause = operation_cause
            .source()
            .expect("operation-aware timeout must retain WallBudgetExceeded");
        assert!(budget_cause.downcast_ref::<WallBudgetExceeded>().is_some());

        let rendered = provider_error.to_string();
        assert!(
            rendered.contains("WASM guest call 'initialize' timed out"),
            "{rendered}"
        );
        assert!(rendered.contains("wall-clock budget"), "{rendered}");
        assert_eq!(
            rendered.matches("wall-clock budget").count(),
            1,
            "{rendered}"
        );
        assert!(
            rendered.contains("check network connectivity"),
            "{rendered}"
        );
        assert!(
            !rendered.to_lowercase().contains("credential"),
            "{rendered}"
        );
    }

    #[test]
    fn initialization_value_encode_failure_keeps_typed_internal_error() {
        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let provider_error =
            WasmProviderInstanceError::InitializeValueEncode(SerializationError::NonFiniteFloat {
                value: f64::NAN,
                context: carina_core::value::SerializationContext::WasmBoundary,
            })
            .into_provider_error_with(&mapper);

        assert_eq!(provider_error.variant_name(), "internal");
        assert_eq!(
            provider_error.detail().context(),
            "failed to encode a WASM provider boundary value"
        );
        assert_eq!(
            provider_error.to_string(),
            "failed to encode a WASM provider boundary value: in operation initialize: cannot serialize at WASM provider boundary: non-finite float NaN"
        );
    }

    #[test]
    fn initialization_call_interrupt_uses_guest_timeout_classification() {
        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let provider_error = WasmProviderInstanceError::InitializeCall(
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while calling provider initialize"),
        )
        .into_provider_error_with(&mapper);

        assert_eq!(provider_error.variant_name(), "timeout");
        assert_eq!(provider_error.detail().context(), "WASM plugin timed out");
        assert_eq!(
            provider_error.to_string(),
            "WASM plugin timed out: initialize exceeded the guest compute budget: error while calling provider initialize: wasm trap: interrupt"
        );
        assert!(
            !provider_error
                .to_string()
                .to_lowercase()
                .contains("credential")
        );
    }

    #[test]
    fn initialization_error_preserves_decoded_provider_error_details() {
        let original = ProviderError::api_error_caused_by(
            "Initialization request failed",
            std::io::Error::other("credential service rejected the request"),
        )
        .with_operation("sts.GetCallerIdentity")
        .with_status(403)
        .with_code("AccessDenied")
        .with_request_id("request-123");
        let decoded = wasm_convert::decode_provider_initialization_error(original.to_wire());
        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let mapped = WasmProviderInstanceError::Initialize(decoded)
            .into_provider_error_with(&mapper)
            .for_provider("awscc");

        assert!(matches!(mapped, ProviderError::ApiError(_)));
        let detail = mapped.detail();
        assert_eq!(detail.provider_name.as_deref(), Some("awscc"));
        assert_eq!(detail.operation.as_deref(), Some("sts.GetCallerIdentity"));
        assert_eq!(detail.status, Some(403));
        assert_eq!(detail.code.as_deref(), Some("AccessDenied"));
        assert_eq!(detail.request_id.as_deref(), Some("request-123"));
        assert_eq!(
            mapped.to_string(),
            "Initialization request failed\n  operation: sts.GetCallerIdentity\n  status: 403 AccessDenied\n  request_id: request-123\n  cause: credential service rejected the request"
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
            ProviderError::internal_caused_by("artifact provenance attached", error)
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

    #[test]
    fn cancellation_poison_fails_fast_naming_the_rejected_operation() {
        let guard_err = PoisonReason::Cancelled.fail_fast_error("delete");
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

    #[tokio::test]
    async fn returned_trap_poison_is_classified_inside_call() {
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

        let wasm_bytes = std::fs::read(wasm_path).expect("read mock WASM provider");
        let factory = WasmProviderFactory::new_uncached(&wasm_bytes)
            .await
            .expect("mock WASM provider should load");
        let instance = factory
            .get_or_create_shared_instance(None, &IndexMap::new())
            .await
            .expect("mock WASM provider should initialize");

        let locked = instance
            .acquire("normalize_state")
            .await
            .expect("fresh instance should be usable");
        let call_error = locked
            .call("normalize_state", async |_bindings, _store| {
                Err::<(), _>(wasmtime::Error::new(wasmtime::Trap::UnreachableCodeReached))
            })
            .await
            .expect_err("the returned trap must fail the guest call");
        assert_eq!(call_error.detail().context(), "WASM guest trapped");

        let next_error = instance
            .acquire("create")
            .await
            .err()
            .expect("the returned trap must poison the slot before call returns");
        let rendered = next_error.to_string();
        assert!(
            rendered.contains("WASM provider instance unusable after trap in normalize_state"),
            "the fail-fast error must retain the trap origin: {rendered}"
        );
        assert!(
            rendered.contains("operation 'create'"),
            "the fail-fast error must name the rejected operation: {rendered}"
        );
    }

    #[test]
    fn trap_poison_names_the_origin_and_keeps_only_the_structured_cause() {
        let trap = wasmtime::Error::new(wasmtime::Trap::UnreachableCodeReached)
            .context("error while executing at wasm backtrace:\n    0: test-frame");
        let reason = PoisonReason::from_trap("normalize_state", &trap);
        let error = reason.fail_fast_error("create");
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

    #[tokio::test]
    async fn rebuild_failure_is_typed_leaves_slot_poisoned_and_retries() {
        let config = build_engine_config();
        let engine = Engine::new(&config).expect("test engine should be valid");
        let component =
            Component::new(&engine, "(component)").expect("empty test component should compile");
        let instance = RebuildableInstance {
            slot: Mutex::new(InstanceSlot::Poisoned {
                reason: PoisonReason::WallClockTimeout {
                    operation: "validate_config",
                },
            }),
            rebuild: RebuildContext {
                engine,
                component,
                budget: GuestCallBudget::PRODUCTION,
            },
        };

        for attempt in 1..=2 {
            let error = match instance.acquire("validate_config").await {
                Ok(_) => panic!("the empty component rebuild must fail"),
                Err(error) => error,
            };
            assert_eq!(error.variant_name(), "internal");
            assert_eq!(
                error.detail().context(),
                "Failed to rebuild WASM provider validation instance"
            );
            let cause = error
                .detail()
                .cause()
                .expect("rebuild failure must retain its typed load error");
            let load_error = cause
                .downcast_ref::<WasmProviderLoadError>()
                .expect("rebuild failure cause must remain WasmProviderLoadError");
            let WasmProviderLoadError::Instantiation(instantiation) = load_error else {
                panic!("empty component must produce an instantiation error: {load_error:?}")
            };
            assert!(
                matches!(
                    instantiation.context.as_ref(),
                    InstantiationContext::HttpThenBasic { .. }
                ),
                "rebuild attempt {attempt} must preserve the real dual-attempt context"
            );
            let source_chain: Vec<_> = instantiation
                .primary_failure
                .chain()
                .map(ToString::to_string)
                .collect();
            assert!(
                source_chain.len() >= 2,
                "rebuild attempt {attempt} must retain Wasmtime's source chain: {source_chain:?}"
            );
            assert!(
                matches!(
                    &*instance.slot.lock().await,
                    InstanceSlot::Poisoned {
                        reason: PoisonReason::WallClockTimeout {
                            operation: "validate_config"
                        }
                    }
                ),
                "a failed rebuild must leave the original poisoned slot intact"
            );
        }
    }

    #[test]
    fn production_wall_budget_exceeds_each_provider_single_call_worst_case() {
        fn retry_backoff_total(
            max_attempts: u32,
            initial_delay: std::time::Duration,
            max_delay: std::time::Duration,
        ) -> std::time::Duration {
            (1..max_attempts).fold(std::time::Duration::ZERO, |total, attempt| {
                total
                    + initial_delay
                        .saturating_mul(2_u32.pow(attempt - 1))
                        .min(max_delay)
            })
        }

        // Mirrors carina-provider-aws/src/helpers.rs RetryPolicy::default()
        // on origin/main.
        // retry_aws_operation runs a complete SDK operation per outer attempt
        // and treats the SDK's TimeoutError as retryable.
        const AWS_RETRY_POLICY_MAX_ATTEMPTS: u32 = 8;
        const AWS_RETRY_POLICY_INITIAL_DELAY: std::time::Duration =
            std::time::Duration::from_secs(5);
        const AWS_RETRY_POLICY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(120);

        // Mirrors the bounded policy in both carina-provider-aws/src/sdk_config.rs
        // and carina-provider-awscc/src/sdk_config.rs on origin/main.
        const AWS_SDK_CONNECT_TIMEOUT: std::time::Duration =
            std::time::Duration::from_millis(3_100);
        const AWS_SDK_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        const AWS_SDK_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

        // Mirrors carina-provider-aws/src/services/ec2/flow_log.rs on
        // origin/main: up to 12 send() attempts with a 5s IAM-propagation
        // sleep between attempts, followed by one read_ec2_flow_log SDK
        // operation in the same create call.
        const AWS_FLOW_LOG_SEND_ATTEMPTS: u32 = 12;
        const AWS_FLOW_LOG_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

        // Mirrors carina-provider-aws/src/services/acm/certificate.rs on
        // origin/main: one RequestCertificate send(), then six
        // wait_for_dvo_populated send() attempts at a 5s interval.
        const AWS_ACM_REQUEST_CERTIFICATE_SENDS: u32 = 1;
        const AWS_ACM_DVO_WAIT_ATTEMPTS: u32 = 6;
        const AWS_ACM_DVO_WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

        let bounded_attempt_worst_case = AWS_SDK_CONNECT_TIMEOUT.min(HTTP_API_REQUEST_TIMEOUT)
            + AWS_SDK_READ_TIMEOUT.min(HTTP_API_REQUEST_TIMEOUT);
        assert_eq!(
            bounded_attempt_worst_case,
            std::time::Duration::from_millis(33_100)
        );
        // In WASM, the operation-attempt and operation deadlines are observed
        // only between attempts. The attempt deadline therefore does not
        // tighten this bound, and one SDK operation can overrun its operation
        // timeout by one bounded connect+read attempt.
        let bounded_sdk_operation_worst_case =
            AWS_SDK_OPERATION_TIMEOUT + bounded_attempt_worst_case;
        assert_eq!(
            bounded_sdk_operation_worst_case,
            std::time::Duration::from_millis(93_100)
        );

        let aws_outer_backoff = retry_backoff_total(
            AWS_RETRY_POLICY_MAX_ATTEMPTS,
            AWS_RETRY_POLICY_INITIAL_DELAY,
            AWS_RETRY_POLICY_MAX_DELAY,
        );
        assert_eq!(aws_outer_backoff, std::time::Duration::from_secs(395));
        let aws_retry_operation_worst_case =
            bounded_sdk_operation_worst_case * AWS_RETRY_POLICY_MAX_ATTEMPTS + aws_outer_backoff;

        let aws_flow_log_worst_case = bounded_sdk_operation_worst_case
            * (AWS_FLOW_LOG_SEND_ATTEMPTS + 1)
            + AWS_FLOW_LOG_RETRY_INTERVAL * (AWS_FLOW_LOG_SEND_ATTEMPTS - 1);
        assert_eq!(
            aws_flow_log_worst_case,
            std::time::Duration::from_millis(1_265_300)
        );

        let aws_acm_dns_validation_worst_case = bounded_sdk_operation_worst_case
            * AWS_ACM_REQUEST_CERTIFICATE_SENDS
            + bounded_sdk_operation_worst_case * AWS_ACM_DVO_WAIT_ATTEMPTS
            + AWS_ACM_DVO_WAIT_INTERVAL * (AWS_ACM_DVO_WAIT_ATTEMPTS - 1);

        // carina-provider-awscc/src/provider/cloudcontrol.rs `finish_issue`
        // and `poll_phase` on origin/main return retries as `poll_after_secs`
        // on a pending step, so one AWSCC guest call is one SDK operation.
        let awscc_guest_call_worst_case = bounded_sdk_operation_worst_case;

        // A host patch release must still run provider releases predating the
        // bounded SDK configuration. Their standard SDK policy used three
        // attempts with 1s + 2s backoff, while wasi:http could consume the
        // host cap once for connect and once for first byte per attempt.
        const LEGACY_AWS_SDK_MAX_ATTEMPTS: u32 = 3;
        const LEGACY_AWS_SDK_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
        const LEGACY_AWS_SDK_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
        let legacy_sdk_backoff = retry_backoff_total(
            LEGACY_AWS_SDK_MAX_ATTEMPTS,
            LEGACY_AWS_SDK_INITIAL_DELAY,
            LEGACY_AWS_SDK_MAX_DELAY,
        );
        assert_eq!(legacy_sdk_backoff, std::time::Duration::from_secs(3));
        let legacy_attempt_worst_case = HTTP_API_REQUEST_TIMEOUT + HTTP_API_REQUEST_TIMEOUT;
        let legacy_sdk_operation_worst_case =
            legacy_attempt_worst_case * LEGACY_AWS_SDK_MAX_ATTEMPTS + legacy_sdk_backoff;
        assert_eq!(
            legacy_sdk_operation_worst_case,
            std::time::Duration::from_secs(183)
        );
        let legacy_aws_flow_log_worst_case = legacy_sdk_operation_worst_case
            * (AWS_FLOW_LOG_SEND_ATTEMPTS + 1)
            + AWS_FLOW_LOG_RETRY_INTERVAL * (AWS_FLOW_LOG_SEND_ATTEMPTS - 1);
        assert_eq!(
            legacy_aws_flow_log_worst_case,
            std::time::Duration::from_secs(2_434)
        );

        assert!(
            GuestCallBudget::PRODUCTION.wall() > aws_retry_operation_worst_case,
            "the production wall budget must exceed retry_aws_operation's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > aws_flow_log_worst_case,
            "the production wall budget must exceed the VPC flow-log loop's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > aws_acm_dns_validation_worst_case,
            "the production wall budget must exceed the ACM DNS-validation wait's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > awscc_guest_call_worst_case,
            "the production wall budget must exceed one AWSCC guest call's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > legacy_aws_flow_log_worst_case,
            "the production wall budget must exceed the legacy VPC flow-log loop's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > GuestCallBudget::PRODUCTION.compute(),
            "the wall budget must outlast the compute budget so compute traps win"
        );
    }
}
