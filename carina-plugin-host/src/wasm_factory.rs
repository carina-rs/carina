//! WasmProviderFactory loads a WASM component and implements ProviderFactory.

mod diagnostics;
pub use diagnostics::MetadataDiagnostic;
pub use diagnostics::SchemaDecodeError;
use guest::{guest_stderr, guest_store};

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::SystemTime;

use hmac::{Hmac, Mac};
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

use carina_core::UserDirs;
use carina_core::effect::PlanOp;
use carina_core::provider::{
    AppliedIdentifier, AppliedState, BoxFuture, CallEgressRecorder, CanonicalCustomTypeValue,
    CanonicalProviderConfigAttributes, CreateAttempt, CreateFailure, CreateOutcome, CreateRequest,
    DecodedGuestCreateCall, DeleteRequest, FinishedGuestCreateCall, InitializeOutcome, Observation,
    PendingCreate as CorePendingCreate, Poll as CorePoll, Provider, ProviderError, ProviderFactory,
    ProviderInitialization, ProviderNormalizer, ProviderReadyConfig, ProviderResult, ReadOutcome,
    ReadRequest, ResentPendingCreate, ResentPoll as CoreResentPoll, Resume as CoreResume,
    ResumeCreateRequest, ResumeFailure, ResumeOutcome, SavedAttrs, Sent, StartedGuestCreateCall,
    UpdateOutcome, UpdateRequest,
};
#[cfg(test)]
use carina_core::provider::{
    CreateFailureKind, CreateObservation, InFlightCreate, InFlightPersistence, ResumeFailureKind,
    UnobservedReason,
};
#[cfg(test)]
use carina_core::provider_text::ProviderDiagnostic;
use carina_core::provider_text::{DiagnosticText, ProviderKey, ProviderText, RedactionSet};
#[cfg(test)]
use carina_core::resource::Resource;
#[cfg(test)]
use carina_core::resource::Value;
use carina_core::resource::{ResourceId, ResourceIdentityState, SnapshotState, State};
use carina_core::schema::{
    CompletionValue, TypeIdentity, ValidatedConfigSchema, ValidatedProviderSchemaWitness,
    ValidatedResourceSchema,
};
use carina_core::wait::BindingPattern;
use carina_core::wait::predicate::AttrPath;
use carina_provider_resolver::{
    ConsentedEgress, EgressSectionError, LockedEgress, ProviderLoadIdentity, ProviderLoadSource,
    ProviderTrustClass, VerifiedEgressDeclaration, VerifiedLoad,
};

use carina_provider_protocol::http_trace::TraceUri;
use carina_provider_protocol::types as proto;
#[cfg(any(test, feature = "test-utils"))]
pub use guest_store::GuestCallBudget;
#[cfg(not(any(test, feature = "test-utils")))]
use guest_store::GuestCallBudget;
use guest_store::{EPOCH_TICK_INTERVAL, GuestStore, HostState, WallBudgetExceeded};

/// Base AWS credentials resolved by the host for an AWS-kind guest.
///
/// Secret fields are held in a private redacting type and are never included
/// in this value's `Debug` output.
///
/// Credentials cannot be obtained as plain strings for guest input.
///
/// ```compile_fail,E0308
/// use carina_plugin_host::AwsCredentials;
/// fn plain(credentials: &AwsCredentials) {
///     let _: &str = credentials.secret_access_key();
/// }
/// ```
#[derive(Clone)]
pub struct AwsCredentials {
    access_key_id: GuestCredential,
    secret_access_key: GuestCredential,
    session_token: Option<GuestCredential>,
    expires_at: Option<SystemTime>,
}

type GuestCredential = carina_core::resource::SecretPayload<
    carina_core::resource::NoPlaceholder,
    carina_core::resource::AbsoluteBindingAddress,
>;
fn guest_credential(value: impl Into<String>) -> GuestCredential {
    Box::new(carina_core::resource::Value::Concrete(
        carina_core::resource::ConcreteValue::String(value.into().into()),
    ))
    .into()
}
impl AwsCredentials {
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
        expires_at: Option<SystemTime>,
    ) -> Self {
        Self {
            access_key_id: guest_credential(access_key_id),
            secret_access_key: guest_credential(secret_access_key),
            session_token: session_token.map(guest_credential),
            expires_at,
        }
    }
    pub fn access_key_id(&self) -> &GuestCredential {
        &self.access_key_id
    }
    pub fn secret_access_key(&self) -> &GuestCredential {
        &self.secret_access_key
    }
    pub fn session_token(&self) -> Option<&GuestCredential> {
        self.session_token.as_ref()
    }
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.expires_at
    }
    /// Retain opaque credential material when adjusting host cache metadata.
    pub fn with_expiry(mut self, expires_at: Option<SystemTime>) -> Self {
        self.expires_at = expires_at;
        self
    }
}
impl fmt::Debug for AwsCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &self.secret_access_key)
            .field("session_token", &self.session_token)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Failure to obtain the host's current base AWS credentials.
///
/// Implementations must ensure payloads are free of access keys, secret keys,
/// session tokens, and other secret material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AwsCredentialError {
    NotFound(String),
    ProviderFailed(String),
}

impl fmt::Display for AwsCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(message) | Self::ProviderFailed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AwsCredentialError {}

/// Refreshing source of base credentials for AWS-kind runtime guests.
///
/// Error payloads must be rendered without secret material.
pub trait AwsCredentialSource: Send + Sync + 'static {
    fn current(&self) -> BoxFuture<'_, Result<AwsCredentials, AwsCredentialError>>;
}

/// Supplies the run-scoped credential source for an AWS-kind runtime load.
///
/// The host calls this only after resolver-owned trust has classified the
/// verified artifact as AWS. Implementations may lazily construct the source
/// and preflight it before returning.
pub trait AwsCredentialProvisioner: Send + Sync {
    fn provision(&self) -> BoxFuture<'_, Result<Arc<dyn AwsCredentialSource>, AwsCredentialError>>;
}

#[derive(Debug)]
struct WasmOperationError<E> {
    operation: &'static str,
    source: E,
}

impl<E> fmt::Display for WasmOperationError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "in operation {}", self.operation)
    }
}

impl<E> std::error::Error for WasmOperationError<E>
where
    E: std::error::Error + 'static,
{
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

/// Capture Wasmtime's complete error chain as a sanitized, bounded diagnostic.
#[cfg_attr(test, derive(Clone))]
struct WasmDiagnostic {
    text: ProviderText,
    redacted: Option<DiagnosticText>,
}

impl fmt::Debug for WasmDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmDiagnostic")
            .field("text", &self.text)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for WasmDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.redacted {
            Some(text) => text.fmt(f),
            None => f.write_str("WASM guest diagnostic"),
        }
    }
}

impl std::error::Error for WasmDiagnostic {}

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
        }
    }
}

impl std::error::Error for WasmGuestCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[cfg(test)]
fn wasm_trap_provider_error_for_test(
    operation: &'static str,
    error: wasmtime::Error,
    secrets: &crate::provider_text_fixtures::Context,
) -> carina_core::provider::HostError {
    crate::provider_text_fixtures::BindProviderText::with_secrets_for_test(
        wasm_trap_provider_error(operation, error),
        secrets,
    )
}

fn provider_schema_decode_error(
    provider_name: &str,
    provider_version: &str,
    detail: SchemaDecodeError,
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
const AWS_CREDENTIALS_INTERFACE: &str = "carina:provider/aws-credentials";

#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
struct WasiHttpImport {
    name: ProviderKey,
    version: WasiHttpImportVersion,
}

#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
enum WasiHttpImportVersion {
    Semver(semver::Version),
    Unversioned,
    Invalid,
}

#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
struct ComponentImports {
    names: Vec<ProviderKey>,
}

impl ComponentImports {
    fn from_component(engine: &Engine, component: &Component) -> Self {
        let component_type = component.component_type();
        let mut names: Vec<_> = component_type
            .imports(engine)
            .map(|(name, _item)| ProviderKey::from(name))
            .collect();
        names.sort();
        names.dedup();
        Self { names }
    }

    fn iter(&self) -> impl Iterator<Item = &ProviderKey> {
        self.names.iter()
    }

    fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    fn imports_aws_credentials(&self) -> bool {
        self.iter().any(|name| {
            let name = name.as_str();
            name == AWS_CREDENTIALS_INTERFACE
                || name
                    .strip_prefix(AWS_CREDENTIALS_INTERFACE)
                    .is_some_and(|suffix| suffix.starts_with('@'))
        })
    }
}

/// A non-empty set of wasi:http imports. Splitting out the first item keeps
/// the "component imports wasi:http" state non-empty by construction.
#[cfg_attr(test, derive(Clone))]
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
                let name = name.as_str();
                if !carina_provider_resolver::is_wasi_http_import(name) {
                    return None;
                }
                let version = match name.rsplit_once('@') {
                    Some((_, raw)) => semver::Version::parse(raw)
                        .map(WasiHttpImportVersion::Semver)
                        .unwrap_or(WasiHttpImportVersion::Invalid),
                    None => WasiHttpImportVersion::Unversioned,
                };
                Some(WasiHttpImport {
                    name: name.into(),
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

#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
struct WasiHttpImportedContext {
    imports: WasiHttpImports,
    host_version: semver::Version,
    compatibility: WasiHttpCompatibility,
}

#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
struct WasiHttpNotImportedContext {
    component_imports: ComponentImports,
    host_version: semver::Version,
}

#[cfg_attr(test, derive(Clone))]
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

#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
enum DualAttemptContext {
    WasiHttpImported {
        context: WasiHttpImportedContext,
    },
    WasiHttpNotImported {
        context: WasiHttpNotImportedContext,
        basic_failure: WasmDiagnostic,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstantiationWorld {
    HttpEnabled,
    Basic,
}

impl InstantiationWorld {
    #[cfg(any(test, feature = "test-utils"))]
    fn test_name(self) -> &'static str {
        match self {
            Self::HttpEnabled => "http-enabled",
            Self::Basic => "basic",
        }
    }
}

/// The attempt shape is explicit: the load path carries its dual-attempt
/// fallback disposition, while the runtime path carries exactly one selected
/// world and cannot represent a fallback that never happened.
#[cfg_attr(test, derive(Clone))]
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
#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
pub struct ProviderInstantiationError {
    primary_failure: WasmDiagnostic,
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
                    basic_failure: WasmDiagnostic::new(basic_failure, false),
                }
            }
        };
        Self {
            primary_failure: WasmDiagnostic::new(http_failure, false),
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
            primary_failure: WasmDiagnostic::new(failure, false),
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
        self.primary_failure.fmt(f)?;
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
                basic_failure.fmt(f)
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
    /// The exact loaded bytes carried an invalid or missing egress declaration.
    EgressDeclaration(MetadataDiagnostic),
    /// The project lock could not be read while selecting an egress record.
    EgressLock(carina_provider_resolver::LockedEgressLoadError),
    /// The selected lock record did not authorize the exact loaded declaration.
    EgressConsent(carina_provider_resolver::EgressConsentError),
    /// `Component::deserialize_file` rejected a precompiled cache entry.
    PrecompiledDeserialization(PrecompiledComponentDeserializationError),
    /// The component could not be instantiated as a Carina provider.
    Instantiation(ProviderInstantiationError),
    /// The artifact reported a different provider name from the host
    /// configuration that selected it.
    ProviderNameMismatch {
        configured: DiagnosticText,
        reported: DiagnosticText,
    },
    /// Host-side AWS credential provisioning failed before guest instantiation.
    AwsCredentials(AwsCredentialError),
    /// An AWS-kind runtime component did not import the AWS credential interface.
    AwsCredentialsImportMissing,
    /// A non-AWS runtime component imported the AWS credential interface.
    AwsCredentialsRequired,
    /// A provider metadata export trapped or returned malformed data.
    Metadata {
        context: DiagnosticText,
        source: MetadataDiagnostic,
    },
    /// Any other engine, component-load, or provider bring-up failure.
    Other(DiagnosticText),
}

impl fmt::Display for WasmProviderLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WasmProviderLoadError::EgressDeclaration(error) => error.fmt(f),
            WasmProviderLoadError::EgressLock(error) => error.fmt(f),
            WasmProviderLoadError::EgressConsent(error) => error.fmt(f),
            WasmProviderLoadError::PrecompiledDeserialization(error) => error.fmt(f),
            WasmProviderLoadError::Instantiation(error) => error.fmt(f),
            WasmProviderLoadError::ProviderNameMismatch {
                configured,
                reported,
            } => write!(
                f,
                "configured provider name '{configured}' does not match artifact-reported name '{reported}'"
            ),
            WasmProviderLoadError::AwsCredentials(error) => error.fmt(f),
            WasmProviderLoadError::AwsCredentialsImportMissing => f.write_str(
                "an AWS-kind provider must be built with the `aws` world (`export_provider!(T, aws, egress: [..])`)",
            ),
            WasmProviderLoadError::AwsCredentialsRequired => f.write_str(
                "this provider requires AWS credentials, which only an AWS-kind provider can receive",
            ),
            WasmProviderLoadError::Metadata { context, source } => {
                write!(f, "{context}: {source}")
            }
            WasmProviderLoadError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for WasmProviderLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EgressLock(source) => source.source(),
            Self::EgressConsent(source) => source.source(),
            Self::EgressDeclaration(_)
            | Self::PrecompiledDeserialization(_)
            | Self::Instantiation(_)
            | Self::ProviderNameMismatch { .. }
            | Self::AwsCredentials(_)
            | Self::AwsCredentialsImportMissing
            | Self::AwsCredentialsRequired
            | Self::Metadata { .. }
            | Self::Other(_) => None,
        }
    }
}

impl WasmProviderLoadError {
    fn metadata(
        context: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Metadata {
            context: DiagnosticText::new(context),
            source: MetadataDiagnostic::new(source),
        }
    }

    /// Whether this load failure came from the wall-clock budget enforced by
    /// [`GuestStore::call`]. The typed source installed by
    /// [`wall_budget_load_error`] is the classification boundary; diagnostic
    /// text is deliberately not inspected.
    fn is_wall_budget_timeout(&self) -> bool {
        matches!(
            self,
            Self::Metadata { source, .. }
                if source.is::<WasmWallBudgetError>()
        )
    }
}

impl From<ProviderInstantiationError> for WasmProviderLoadError {
    fn from(error: ProviderInstantiationError) -> Self {
        WasmProviderLoadError::Instantiation(error)
    }
}

impl From<EgressSectionError> for WasmProviderLoadError {
    fn from(error: EgressSectionError) -> Self {
        Self::EgressDeclaration(MetadataDiagnostic::new(error))
    }
}

impl From<carina_provider_resolver::LockedEgressLoadError> for WasmProviderLoadError {
    fn from(error: carina_provider_resolver::LockedEgressLoadError) -> Self {
        Self::EgressLock(error)
    }
}

impl From<carina_provider_resolver::EgressConsentError> for WasmProviderLoadError {
    fn from(error: carina_provider_resolver::EgressConsentError) -> Self {
        Self::EgressConsent(error)
    }
}

impl From<AwsCredentialError> for WasmProviderLoadError {
    fn from(error: AwsCredentialError) -> Self {
        Self::AwsCredentials(error)
    }
}

impl From<String> for WasmProviderLoadError {
    fn from(message: String) -> Self {
        WasmProviderLoadError::Other(message.into())
    }
}

impl From<DiagnosticText> for WasmProviderLoadError {
    fn from(message: DiagnosticText) -> Self {
        Self::Other(message)
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
    source: WasmDiagnostic,
}

impl fmt::Display for PrecompiledComponentDeserializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Failed to deserialize WASM component from {}: ",
            self.cwasm_path.display()
        )?;
        self.source.fmt(f)
    }
}

impl std::error::Error for PrecompiledComponentDeserializationError {}

/// Failure while creating a credentialed per-binding runtime instance.
/// Instantiation, `info` guest-call, and decoded initialization failures remain
/// typed until the `ProviderFactory` trait boundary.
#[derive(Debug)]
enum WasmProviderInstanceError {
    MissingEgressConsent,
    Instantiation(ProviderInstantiationError),
    InfoCall(wasmtime::Error),
    WallBudgetExceeded {
        operation: &'static str,
        source: WallBudgetExceeded,
    },
    InitializeCall(wasmtime::Error),
    Initialize(ProviderError),
    InitializeReferenceScope(proto::ReferenceScopeError),
    ProtocolVersion(DiagnosticText),
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
            WasmProviderInstanceError::MissingEgressConsent => {
                carina_core::provider::HostError::internal(
                    "provider runtime instance is missing consented egress",
                )
                .into()
            }
            WasmProviderInstanceError::Instantiation(error) => mapper(error),
            WasmProviderInstanceError::InfoCall(error) => wasm_trap_provider_error("info", error),
            WasmProviderInstanceError::WallBudgetExceeded { operation, source } => {
                wall_budget_provider_error(operation, source)
            }
            WasmProviderInstanceError::InitializeCall(error) => {
                wasm_trap_provider_error("initialize", error)
            }
            WasmProviderInstanceError::Initialize(error) => error,
            WasmProviderInstanceError::InitializeReferenceScope(error) => {
                ProviderError::invalid_input_caused_by(
                    "provider returned an invalid reference scope from initialize",
                    error,
                )
            }
            WasmProviderInstanceError::ProtocolVersion(message) => {
                carina_core::provider::HostError::internal(message.to_string()).into()
            }
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

// -- HTTP egress hooks --

/// The complete network capability carried by one guest store.
#[derive(Debug, Clone)]
enum GuestNetwork {
    SchemaOnly,
    Runtime {
        capabilities: RuntimeCapabilities,
        egress: ConsentedEgress,
    },
}

enum GuestNetworkRequest<'a> {
    SchemaOnly,
    Runtime {
        egress: ConsentedEgress,
        aws: &'a dyn AwsCredentialProvisioner,
    },
}

impl GuestNetwork {
    fn kind(&self) -> Option<ProviderKind> {
        match self {
            Self::SchemaOnly => None,
            Self::Runtime {
                capabilities,
                egress: _,
            } => Some(capabilities.kind()),
        }
    }

    fn runtime_capabilities(&self) -> Option<&RuntimeCapabilities> {
        match self {
            Self::SchemaOnly => None,
            Self::Runtime {
                capabilities,
                egress: _,
            } => Some(capabilities),
        }
    }
}

#[cfg(test)]
fn test_explicit_egress(declaration: &VerifiedEgressDeclaration) -> LockedEgress {
    let directory = tempfile::tempdir().expect("create egress lock directory");
    let source = declaration.source().to_string();
    let sha256 = declaration.sha256().as_str();
    let hosts = declaration
        .patterns()
        .iter()
        .map(|pattern| format!("    {:?},", pattern.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        directory.path().join("carina-providers.lock"),
        format!(
            "version = 5\n\n[[provider]]\nname = \"not-consent-identity\"\nsource = {source:?}\nmode = \"file\"\nsha256 = {sha256:?}\n\n[provider.egress]\nconsent = \"explicit\"\nhosts = [\n{hosts}\n]\n"
        ),
    )
    .expect("write egress lock");
    let project =
        carina_core::paths::ResolvedPath::new(directory.path()).expect("pin fixture project");
    let (lock, _migration) = carina_provider_resolver::LockFile::load(&project)
        .expect("load egress lock")
        .expect("egress lock exists")
        .into_parts();
    lock.locked_egress(declaration, &project)
        .expect("lock has consent for the declaration source")
}

/// Every metadata address withheld from ordinary resolved destinations.
///
/// Alibaba and Azure metadata are deliberately not owned by any
/// [`ProviderKind`], so they remain withheld even from AWS-kind providers.
const METADATA_ENDPOINTS: [IpAddr; 5] = [
    IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
    IpAddr::V4(Ipv4Addr::new(169, 254, 170, 2)),
    IpAddr::V6(Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254)),
    IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200)),
    IpAddr::V4(Ipv4Addr::new(168, 63, 129, 16)),
];

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
        Self::start_with_tick_observer(engine, || {})
    }

    fn start_with_tick_observer(engine: Engine, on_tick: impl Fn() + Send + 'static) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(EPOCH_TICK_INTERVAL);
                engine.increment_epoch();
                on_tick();
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DestinationDenied {
    HostNotAllowed,
    MetadataWithheld(IpAddr),
    LoopbackAddressDenied(IpAddr),
    UnspecifiedAddressDenied(IpAddr),
    LinkLocalAddressDenied(IpAddr),
    PrivateAddressDenied(IpAddr),
}

impl fmt::Display for DestinationDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DestinationDenied::HostNotAllowed => {
                f.write_str("host is not in the provider's consented egress declaration")
            }
            DestinationDenied::MetadataWithheld(address) => {
                write!(
                    f,
                    "instance-metadata address {address} is withheld from every provider"
                )
            }
            DestinationDenied::LoopbackAddressDenied(address) => {
                write!(f, "loopback resolved address {address} is denied")
            }
            DestinationDenied::UnspecifiedAddressDenied(address) => {
                write!(f, "unspecified resolved address {address} is denied")
            }
            DestinationDenied::LinkLocalAddressDenied(address) => {
                write!(f, "link-local resolved address {address} is denied")
            }
            DestinationDenied::PrivateAddressDenied(address) => write!(
                f,
                "private resolved address {address} is not available to this provider"
            ),
        }
    }
}

#[derive(Debug)]
struct EgressDenied {
    label: Arc<ProviderLabel>,
    kind: Option<ProviderKind>,
    destination: String,
    reason: DestinationDenied,
}

impl EgressDenied {
    fn new(
        label: Arc<ProviderLabel>,
        kind: Option<ProviderKind>,
        destination: String,
        reason: DestinationDenied,
    ) -> Self {
        Self {
            label,
            kind,
            destination,
            reason,
        }
    }
}

impl fmt::Display for EgressDenied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.kind, &self.label.source, &self.reason) {
            (
                None
                | Some(ProviderKind::Aws)
                | Some(ProviderKind::GitHub)
                | Some(ProviderKind::Untrusted),
                ProviderLoadSource::LocalFile { .. } | ProviderLoadSource::Remote { source: _ },
                DestinationDenied::HostNotAllowed
                | DestinationDenied::MetadataWithheld(_)
                | DestinationDenied::LoopbackAddressDenied(_)
                | DestinationDenied::UnspecifiedAddressDenied(_)
                | DestinationDenied::LinkLocalAddressDenied(_)
                | DestinationDenied::PrivateAddressDenied(_),
            ) => write!(
                formatter,
                "{}",
                DiagnosticText::new(format!(
                    "provider '{}' ({}) request to {} was denied: {}",
                    self.label.name, self.label.source, self.destination, self.reason
                ))
            ),
        }
    }
}

impl std::error::Error for EgressDenied {}

#[derive(Debug)]
struct EgressDeniedCall {
    operation: &'static str,
    denial: EgressDenied,
}

impl EgressDeniedCall {
    fn denial(&self) -> &EgressDenied {
        &self.denial
    }
}

impl fmt::Display for EgressDeniedCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} call: {}", self.operation, self.denial())
    }
}

// `Display` embeds the denial, so exposing it again through `source()` would
// duplicate the diagnostic in error-chain renderers.
impl std::error::Error for EgressDeniedCall {}

/// A host that passed the request URI's host-string policy.
///
/// Its fields are private so only [`check_host`] can create it. Resolution is
/// still required before this value can become a [`VettedDestination`].
struct AllowedHost {
    private_address_policy: Option<PrivateAddressPolicy>,
    host: String,
}

/// The only destination type accepted by the TCP connection handler.
///
/// Production instances are created only by [`vet_resolved_destination`],
/// which checks every resolved address before constructing this value. The
/// cfg-gated [`HttpTestRoute`] mechanism has the only test-only constructor.
struct VettedDestination {
    host: String,
    first: SocketAddr,
    rest: Vec<SocketAddr>,
}

impl VettedDestination {
    /// Construct the integration-test-only loopback destination selected by
    /// [`HttpTestRoute`]. Production builds have no route variant and compile
    /// no bypass constructor.
    #[cfg(any(test, feature = "test-utils"))]
    fn from_http_test_route(allowed_host: AllowedHost, destination: HttpTestDestination) -> Self {
        Self {
            host: allowed_host.host,
            first: destination.address(),
            rest: Vec::new(),
        }
    }
}

/// A factory-scoped HTTP route used only by host/guest integration tests.
///
/// Production construction never sets this value. Keeping it on the factory,
/// instead of a process-global override, prevents parallel tests from changing
/// one another's outbound policy.
#[derive(Clone, Debug)]
enum HttpTestRoute {
    #[cfg(any(test, feature = "test-utils"))]
    Configured {
        host: String,
        destination: SocketAddr,
    },
}

#[cfg(any(test, feature = "test-utils"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HttpTestDestination(SocketAddr);

#[cfg(any(test, feature = "test-utils"))]
impl HttpTestDestination {
    fn loopback(address: SocketAddr) -> Option<Self> {
        let canonical_address = canonical_policy_address(address.ip());
        match ResolvedAddressClass::classify(canonical_address) {
            ResolvedAddressClass::Loopback => Some(Self(address)),
            ResolvedAddressClass::Metadata
            | ResolvedAddressClass::Unspecified
            | ResolvedAddressClass::LinkLocal
            | ResolvedAddressClass::Private
            | ResolvedAddressClass::Public => None,
        }
    }

    fn address(self) -> SocketAddr {
        self.0
    }
}

impl HttpTestRoute {
    #[cfg(any(test, feature = "test-utils"))]
    fn new(host: String, destination: SocketAddr) -> Self {
        Self::Configured { host, destination }
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn resolution_for(&self, host: &str, port: u16, use_tls: bool) -> Option<ResolvedAddresses> {
        match self {
            Self::Configured {
                host: route_host,
                destination,
            } if !use_tls
                && route_host.eq_ignore_ascii_case(host)
                && destination.port() == port =>
            {
                Some(match HttpTestDestination::loopback(*destination) {
                    Some(destination) => ResolvedAddresses::HttpTestRoute(destination),
                    None => ResolvedAddresses::Standard(vec![*destination]),
                })
            }
            Self::Configured {
                host: _,
                destination: _,
            } => None,
        }
    }
}

enum ResolvedAddresses {
    Standard(Vec<SocketAddr>),
    #[cfg(any(test, feature = "test-utils"))]
    HttpTestRoute(HttpTestDestination),
}

fn ip_literal(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}

/// Canonicalize addresses before applying resolved-address policy.
///
/// In addition to IPv4-mapped IPv6, treat the final 32 bits of the NAT64
/// well-known prefix and the RFC 8215 local-use prefix as their embedded IPv4
/// address. The local-use prefix is deliberately handled with the same
/// last-32-bit layout as the well-known /96 prefix.
fn canonical_policy_address(address: IpAddr) -> IpAddr {
    match address.to_canonical() {
        IpAddr::V6(address) => {
            let segments = address.segments();
            let is_well_known_nat64 = segments[..6] == [0x0064, 0xff9b, 0, 0, 0, 0];
            let is_local_use_nat64 = segments[..3] == [0x0064, 0xff9b, 0x0001];
            if is_well_known_nat64 || is_local_use_nat64 {
                let octets = address.octets();
                IpAddr::V4(Ipv4Addr::new(
                    octets[12], octets[13], octets[14], octets[15],
                ))
            } else {
                IpAddr::V6(address)
            }
        }
        address => address,
    }
}

/// Security-relevant class of one canonical resolved address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedAddressClass {
    Metadata,
    Loopback,
    Unspecified,
    LinkLocal,
    Private,
    Public,
}

impl ResolvedAddressClass {
    /// Classify an address that has already passed through
    /// [`canonical_policy_address`]. Metadata wins over every range class.
    fn classify(address: IpAddr) -> Self {
        if METADATA_ENDPOINTS.contains(&address) {
            return Self::Metadata;
        }

        match address {
            IpAddr::V4(address) if address.is_loopback() => Self::Loopback,
            IpAddr::V6(address) if address.is_loopback() => Self::Loopback,
            IpAddr::V4(address) if address.is_unspecified() => Self::Unspecified,
            IpAddr::V6(address) if address.is_unspecified() => Self::Unspecified,
            IpAddr::V4(address) if address.is_link_local() => Self::LinkLocal,
            IpAddr::V6(address) if (address.segments()[0] & 0xffc0) == 0xfe80 => Self::LinkLocal,
            IpAddr::V4(address) => {
                let [first, second, _, _] = address.octets();
                if first == 10
                    || (first == 172 && (16..=31).contains(&second))
                    || (first == 192 && second == 168)
                    || (first == 100 && (64..=127).contains(&second))
                {
                    Self::Private
                } else {
                    Self::Public
                }
            }
            IpAddr::V6(address) => {
                if (address.segments()[0] & 0xfe00) == 0xfc00 {
                    Self::Private
                } else {
                    Self::Public
                }
            }
        }
    }
}

fn check_host(network: &GuestNetwork, host: &str) -> Result<AllowedHost, DestinationDenied> {
    let private_address_policy = network
        .runtime_capabilities()
        .map(RuntimeCapabilities::private_address_policy);
    let target = match ip_literal(host) {
        Some(address) => {
            let canonical_address = canonical_policy_address(address);
            match ResolvedAddressClass::classify(canonical_address) {
                ResolvedAddressClass::Metadata => {
                    return Err(DestinationDenied::MetadataWithheld(canonical_address));
                }
                ResolvedAddressClass::Loopback
                | ResolvedAddressClass::Unspecified
                | ResolvedAddressClass::LinkLocal
                | ResolvedAddressClass::Private
                | ResolvedAddressClass::Public => {
                    return Err(DestinationDenied::HostNotAllowed);
                }
            }
        }
        None => match network {
            GuestNetwork::SchemaOnly => return Err(DestinationDenied::HostNotAllowed),
            GuestNetwork::Runtime {
                capabilities: _,
                egress,
            } if egress.allows(host) => host.to_string(),
            GuestNetwork::Runtime {
                capabilities: _,
                egress: _,
            } => {
                return Err(DestinationDenied::HostNotAllowed);
            }
        },
    };

    Ok(AllowedHost {
        private_address_policy,
        host: target,
    })
}

fn vet_resolved_destination(
    allowed_host: AllowedHost,
    first: SocketAddr,
    rest: &[SocketAddr],
) -> Result<VettedDestination, (DestinationDenied, String)> {
    let AllowedHost {
        private_address_policy,
        host,
    } = allowed_host;
    let private_address_policy = private_address_policy.unwrap_or(PrivateAddressPolicy::Deny);
    for address in std::iter::once(&first).chain(rest) {
        let canonical_ip = canonical_policy_address(address.ip());
        match ResolvedAddressClass::classify(canonical_ip) {
            ResolvedAddressClass::Metadata => {
                return Err((DestinationDenied::MetadataWithheld(canonical_ip), host));
            }
            ResolvedAddressClass::Loopback => {
                return Err((DestinationDenied::LoopbackAddressDenied(canonical_ip), host));
            }
            ResolvedAddressClass::Unspecified => {
                return Err((
                    DestinationDenied::UnspecifiedAddressDenied(canonical_ip),
                    host,
                ));
            }
            ResolvedAddressClass::LinkLocal => {
                return Err((
                    DestinationDenied::LinkLocalAddressDenied(canonical_ip),
                    host,
                ));
            }
            ResolvedAddressClass::Private => match private_address_policy {
                PrivateAddressPolicy::Allow => {}
                PrivateAddressPolicy::Deny => {
                    return Err((DestinationDenied::PrivateAddressDenied(canonical_ip), host));
                }
            },
            ResolvedAddressClass::Public => {}
        }
    }

    Ok(VettedDestination {
        host,
        first,
        rest: rest.to_vec(),
    })
}

/// Apply both the URI host-string policy and the post-resolution address
/// policy without performing network I/O.
#[cfg(test)]
fn check_destination(
    network: &GuestNetwork,
    host: &str,
    resolved: &[SocketAddr],
) -> Result<VettedDestination, DestinationDenied> {
    let (first, rest) = resolved
        .split_first()
        .expect("test resolution must contain at least one address");
    let allowed_host = check_host(network, host)?;
    vet_resolved_destination(allowed_host, *first, rest)
        .map_err(|(reason, _denied_destination)| reason)
}

type ResolveFuture =
    Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'static>>;

trait DestinationResolver: Send + Sync {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture;
}

struct TokioDestinationResolver;

impl DestinationResolver for TokioDestinationResolver {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture {
        Box::pin(async move {
            tokio::net::lookup_host((host.as_str(), port))
                .await
                .map(|addresses| addresses.collect())
        })
    }
}

fn dns_error_code() -> wasmtime_wasi_http::p2::bindings::http::types::ErrorCode {
    use wasmtime_wasi_http::p2::bindings::http::types::{DnsErrorPayload, ErrorCode};

    ErrorCode::DnsError(DnsErrorPayload {
        rcode: Some("address not available".to_string()),
        info_code: Some(0),
    })
}

/// Custom `WasiHttpHooks` that restricts outgoing HTTP requests to this
/// store's consented declaration and typed resolved-address policy.
struct AllowListHttpHooks {
    label: Arc<ProviderLabel>,
    network: GuestNetwork,
    resolver: Arc<dyn DestinationResolver>,
    test_route: Option<HttpTestRoute>,
    call_egress: Option<CallEgressRecorder>,
    stderr: guest_stderr::GuestStderr,
}

impl AllowListHttpHooks {
    fn with_resolver(
        label: Arc<ProviderLabel>,
        network: GuestNetwork,
        resolver: Arc<dyn DestinationResolver>,
    ) -> Self {
        Self {
            label,
            network,
            resolver,
            test_route: None,
            call_egress: None,
            stderr: guest_stderr::GuestStderr::new(std::io::stderr()),
        }
    }

    fn with_test_route(
        label: Arc<ProviderLabel>,
        network: GuestNetwork,
        test_route: Option<HttpTestRoute>,
    ) -> Self {
        let mut hooks = Self::with_resolver(label, network, Arc::new(TokioDestinationResolver));
        hooks.test_route = test_route;
        hooks
    }
}

impl wasmtime_wasi_http::p2::WasiHttpHooks for AllowListHttpHooks {
    fn send_request(
        &mut self,
        request: hyper::Request<wasmtime_wasi_http::p2::body::HyperOutgoingBody>,
        config: wasmtime_wasi_http::p2::types::OutgoingRequestConfig,
    ) -> wasmtime_wasi_http::p2::HttpResult<wasmtime_wasi_http::p2::types::HostFutureIncomingResponse>
    {
        use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;

        let host = match request.uri().host() {
            Some(host) => host,
            None => {
                return Err(ErrorCode::HttpRequestUriInvalid.into());
            }
        };
        let port = request
            .uri()
            .port_u16()
            .unwrap_or(if config.use_tls { 443 } else { 80 });
        let kind = self.network.kind();
        let allowed_host = check_host(&self.network, host).map_err(|reason| {
            wasmtime_wasi_http::p2::HttpError::trap(EgressDenied::new(
                Arc::clone(&self.label),
                kind,
                host.to_string(),
                reason,
            ))
        })?;
        #[cfg(any(test, feature = "test-utils"))]
        let test_resolution = self
            .test_route
            .as_ref()
            .and_then(|route| route.resolution_for(host, port, config.use_tls));
        let label = Arc::clone(&self.label);
        let resolver = Arc::clone(&self.resolver);
        let stderr = self.stderr.clone();
        let mut pending_egress = self
            .call_egress
            .as_ref()
            .map(CallEgressRecorder::begin_request);
        let trace_state = trace_http_enabled().then(|| {
            (
                request.method().to_string(),
                request.uri().to_string(),
                std::time::Instant::now(),
            )
        });
        let handle = wasmtime_wasi::runtime::spawn(async move {
            let handler_start = std::time::Instant::now();
            let trace = trace_state.is_some();
            let resolved = async {
                #[cfg(any(test, feature = "test-utils"))]
                if let Some(resolution) = test_resolution {
                    return Ok::<_, ErrorCode>(resolution);
                }

                let host = &allowed_host.host;
                let addresses = tokio::time::timeout(
                    config.connect_timeout.min(HTTP_API_REQUEST_TIMEOUT),
                    resolver.resolve(host.clone(), port),
                )
                .await
                .map_err(|_| {
                    log::warn!("{}", http_resolution_warning(host, None));
                    ErrorCode::ConnectionTimeout
                })?
                .map_err(|error| {
                    log::warn!("{}", http_resolution_warning(host, Some(&error)));
                    dns_error_code()
                })?;
                Ok::<_, ErrorCode>(ResolvedAddresses::Standard(addresses))
            }
            .await;
            let vetted = match resolved {
                Err(error) => Ok(Err(error)),
                Ok(ResolvedAddresses::Standard(resolved)) => match resolved.split_first() {
                    None => Ok(Err(dns_error_code())),
                    Some((first, rest)) => {
                        vet_resolved_destination(allowed_host, *first, rest).map(Ok)
                    }
                },
                #[cfg(any(test, feature = "test-utils"))]
                Ok(ResolvedAddresses::HttpTestRoute(destination)) => Ok(Ok(
                    VettedDestination::from_http_test_route(allowed_host, destination),
                )),
            };
            let result = match vetted {
                Err((reason, destination)) => Err(wasmtime::Error::new(EgressDenied::new(
                    label,
                    kind,
                    destination,
                    reason,
                ))),
                Ok(Err(error)) => Ok(Err(error)),
                Ok(Ok(vetted)) => {
                    if let Some(pending) = pending_egress.take() {
                        pending.dispatched();
                    }
                    let mut config = config;
                    config.connect_timeout = config.connect_timeout.min(HTTP_API_REQUEST_TIMEOUT);
                    config.first_byte_timeout =
                        config.first_byte_timeout.min(HTTP_API_REQUEST_TIMEOUT);
                    config.between_bytes_timeout =
                        config.between_bytes_timeout.min(HTTP_API_REQUEST_TIMEOUT);

                    Ok(
                        send_request_handler(
                            request,
                            config,
                            vetted,
                            trace.then(|| stderr.clone()),
                        )
                        .await,
                    )
                }
            };
            if let Some((method, uri, spawn_start)) = trace_state {
                let queue_ms = handler_start.duration_since(spawn_start).as_millis();
                let handler_ms = handler_start.elapsed().as_millis();
                let _ = stderr.write_line(&http_trace_line(
                    &method,
                    TraceUri::new(&uri),
                    queue_ms,
                    handler_ms,
                    &match &result {
                        Ok(Ok(response)) => format!("{}", response.resp.status().as_u16()),
                        Ok(Err(error)) => format!("err:{error:?}"),
                        Err(error) => format!("trap:{error}"),
                    },
                ));
            }
            result
        });

        Ok(wasmtime_wasi_http::p2::types::HostFutureIncomingResponse::pending(handle))
    }
}

/// Companion to carina-plugin-sdk's `CARINA_WASI_HTTP_TRACE` switch.
///
/// When set to "1", the host-side request handler emits its wall-clock
/// breakdown to stderr with URI userinfo, fragment and query values removed.
/// Off by default; the gate is a single atomic load per request when disabled.
fn trace_http_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("CARINA_WASI_HTTP_TRACE")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

fn http_resolution_warning(host: &str, error: Option<&dyn fmt::Display>) -> DiagnosticText {
    DiagnosticText::new(match error {
        Some(error) => format!("WASM plugin HTTP request failed to resolve host {host:?}: {error}"),
        None => format!("WASM plugin HTTP request timed out resolving host {host:?}"),
    })
}

// Keep complete text until the instance writer redacts it. Truncating here
// could expose a secret prefix when its suffix falls beyond the line limit.
fn http_trace_line(
    method: &str,
    uri: TraceUri<'_>,
    queue_ms: u128,
    handler_ms: u128,
    status: &str,
) -> String {
    format!(
        "carina-host-http-trace method={} uri={} queue_ms={} handler_ms={} status={}",
        method, uri, queue_ms, handler_ms, status,
    )
}

fn http_trace_phases_line(method: &str, uri: TraceUri<'_>, phases_ms: [u128; 4]) -> String {
    let [tcp_connect, tls_handshake, http_handshake, send_request] = phases_ms;
    format!(
        "carina-host-http-trace-phases method={} uri={} \
         tcp_connect_ms={} tls_handshake_ms={} http_handshake_ms={} send_request_ms={}",
        method, uri, tcp_connect, tls_handshake, http_handshake, send_request,
    )
}

/// Local copy of `wasmtime_wasi_http::p2::default_send_request_handler` that
/// connects only to addresses produced by [`vet_resolved_destination`] or by
/// the cfg-gated [`HttpTestRoute`] integration-test mechanism.
///
/// Both traced and untraced requests use this handler, preventing the
/// connection path from performing a second DNS lookup after policy checks.
/// When `trace` is true it also records `Instant::elapsed()` between phases.
///
/// Phases (cumulative ms from entry):
/// - `tcp_connect_ms` — TCP three-way handshake (`TcpStream::connect`)
/// - `tls_handshake_ms` — rustls/tokio-rustls handshake (HTTPS only)
/// - `http_handshake_ms` — hyper http/1.1 protocol handshake
/// - `send_request_ms` — request headers/body sent, response head arrived
///
async fn send_request_handler(
    mut request: hyper::Request<wasmtime_wasi_http::p2::body::HyperOutgoingBody>,
    config: wasmtime_wasi_http::p2::types::OutgoingRequestConfig,
    destination: VettedDestination,
    trace: Option<guest_stderr::GuestStderr>,
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

    let trace_request = trace.as_ref().map(|stderr| {
        (
            stderr,
            request.method().to_string(),
            request.uri().to_string(),
        )
    });

    let VettedDestination { host, first, rest } = destination;
    let tcp_stream = timeout(connect_timeout, async move {
        let mut last_error = match TcpStream::connect(first).await {
            Ok(stream) => return Ok(stream),
            Err(error) => error,
        };
        for address in rest {
            match TcpStream::connect(address).await {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    })
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
        let domain = ServerName::try_from(host).map_err(|_| {
            ErrorCode::DnsError(
                wasmtime_wasi_http::p2::bindings::http::types::DnsErrorPayload {
                    rcode: Some("invalid dns name".to_string()),
                    info_code: Some(0),
                },
            )
        })?;
        let stream = connector.connect(domain, tcp_stream).await.map_err(|_| {
            // The SDK classifies TlsProtocolError as NotSent because the
            // carina host emits it only here, during the TLS handshake.
            ErrorCode::TlsProtocolError
        })?;
        let tls_handshake_ms = ms(phase_start);

        let stream = TokioIo::new(stream);
        let (sender, conn) = timeout(
            connect_timeout,
            hyper::client::conn::http1::handshake(stream),
        )
        .await
        .map_err(|_| ErrorCode::ConnectionTimeout)?
        .map_err(|_| {
            // Hyper has not received a request yet, so this is pre-send even
            // though wasi:http has no dedicated HTTP-handshake error code.
            ErrorCode::ConnectionRefused
        })?;
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
        .map_err(|_| {
            // Hyper has not received a request yet, so this is pre-send even
            // though wasi:http has no dedicated HTTP-handshake error code.
            ErrorCode::ConnectionRefused
        })?;
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

    if let Some((stderr, method, uri)) = trace_request {
        let _ = stderr.write_line(&http_trace_phases_line(
            &method,
            TraceUri::new(&uri),
            [
                tcp_connect_ms,
                tls_handshake_ms,
                http_handshake_ms,
                send_request_ms,
            ],
        ));
    }

    Ok(wasmtime_wasi_http::p2::types::IncomingResponse {
        resp,
        worker: Some(worker),
        between_bytes_timeout,
    })
}

fn host_stderr() -> std::io::Stderr {
    std::io::stderr()
}

// -- Helper: create a new Store + CarinaProvider instance --

enum WasmBindingCreationError {
    Instantiation(wasmtime::Error),
    InfoCall(wasmtime::Error),
    WallBudgetExceeded {
        operation: &'static str,
        source: WallBudgetExceeded,
    },
    ProtocolVersion(DiagnosticText),
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

fn map_binding_creation_error<T>(
    failure: WasmBindingCreationError,
    instantiation: impl FnOnce(wasmtime::Error) -> T,
    info_call: impl FnOnce(wasmtime::Error) -> T,
    wall_budget_exceeded: impl FnOnce(&'static str, WallBudgetExceeded) -> T,
    protocol_version: impl FnOnce(DiagnosticText) -> T,
) -> T {
    match failure {
        WasmBindingCreationError::Instantiation(error) => instantiation(error),
        WasmBindingCreationError::InfoCall(error) => info_call(error),
        WasmBindingCreationError::WallBudgetExceeded { operation, source } => {
            wall_budget_exceeded(operation, source)
        }
        WasmBindingCreationError::ProtocolVersion(message) => protocol_version(message),
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

/// Register host imports in one place for runtime and metadata-only loads.
///
/// Linking an interface does not grant its capabilities: schema-only stores
/// use the same typed credential import, but its host implementation returns
/// the no-credentials error without consulting a credential source.
fn create_linker(
    engine: &Engine,
    network: &GuestNetwork,
    world: InstantiationWorld,
) -> Result<Linker<HostState>, WasmBindingCreationError> {
    let mut linker = Linker::new(engine);
    add_wasi_sans_sockets_to_linker(&mut linker).map_err(|error| {
        WasmBindingCreationError::Instantiation(error.context("Failed to add WASI to linker"))
    })?;
    match world {
        InstantiationWorld::Basic => {}
        InstantiationWorld::HttpEnabled => {
            wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker).map_err(
                |error| {
                    WasmBindingCreationError::Instantiation(
                        error.context("Failed to add wasi:http to linker"),
                    )
                },
            )?;
        }
    }
    add_aws_credentials_to_linker(&mut linker, network).map_err(|error| {
        WasmBindingCreationError::Instantiation(
            error.context("Failed to add AWS credentials to linker"),
        )
    })?;
    Ok(linker)
}

async fn create_instance(
    engine: &Engine,
    component: &Component,
    label: Arc<ProviderLabel>,
    network: GuestNetwork,
    budget: GuestCallBudget,
    text: Option<carina_core::provider::ProviderValidationInputs>,
) -> Result<(GuestStore, WasmBindings), WasmBindingCreationError> {
    let linker = create_linker(engine, &network, InstantiationWorld::Basic)?;
    let mut store = GuestStore::new(engine, label, network, None, build_store_limits(), budget);
    if let Some(text) = text {
        store.set_configuration_secrets(text);
    }

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

/// Region and endpoint variables exposed only to the AWS providers (`aws`,
/// `awscc`). Credential discovery runs in the host and no credential-source
/// configuration crosses into the guest environment.
const AWS_ENV_ALLOWLIST: &[&str] = &["AWS_REGION", "AWS_DEFAULT_REGION", "AWS_ENDPOINT_URL"];

/// Environment variables exposed only to the GitHub provider.
const GITHUB_ENV_ALLOWLIST: &[&str] = &["GITHUB_TOKEN"];

#[derive(Debug)]
struct ProviderLabel {
    name: String,
    source: ProviderLoadSource,
}

struct ConfiguredProvider {
    label: Arc<ProviderLabel>,
    kind: ProviderKind,
}

impl ConfiguredProvider {
    fn new(identity: &ProviderLoadIdentity) -> Self {
        let kind = match identity.trust().class() {
            ProviderTrustClass::Aws => ProviderKind::Aws,
            ProviderTrustClass::GitHub => ProviderKind::GitHub,
            ProviderTrustClass::Untrusted => ProviderKind::Untrusted,
        };
        let label = Arc::new(ProviderLabel {
            name: identity.configured_name().to_string(),
            source: identity.source().clone(),
        });
        Self { label, kind }
    }
}

async fn runtime_network(
    configured: &ConfiguredProvider,
    component_imports: &ComponentImports,
    egress: ConsentedEgress,
    aws: &dyn AwsCredentialProvisioner,
) -> Result<GuestNetwork, WasmProviderLoadError> {
    let imports_aws_credentials = component_imports.imports_aws_credentials();
    match configured.kind {
        ProviderKind::Aws if !imports_aws_credentials => {
            return Err(WasmProviderLoadError::AwsCredentialsImportMissing);
        }
        ProviderKind::GitHub | ProviderKind::Untrusted if imports_aws_credentials => {
            return Err(WasmProviderLoadError::AwsCredentialsRequired);
        }
        ProviderKind::Aws | ProviderKind::GitHub | ProviderKind::Untrusted => {}
    }

    let capabilities = match configured.kind {
        ProviderKind::Aws => RuntimeCapabilities::Aws {
            credentials: aws.provision().await?,
        },
        ProviderKind::GitHub => RuntimeCapabilities::GitHub,
        ProviderKind::Untrusted => RuntimeCapabilities::Untrusted,
    };
    Ok(GuestNetwork::Runtime {
        capabilities,
        egress,
    })
}

/// A provider kind whose credential, metadata, and private-address partitions
/// the host knows about. It is classified only from the resolver-owned
/// verified-source identity, never from configured or guest-reported names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    /// AWS native-SDK and Cloud Control providers. Their guests receive only
    /// region and endpoint configuration; credentials come from the typed
    /// host import.
    Aws,
    /// The GitHub provider.
    GitHub,
    /// Any provider the host has no credential partition for (e.g. the mock
    /// provider). Receives the shared group only.
    Untrusted,
}

/// Runtime-only capabilities granted according to resolver-owned provider
/// trust. The AWS variant cannot be constructed without a credential source.
#[derive(Clone)]
pub(crate) enum RuntimeCapabilities {
    Aws {
        credentials: Arc<dyn AwsCredentialSource>,
    },
    GitHub,
    Untrusted,
}

impl fmt::Debug for RuntimeCapabilities {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aws { credentials: _ } => formatter
                .debug_struct("Aws")
                .field("credentials", &"<credential source>")
                .finish(),
            Self::GitHub => formatter.write_str("GitHub"),
            Self::Untrusted => formatter.write_str("Untrusted"),
        }
    }
}

/// Whether a provider kind may connect to a resolved private address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateAddressPolicy {
    Allow,
    Deny,
}

impl RuntimeCapabilities {
    fn kind(&self) -> ProviderKind {
        match self {
            Self::Aws { credentials: _ } => ProviderKind::Aws,
            Self::GitHub => ProviderKind::GitHub,
            Self::Untrusted => ProviderKind::Untrusted,
        }
    }

    /// Utility and credential environment partitions are classified once.
    /// All values in the credential partition enter as SecretPayload; adding
    /// another credential never requires changing the redaction collector.
    fn environment_partition(&self) -> GuestEnvironmentPartition {
        match self {
            Self::Aws { credentials: _ } => GuestEnvironmentPartition {
                public: AWS_ENV_ALLOWLIST,
                secret: &[],
            },
            Self::GitHub => GuestEnvironmentPartition {
                public: &[],
                secret: GITHUB_ENV_ALLOWLIST,
            },
            Self::Untrusted => GuestEnvironmentPartition {
                public: &[],
                secret: &[],
            },
        }
    }
    #[cfg(test)]
    fn credential_partition(&self) -> Vec<&'static str> {
        let partition = self.environment_partition();
        partition
            .public
            .iter()
            .chain(partition.secret)
            .copied()
            .collect()
    }

    /// Private-address policy for this kind. Metadata is withheld
    /// unconditionally before this kind-specific policy is considered.
    fn private_address_policy(&self) -> PrivateAddressPolicy {
        match self {
            Self::Aws { credentials: _ } => PrivateAddressPolicy::Allow,
            Self::GitHub | Self::Untrusted => PrivateAddressPolicy::Deny,
        }
    }
}

/// Resolve the set of allowlisted env-var names a given provider's guest
/// may receive: the shared group plus that kind's own credential
/// partition.
///
/// `None` is the kind-less info/schemas instance, which calls neither
/// `initialize` nor any credentialed operation and therefore gets the shared
/// group only.
#[cfg(test)]
fn env_keys_for_capabilities(capabilities: Option<&RuntimeCapabilities>) -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = SHARED_ENV_ALLOWLIST.to_vec();
    if let Some(capabilities) = capabilities {
        keys.extend(capabilities.credential_partition());
    }
    keys
}

/// Build a WASI context that only exposes the env vars allowlisted for
/// `provider_kind` (its credential partition plus the shared group).
///
/// See [`env_keys_for_capabilities`] for the partitioning rule.
#[cfg(test)]
fn build_sandboxed_wasi_ctx(capabilities: Option<&RuntimeCapabilities>) -> WasiCtx {
    build_sandboxed_wasi_ctx_with(capabilities, |key| std::env::var(key).ok())
}

#[cfg(test)]
fn build_sandboxed_wasi_ctx_with(
    capabilities: Option<&RuntimeCapabilities>,
    env_value: impl FnMut(&str) -> Option<String>,
) -> WasiCtx {
    let engine = Engine::default();
    let inputs = carina_core::provider::ProviderPlan::state_only()
        .session("", None, None)
        .guest_inventory();
    let store = carina_core::provider_text::GuestStoreData::new_store(&engine, (), inputs);
    build_sandboxed_wasi_ctx_with_stderr(
        store.data(),
        capabilities,
        env_value,
        guest_stderr::GuestStderr::new(std::io::stderr()),
    )
}

struct GuestEnvironmentPartition {
    public: &'static [&'static str],
    secret: &'static [&'static str],
}
enum GuestEnvironmentValue {
    Public(String),
    Secret(GuestCredential),
}
fn handoff_credential<T>(
    store: &carina_core::provider_text::GuestStoreData<T>,
    value: &GuestCredential,
) -> String {
    match store
        .input_inventory()
        .handoff(store.credential_handoff(value))
        .expect("host credentials are concrete strings")
    {
        carina_provider_protocol::types::Value::String(value) => value,
        _ => unreachable!("host credentials are constructed as strings"),
    }
}
fn build_sandboxed_wasi_ctx_with_stderr<T>(
    store: &carina_core::provider_text::GuestStoreData<T>,
    capabilities: Option<&RuntimeCapabilities>,
    mut env_value: impl FnMut(&str) -> Option<String>,
    stderr: guest_stderr::GuestStderr,
) -> WasiCtx {
    let mut builder = WasiCtxBuilder::new();
    builder.stderr(stderr);
    let partition = capabilities
        .map(RuntimeCapabilities::environment_partition)
        .unwrap_or(GuestEnvironmentPartition {
            public: &[],
            secret: &[],
        });
    let entries = SHARED_ENV_ALLOWLIST
        .iter()
        .chain(partition.public)
        .map(|key| (*key, false))
        .chain(partition.secret.iter().map(|key| (*key, true)));
    for (key, secret) in entries {
        if let Some(value) = env_value(key) {
            let value = if secret {
                GuestEnvironmentValue::Secret(guest_credential(value))
            } else {
                GuestEnvironmentValue::Public(value)
            };
            match value {
                GuestEnvironmentValue::Public(value) => {
                    builder.env(key, &value);
                }
                GuestEnvironmentValue::Secret(value) => {
                    builder.env(key, handoff_credential(store, &value));
                }
            }
        }
    }
    builder.build()
}

async fn create_instance_with_http(
    engine: &Engine,
    component: &Component,
    label: Arc<ProviderLabel>,
    network: GuestNetwork,
    budget: GuestCallBudget,
    http_test_route: Option<HttpTestRoute>,
    text: Option<carina_core::provider::ProviderValidationInputs>,
) -> Result<(GuestStore, WasmBindings), WasmBindingCreationError> {
    let linker = create_linker(engine, &network, InstantiationWorld::HttpEnabled)?;

    let mut store = GuestStore::new_with_http_test_route(
        engine,
        label,
        network,
        Some(WasiHttpCtx::new()),
        build_store_limits(),
        budget,
        http_test_route,
    );

    if let Some(text) = text {
        store.set_configuration_secrets(text);
    }
    let bindings = WasmBindings::instantiate_http(&mut store, component, &linker)
        .await
        .map_err(|error| {
            error.instantiation_context("Failed to instantiate WASM component (HTTP)")
        })?;

    Ok((store, bindings))
}

/// Instantiate only the stable provider metadata interface.
///
/// The store is deliberately kind-less: it receives neither AWS nor GitHub
/// credentials, and its HTTP hook rejects every destination. Linking the same
/// host imports as the HTTP runtime world lets current and released providers
/// instantiate without granting metadata calls credentials or network access.
async fn create_schema_only_bindings(
    engine: &Engine,
    component: &Component,
    label: Arc<ProviderLabel>,
) -> Result<(GuestStore, SchemaOnlyBindings), WasmProviderLoadError> {
    let budget = GuestCallBudget::PRODUCTION;
    let network = GuestNetwork::SchemaOnly;
    let linker =
        create_linker(engine, &network, InstantiationWorld::HttpEnabled).map_err(|failure| {
            single_attempt_load_error(engine, component, InstantiationWorld::HttpEnabled, failure)
        })?;
    let mut store = GuestStore::new_with_http_test_route(
        engine,
        label,
        network,
        Some(WasiHttpCtx::new()),
        build_store_limits(),
        budget,
        None,
    );
    let bindings = SchemaOnlyBindings::instantiate(&mut store, component, &linker)
        .await
        .map_err(|failure| {
            single_attempt_load_error(engine, component, InstantiationWorld::HttpEnabled, failure)
        })?;
    Ok((store, bindings))
}

/// Instantiate exactly the world selected when the factory was loaded.
///
/// Runtime per-binding instances and validation-instance rebuilds share this
/// selection point, so neither path probes or retries a different world.
#[allow(clippy::too_many_arguments)]
async fn create_instance_for_world(
    engine: &Engine,
    component: &Component,
    label: Arc<ProviderLabel>,
    network: GuestNetwork,
    world: InstantiationWorld,
    budget: GuestCallBudget,
    http_test_route: Option<HttpTestRoute>,
    text: Option<carina_core::provider::ProviderValidationInputs>,
) -> Result<LiveInstance, WasmBindingCreationError> {
    let (store, bindings) = match world {
        InstantiationWorld::HttpEnabled => {
            create_instance_with_http(
                engine,
                component,
                label,
                network,
                budget,
                http_test_route,
                text,
            )
            .await
        }
        InstantiationWorld::Basic => {
            create_instance(engine, component, label, network, budget, text).await
        }
    }?;
    Ok(LiveInstance::new(store, bindings, world))
}

#[allow(clippy::too_many_arguments)]
async fn create_runtime_instance(
    engine: &Engine,
    component: &Component,
    label: Arc<ProviderLabel>,
    network: GuestNetwork,
    world: InstantiationWorld,
    budget: GuestCallBudget,
    http_test_route: Option<HttpTestRoute>,
    text: Option<carina_core::provider::ProviderValidationInputs>,
) -> Result<LiveInstance, WasmProviderInstanceError> {
    let result = create_instance_for_world(
        engine,
        component,
        label,
        network,
        world,
        budget,
        http_test_route,
        text,
    )
    .await;
    result.map_err(|failure| {
        map_binding_creation_error(
            failure,
            |failure| {
                WasmProviderInstanceError::Instantiation(
                    ProviderInstantiationError::from_single_attempt(
                        engine, component, world, failure,
                    ),
                )
            },
            WasmProviderInstanceError::InfoCall,
            |operation, source| WasmProviderInstanceError::WallBudgetExceeded { operation, source },
            WasmProviderInstanceError::ProtocolVersion,
        )
    })
}

fn info_call_load_error(error: wasmtime::Error) -> WasmProviderLoadError {
    WasmProviderLoadError::metadata(
        "Failed to call info()",
        WasmGuestCallError {
            context: None,
            source: error,
        },
    )
}

fn single_attempt_load_error(
    engine: &Engine,
    component: &Component,
    world: InstantiationWorld,
    failure: WasmBindingCreationError,
) -> WasmProviderLoadError {
    map_binding_creation_error(
        failure,
        |failure| {
            ProviderInstantiationError::from_single_attempt(engine, component, world, failure)
                .into()
        },
        info_call_load_error,
        wall_budget_load_error,
        WasmProviderLoadError::from,
    )
}

/// Output of `create_instance_auto`: one callable instance tagged with the
/// world whose linker instantiated it.
type CreateInstanceResult = Result<LiveInstance, WasmProviderLoadError>;

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
fn create_instance_auto(rebuild: &mut RebuildContext) -> BoxFuture<'_, CreateInstanceResult> {
    Box::pin(async move {
        let map_load_error = |failure| {
            map_binding_creation_error(
                failure,
                Result::<wasmtime::Error, WasmProviderLoadError>::Ok,
                |error| Err(info_call_load_error(error)),
                |operation, source| Err(wall_budget_load_error(operation, source)),
                |message| Err(message.into()),
            )
        };
        match rebuild.create_instance().await {
            Ok(instance) => Ok(instance),
            Err(failure) => match map_load_error(failure) {
                Err(error) => Err(error),
                Ok(http_err) => {
                    rebuild.world = InstantiationWorld::Basic;
                    match rebuild.create_instance().await {
                        Ok(instance) => Ok(instance),
                        Err(failure) => match map_load_error(failure) {
                            Err(error) => Err(error),
                            Ok(basic_err) => Err(ProviderInstantiationError::from_attempts(
                                &rebuild.engine,
                                &rebuild.component,
                                http_err,
                                basic_err,
                            )
                            .into()),
                        },
                    }
                }
            },
        }
    })
}

// -- Shared WASM instance state --

/// A validation operation whose input participates in rebuild suspension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ValidationOperation {
    ValidateConfig,
    ValidateCustomType,
}

impl ValidationOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ValidateConfig => "validate_config",
            Self::ValidateCustomType => "validate_custom_type",
        }
    }
}

/// Secret-safe identity of one validation call.
///
/// Provider bytes are fixed for the lifetime of a factory, so the operation
/// and keyed SHA-256 digest of the already-converted WIT arguments are
/// sufficient to distinguish validation contexts. No cleartext validation
/// value is retained.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ValidationContextFingerprint {
    operation: ValidationOperation,
    keyed_input_sha256: [u8; 32],
}

impl fmt::Debug for ValidationContextFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidationContextFingerprint")
            .field("operation", &self.operation)
            .field("digest", &"<redacted>")
            .finish()
    }
}

type ValidationFingerprintHasher = Hmac<Sha256>;

fn validation_fingerprint_hasher(
    key: &[u8; 32],
    operation: ValidationOperation,
) -> ValidationFingerprintHasher {
    let mut hasher = ValidationFingerprintHasher::new_from_slice(key)
        .expect("HMAC-SHA256 accepts every 32-byte validation fingerprint key");
    hash_validation_bytes(&mut hasher, operation.as_str().as_bytes());
    hasher
}

fn random_validation_fingerprint_key() -> [u8; 32] {
    let mut key = [0_u8; 32];
    getrandom::fill(&mut key)
        .expect("the host must provide randomness for validation fingerprints");
    key
}

fn hash_validation_bytes(hasher: &mut ValidationFingerprintHasher, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).expect("validation input length must fit in u64");
    hasher.update(&len.to_be_bytes());
    hasher.update(bytes);
}

/// Why a component instance can no longer be entered safely.
///
/// The originating operation and typed Wasmtime trap classification are kept
/// separately from the rejected follow-up operation, so fail-fast diagnostics
/// never misattribute an earlier failure to a later guest call.
#[derive(Clone)]
enum PoisonReason {
    /// A guest future was cancelled before Wasmtime returned a result.
    Cancelled,
    /// The host observed the wall-clock deadline while the guest was in flight.
    WallClockTimeout { operation: &'static str },
    /// Wasmtime returned a component trap.
    Trap {
        operation: &'static str,
        trap: Option<wasmtime::Trap>,
        root_cause: String,
        secrets: Option<RedactionSet>,
        before_request_root: Option<DiagnosticText>,
        root_is_trap: bool,
    },
}

impl fmt::Debug for PoisonReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Cancelled"),
            Self::WallClockTimeout { operation } => f
                .debug_struct("WallClockTimeout")
                .field("operation", operation)
                .finish(),
            Self::Trap {
                operation, trap, ..
            } => f
                .debug_struct("Trap")
                .field("operation", operation)
                .field("trap", trap)
                .field("root_cause", &"<provider text>")
                .finish_non_exhaustive(),
        }
    }
}

impl PoisonReason {
    fn capture_text(&self) -> String {
        match self {
            Self::Trap {
                operation,
                trap,
                root_cause,
                root_is_trap,
                secrets,
                ..
            } => {
                let context = format!("WASM provider instance unusable after trap in {operation}");
                if secrets.is_none() {
                    return format!("{context}: {root_cause}");
                }
                match trap {
                    Some(trap) if *root_is_trap => format!("{context}: {trap}"),
                    Some(trap) => format!("{context}: {trap}: {root_cause}"),
                    None => format!("{context}: {root_cause}"),
                }
            }
            _ => self.to_string(),
        }
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
                secrets,
                before_request_root,
                ..
            } => {
                let context = format!("WASM provider instance unusable after trap in {operation}");
                if secrets.is_some() {
                    f.write_str(&context)
                } else {
                    DiagnosticText::new(
                        [
                            context.as_str(),
                            ": ",
                            before_request_root
                                .as_ref()
                                .expect("metadata trap has terminal text")
                                .as_str(),
                        ]
                        .concat(),
                    )
                    .fmt(f)
                }
            }
        }
    }
}

/// A callable component instance. Store and bindings live and die together,
/// so a poisoned instance slot has neither object available for a guest call.
struct LiveInstance {
    store: GuestStore,
    bindings: WasmBindings,
    world: InstantiationWorld,
}

struct InitializedWasmInstance {
    instance: LiveInstance,
    reference_scope: proto::ReferenceScope,
}

impl LiveInstance {
    fn new(store: GuestStore, bindings: WasmBindings, world: InstantiationWorld) -> Self {
        Self {
            store,
            bindings,
            world,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn instantiation_world(&self) -> InstantiationWorld {
        self.world
    }
}

/// The complete health state protected by a configured provider instance.
///
/// There is deliberately no representation containing a poisoned store or
/// bindings: poisoning replaces `Live` wholesale and drops both while the
/// mutex remains held.
enum FailFastSlot {
    Live(Box<LiveInstance>),
    Poisoned { reason: PoisonReason },
}

/// Exclusive access to a [`LiveInstance`]. This guard can only be constructed
/// after matching [`FailFastSlot::Live`].
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
    slot: Option<tokio::sync::MutexGuard<'a, FailFastSlot>>,
}

impl<'a> LiveInstanceGuard<'a> {
    fn with_secrets(mut self, secrets: carina_core::provider::ProviderValidationInputs) -> Self {
        let slot = self.slot.as_mut().expect("armed instance guard");
        if let FailFastSlot::Live(live) = &mut **slot {
            live.store.set_secrets(secrets);
        }
        self
    }

    fn from_slot(
        slot: tokio::sync::MutexGuard<'a, FailFastSlot>,
        operation: &'static str,
    ) -> ProviderResult<Self> {
        match &*slot {
            FailFastSlot::Live(_) => {}
            FailFastSlot::Poisoned { reason } => {
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
        *slot = FailFastSlot::Poisoned { reason };
    }

    fn release(mut self) {
        drop(
            self.slot
                .take()
                .expect("a released LiveInstanceGuard always owns its slot"),
        );
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
        let FailFastSlot::Live(live) = &mut **slot else {
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
                self.release();
                Ok(value)
            }
            Err(error) => {
                let secrets = store.known_secrets();
                let reason = PoisonReason::from_trap(operation, &error, secrets.as_ref());
                self.poison(reason);
                Err(wasm_trap_provider_error(operation, error))
            }
        }
    }

    async fn call_create<U, F>(
        self,
        operation: &'static str,
        id: ResourceId,
        started: StartedGuestCreateCall,
        f: F,
    ) -> Result<ReturnedGuestCreateCall<U>, CreateFailure>
    where
        U: 'static,
        F: AsyncFnOnce(&WasmBindings, &mut Store<HostState>) -> wasmtime::Result<U>,
    {
        let mut active = ActiveGuestCreateCall::new(self, id, started);
        let (call, secrets) = {
            let guard = active
                .guard
                .as_mut()
                .expect("an active guest create call owns its instance guard");
            let slot = guard
                .slot
                .as_mut()
                .expect("an armed LiveInstanceGuard always owns its slot");
            match &mut **slot {
                FailFastSlot::Live(live) => {
                    let (store, bindings) = (&mut live.store, &live.bindings);
                    let call = store.call(|store| f(bindings, store)).await;
                    (call, store.known_secrets())
                }
                FailFastSlot::Poisoned { reason: _reason } => {
                    unreachable!("LiveInstanceGuard can only hold a live slot")
                }
            }
        };
        let call = match call {
            Ok(call) => call,
            Err(error) => {
                active.poison(PoisonReason::WallClockTimeout { operation });
                let error = wall_budget_provider_error(operation, error);
                return Err(active.classify_failure(error).await);
            }
        };
        match call {
            Ok(value) => Ok(active.finish(value)),
            Err(error) => {
                let reason = PoisonReason::from_trap(operation, &error, secrets.as_ref());
                active.poison(reason);
                Err(active
                    .classify_failure(wasm_trap_provider_error(operation, error))
                    .await)
            }
        }
    }
}

impl Drop for LiveInstanceGuard<'_> {
    fn drop(&mut self) {
        if let Some(slot) = &mut self.slot {
            **slot = FailFastSlot::Poisoned {
                reason: PoisonReason::Cancelled,
            };
        }
    }
}

struct ActiveGuestCreateCall<'a> {
    guard: Option<LiveInstanceGuard<'a>>,
    id: ResourceId,
    started: Option<StartedGuestCreateCall>,
}

impl<'a> ActiveGuestCreateCall<'a> {
    fn new(
        mut guard: LiveInstanceGuard<'a>,
        id: ResourceId,
        started: StartedGuestCreateCall,
    ) -> Self {
        let recorder = started.egress_recorder();
        let slot = guard
            .slot
            .as_mut()
            .expect("an armed LiveInstanceGuard always owns its slot");
        match &mut **slot {
            FailFastSlot::Live(live) => live.store.begin_create_call(recorder),
            FailFastSlot::Poisoned { reason: _reason } => {
                unreachable!("LiveInstanceGuard can only hold a live slot")
            }
        }
        Self {
            guard: Some(guard),
            id,
            started: Some(started),
        }
    }

    fn poison(&mut self, reason: PoisonReason) {
        self.guard
            .take()
            .expect("an active guest create call owns its instance guard")
            .poison(reason);
    }

    async fn classify_failure(mut self, error: ProviderError) -> CreateFailure {
        let id = self.id.clone();
        self.started
            .take()
            .expect("an active guest create call owns its call witness")
            .finish()
            .failed(error.for_resource(id))
            .await
            .classify()
    }

    fn finish<U>(mut self, value: U) -> ReturnedGuestCreateCall<U> {
        let mut guard = self
            .guard
            .take()
            .expect("an active guest create call owns its instance guard");
        let slot = guard
            .slot
            .as_mut()
            .expect("an armed LiveInstanceGuard always owns its slot");
        match &mut **slot {
            FailFastSlot::Live(live) => live.store.finish_create_call(),
            FailFastSlot::Poisoned { reason: _reason } => {
                unreachable!("LiveInstanceGuard can only hold a live slot")
            }
        }
        let call = self
            .started
            .take()
            .expect("an active guest create call owns its call witness")
            .finish();
        guard.release();
        ReturnedGuestCreateCall { call, value }
    }
}

impl Drop for ActiveGuestCreateCall<'_> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(guard);
        }
        if let Some(started) = self.started.take() {
            drop(started);
        }
    }
}

struct ReturnedGuestCreateCall<T> {
    call: FinishedGuestCreateCall,
    value: T,
}

impl<T> ReturnedGuestCreateCall<T> {
    async fn decode<U>(
        self,
        decode: impl FnOnce(T) -> Result<U, ProviderError>,
    ) -> Result<DecodedGuestCreateCall<U>, CreateFailure> {
        self.call.decode(self.value, decode).await
    }
}

/// Shared configured provider/normalizer instance with fail-fast poisoning.
///
/// A trap, timeout, or cancellation permanently changes its slot to
/// [`FailFastSlot::Poisoned`]. Later plan/apply operations receive the original
/// reason and never continue on a freshly configured instance (carina#3106).
fn same_inventory(
    left: &Option<carina_core::provider::ProviderValidationInputs>,
    right: &Option<carina_core::provider::ProviderValidationInputs>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.redaction_set().shares_inventory(right.redaction_set()),
        _ => false,
    }
}

struct FailFastInstance {
    #[cfg(any(test, feature = "test-utils"))]
    observer: Option<PollObserverForTest>,
    text_context: Option<carina_core::provider::ProviderValidationInputs>,
    slot: Mutex<FailFastSlot>,
    reference_scope: proto::ReferenceScope,
}

impl FailFastInstance {
    fn new(initialized: InitializedWasmInstance) -> Self {
        let InitializedWasmInstance {
            instance,
            reference_scope,
        } = initialized;
        Self {
            #[cfg(any(test, feature = "test-utils"))]
            observer: None,
            text_context: instance.store.configuration_secrets(),
            slot: Mutex::new(FailFastSlot::Live(Box::new(instance))),
            reference_scope,
        }
    }

    fn reference_scope(&self) -> &proto::ReferenceScope {
        &self.reference_scope
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn with_observer_for_test(mut self, observer: Option<PollObserverForTest>) -> Self {
        if let FailFastSlot::Live(live) = self.slot.get_mut() {
            live.store.set_deadline_observer_for_test(observer.clone());
        }
        self.observer = observer;
        self
    }

    async fn acquire(&self, operation: &'static str) -> ProviderResult<LiveInstanceGuard<'_>> {
        #[cfg(any(test, feature = "test-utils"))]
        if let Some(observer) = &self.observer {
            observer(PollEventForTest::StoreQueued { operation });
        }
        let guard = LiveInstanceGuard::from_slot(self.slot.lock().await, operation)?;
        #[cfg(any(test, feature = "test-utils"))]
        if let Some(observer) = &self.observer {
            observer(PollEventForTest::StoreAcquired { operation });
        }
        let context = self.text_context.clone();
        Ok(match context {
            Some(context) => guard.with_secrets(context),
            None => guard,
        })
    }
}

const MAX_VALIDATION_INPUT_HEALTH_ENTRIES: usize = 64;

/// Timeout history for one secret-safe validation input fingerprint.
#[derive(Clone, Debug)]
enum InputHealth {
    TimedOutOnce,
    Suspended { reason: PoisonReason },
}

/// Whether the reusable validation instance is safe to enter.
enum ValidationInstanceState {
    Live(Box<LiveInstance>),
    Poisoned { reason: PoisonReason },
}

/// Complete mutex-protected policy state for factory validation.
///
/// Instance health and per-input timeout history deliberately live in one
/// state object so rebuilding or successfully validating another input cannot
/// reset a suspended input's history.
struct RebuildableState {
    inputs: carina_core::provider_text::GuestInputInventory,
    instance: ValidationInstanceState,
    input_health: IndexMap<ValidationContextFingerprint, InputHealth>,
}

impl RebuildableState {
    fn new(live: LiveInstance) -> Self {
        Self {
            inputs: carina_core::provider::ProviderPlan::state_only()
                .session("", None, None)
                .guest_inventory(),
            instance: ValidationInstanceState::Live(Box::new(live)),
            input_health: IndexMap::new(),
        }
    }

    fn suspended_reason(&self, context: &ValidationContextFingerprint) -> Option<&PoisonReason> {
        match self.input_health.get(context) {
            Some(InputHealth::Suspended { reason }) => Some(reason),
            Some(InputHealth::TimedOutOnce) | None => None,
        }
    }

    fn record_wall_clock_timeout(
        &mut self,
        context: ValidationContextFingerprint,
        reason: PoisonReason,
    ) {
        let previous = self.input_health.shift_remove(&context);
        let health = match previous {
            Some(InputHealth::TimedOutOnce | InputHealth::Suspended { .. }) => {
                InputHealth::Suspended {
                    reason: reason.clone(),
                }
            }
            None => InputHealth::TimedOutOnce,
        };
        if self.input_health.len() == MAX_VALIDATION_INPUT_HEALTH_ENTRIES {
            self.input_health.shift_remove_index(0);
        }
        self.input_health.insert(context, health);
        self.instance = ValidationInstanceState::Poisoned { reason };
    }

    fn record_success(&mut self, context: &ValidationContextFingerprint) {
        self.input_health.shift_remove(context);
    }
}

fn suspended_validation_error(
    reason: &PoisonReason,
    context: &ValidationContextFingerprint,
) -> ProviderError {
    let original_operation = match reason {
        PoisonReason::WallClockTimeout { operation, .. } => *operation,
        PoisonReason::Cancelled | PoisonReason::Trap { .. } => context.operation(),
    };
    carina_core::provider::HostError::timeout(format!(
        "provider validation is suspended after repeated wall-clock timeouts for this input in \
         {original_operation}; changing this value (or restarting with a new provider version) \
         resumes validation"
    ))
    .with_host_cause(carina_core::provider::HostErrorCause::ValidationSuspended(
        context.suspended_validation_id(),
    ))
    .into()
}

/// Exclusive, one-call access to a rebuildable validation instance.
///
/// Like [`LiveInstanceGuard`], this guard owns the slot lock through the guest
/// call and replaces the live state before releasing it on timeout, trap, or
/// cancellation. This preserves the carina#3106 lock/drop ordering guarantee.
struct RebuildableLiveInstanceGuard<'a> {
    state: Option<tokio::sync::MutexGuard<'a, RebuildableState>>,
    context: ValidationContextFingerprint,
}

impl<'a> RebuildableLiveInstanceGuard<'a> {
    fn with_secrets(mut self, secrets: carina_core::provider::ProviderValidationInputs) -> Self {
        let state = self.state.as_mut().expect("armed validation guard");
        if let ValidationInstanceState::Live(live) = &mut state.instance {
            live.store.set_secrets(secrets);
        }
        self
    }

    fn from_state(
        state: tokio::sync::MutexGuard<'a, RebuildableState>,
        context: ValidationContextFingerprint,
    ) -> ProviderResult<Self> {
        match &state.instance {
            ValidationInstanceState::Live(_) => Ok(Self {
                state: Some(state),
                context,
            }),
            ValidationInstanceState::Poisoned { reason } => {
                Err(reason.fail_fast_error(context.operation()))
            }
        }
    }

    fn poison(mut self, reason: PoisonReason) {
        let mut state = self
            .state
            .take()
            .expect("an armed RebuildableLiveInstanceGuard always owns its state");
        state.instance = ValidationInstanceState::Poisoned { reason };
    }

    fn wall_clock_timeout(mut self) {
        let mut state = self
            .state
            .take()
            .expect("an armed RebuildableLiveInstanceGuard always owns its state");
        let reason = PoisonReason::WallClockTimeout {
            operation: self.context.operation(),
        };
        state.record_wall_clock_timeout(self.context.clone(), reason);
    }

    async fn call<U, F>(mut self, f: F) -> ProviderResult<U>
    where
        U: 'static,
        F: AsyncFnOnce(&WasmBindings, &mut Store<HostState>) -> wasmtime::Result<U>,
    {
        let (call, secrets) = {
            let state = self
                .state
                .as_mut()
                .expect("an armed RebuildableLiveInstanceGuard always owns its state");
            let ValidationInstanceState::Live(live) = &mut state.instance else {
                unreachable!("RebuildableLiveInstanceGuard can only hold a live instance");
            };
            let (store, bindings) = (&mut live.store, &live.bindings);
            let call = store.call(|store| f(bindings, store)).await;
            (call, store.known_secrets())
        };
        let call = match call {
            Ok(call) => call,
            Err(error) => {
                let operation = self.context.operation();
                self.wall_clock_timeout();
                return Err(wall_budget_provider_error(operation, error));
            }
        };
        match call {
            Ok(value) => {
                let mut state = self
                    .state
                    .take()
                    .expect("a successful guest call must still own its state");
                state.record_success(&self.context);
                drop(state);
                Ok(value)
            }
            Err(error) => {
                let operation = self.context.operation();
                let reason = PoisonReason::from_trap(operation, &error, secrets.as_ref());
                self.poison(reason);
                Err(wasm_trap_provider_error(operation, error))
            }
        }
    }
}

impl Drop for RebuildableLiveInstanceGuard<'_> {
    fn drop(&mut self) {
        if let Some(state) = &mut self.state {
            state.instance = ValidationInstanceState::Poisoned {
                reason: PoisonReason::Cancelled,
            };
        }
    }
}

struct RebuildContext {
    engine: Engine,
    component: Component,
    label: Arc<ProviderLabel>,
    network: GuestNetwork,
    world: InstantiationWorld,
    budget: GuestCallBudget,
    http_test_route: Option<HttpTestRoute>,
}

impl RebuildContext {
    fn new(
        engine: Engine,
        component: Component,
        label: Arc<ProviderLabel>,
        budget: GuestCallBudget,
    ) -> Self {
        Self {
            engine,
            component,
            label,
            network: GuestNetwork::SchemaOnly,
            world: InstantiationWorld::HttpEnabled,
            budget,
            http_test_route: None,
        }
    }

    async fn create_instance(&self) -> Result<LiveInstance, WasmBindingCreationError> {
        create_instance_for_world(
            &self.engine,
            &self.component,
            Arc::clone(&self.label),
            self.network.clone(),
            self.world,
            self.budget,
            self.http_test_route.clone(),
            None,
        )
        .await
    }
}

/// A failed validation-instance rebuild, retaining both why the previous
/// instance was unusable and the typed failure from creating its replacement.
#[derive(Debug)]
struct WasmProviderRebuildError {
    previous_poison: PoisonReason,
    load_error: WasmProviderLoadError,
}

impl fmt::Display for WasmProviderRebuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "previous poison reason: {}",
            self.previous_poison.capture_text()
        )
    }
}

impl std::error::Error for WasmProviderRebuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.load_error)
    }
}

fn rebuild_provider_error(
    previous_poison: PoisonReason,
    load_error: WasmProviderLoadError,
) -> ProviderError {
    #[cfg(test)]
    tests::record_rebuild_load_probe(&load_error);
    let timed_out = load_error.is_wall_budget_timeout();
    let error = WasmProviderRebuildError {
        previous_poison,
        load_error,
    };
    if timed_out {
        ProviderError::timeout_caused_by(
            "Failed to rebuild WASM provider validation instance",
            error,
        )
    } else {
        ProviderError::internal_caused_by(
            "Failed to rebuild WASM provider validation instance",
            error,
        )
    }
}

/// Factory-validation instance that rebuilds after poisoning and independently
/// tracks wall-clock timeout history for each validation input.
///
/// Acquisition holds the slot mutex while recreating the instance from the
/// already verified component and the same engine, selected world, and call
/// budget. A failed rebuild leaves the slot poisoned, preserves the typed load
/// error, and is retried by the next non-suspended validation call. A suspended
/// input fails fast without inspecting or rebuilding the instance; other inputs
/// continue to rebuild and use the shared instance normally.
const VALIDATION_SECRET_VALUE_LIMIT: usize = 256;

struct RebuildableInstance {
    state: Mutex<RebuildableState>,
    rebuild: RebuildContext,
    fingerprint_key: [u8; 32],
    #[cfg(test)]
    additional_instantiations: std::sync::atomic::AtomicUsize,
}

impl RebuildableInstance {
    fn new(live: LiveInstance, rebuild: RebuildContext) -> Self {
        Self {
            state: Mutex::new(RebuildableState::new(live)),
            rebuild,
            fingerprint_key: random_validation_fingerprint_key(),
            #[cfg(test)]
            additional_instantiations: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    async fn acquire(
        &self,
        context: ValidationContextFingerprint,
    ) -> ProviderResult<RebuildableLiveInstanceGuard<'_>> {
        self.acquire_in_session(context, None).await
    }
    async fn acquire_in_session(
        &self,
        context: ValidationContextFingerprint,
        session: Option<carina_core::provider::ProviderValidationInputs>,
    ) -> ProviderResult<RebuildableLiveInstanceGuard<'_>> {
        let mut state = self.state.lock().await;
        let reset_inventory = session.as_ref().is_some_and(|inputs| {
            state
                .inputs
                .would_exceed_secret_limit(inputs, VALIDATION_SECRET_VALUE_LIMIT)
        });
        if let Some(reason) = state.suspended_reason(&context) {
            if let Some(inputs) = &session {
                state.inputs.publish_to(inputs);
            }
            return Err(suspended_validation_error(reason, &context));
        }
        // Forget earlier inputs only after replacing the component that could
        // remember them. Acquisition failures publish earlier coverage to the
        // caller without retaining inputs that never entered this component.
        let previous_poison = match &state.instance {
            ValidationInstanceState::Live(_) => None,
            ValidationInstanceState::Poisoned { reason } => Some(reason.clone()),
        };
        if previous_poison.is_some() || reset_inventory {
            #[cfg(test)]
            self.additional_instantiations
                .fetch_add(1, Ordering::Relaxed);
            let live = self.rebuild.create_instance().await.map_err(|failure| {
                if let Some(inputs) = &session {
                    state.inputs.publish_to(inputs);
                }
                let error = single_attempt_load_error(
                    &self.rebuild.engine,
                    &self.rebuild.component,
                    self.rebuild.world,
                    failure,
                );
                match &previous_poison {
                    Some(reason) => rebuild_provider_error(reason.clone(), error),
                    _ => ProviderError::internal_caused_by(
                        "Failed to create WASM provider validation instance",
                        error,
                    ),
                }
            })?;
            if reset_inventory && let Some(inputs) = &session {
                state.inputs = inputs.guest_inventory();
            }
            state.instance = ValidationInstanceState::Live(Box::new(live));
            if let Some(reason) = previous_poison {
                log::warn!(
                    "rebuilt poisoned WASM provider validation instance; previous poison reason: {reason}"
                );
            }
        }

        let _inputs = session.as_ref().map(|inputs| state.inputs.observe(inputs));
        let inventory = state.inputs.clone();
        if let ValidationInstanceState::Live(live) = &mut state.instance {
            live.store.set_input_inventory(inventory);
        }
        let guard = RebuildableLiveInstanceGuard::from_state(state, context)?;
        Ok(match session {
            Some(session) => guard.with_secrets(session),
            None => guard,
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    async fn live_world(&self) -> Option<InstantiationWorld> {
        match &self.state.lock().await.instance {
            ValidationInstanceState::Live(live) => Some(live.instantiation_world()),
            ValidationInstanceState::Poisoned { reason: _ } => None,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn set_budget(&mut self, budget: GuestCallBudget) {
        self.rebuild.budget = budget;
        match &mut self.state.get_mut().instance {
            ValidationInstanceState::Live(live) => live.store.set_budget(budget),
            ValidationInstanceState::Poisoned { reason: _ } => {}
        }
    }
}

// -- WasmProviderFactory --

enum CachedComponentAcquisition {
    Hit(Component),
    Miss,
}

type ProviderConfigCompletions = HashMap<String, Vec<CompletionValue>>;

#[cfg(any(test, feature = "test-utils"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollEventForTest {
    StoreQueued {
        operation: &'static str,
    },
    StoreAcquired {
        operation: &'static str,
    },
    GuestCallDeadlineCreated {
        wall: std::time::Duration,
    },
    Sleep {
        operation: &'static str,
        delay: std::time::Duration,
    },
    Attempt {
        operation: &'static str,
    },
}

#[cfg(any(test, feature = "test-utils"))]
type PollObserverForTest = Arc<dyn Fn(PollEventForTest) + Send + Sync>;

pub struct WasmProviderFactory {
    #[cfg(any(test, feature = "test-utils"))]
    poll_observer: Option<PollObserverForTest>,
    engine: Engine,
    component: Component,
    configured: ConfiguredProvider,
    runtime_network: GuestNetwork,
    budget: GuestCallBudget,
    display_name: DiagnosticText,
    version: ProviderKey,
    schema_witness: Arc<ValidatedProviderSchemaWitness>,
    cached_config_completions: ProviderConfigCompletions,
    cached_identity_attributes: Vec<String>,
    enable_http: bool,
    http_test_route: Option<HttpTestRoute>,
    /// Reusable, rebuildable WASM instance for editor-facing validation.
    /// A poisoned instance is recreated from `engine` + `component`; each input
    /// is suspended after its second wall-clock timeout. A successful retry
    /// after one timeout clears that input's history; suspension lasts for this
    /// factory, so loading a new provider version starts with a fresh map.
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

/// Provider metadata loaded without constructing runtime protocol bindings.
///
/// This factory exists only for read-only configuration analysis. Its schema
/// and identity methods are usable, while the runtime constructor fails
/// closed. In particular, loading it calls neither `initialize` nor any CRUD
/// export, and the kind-less guest store receives no provider credentials.
pub struct SchemaOnlyWasmProviderFactory {
    name: String,
    display_name: DiagnosticText,
    version: ProviderKey,
    schema_witness: Arc<ValidatedProviderSchemaWitness>,
    cached_config_completions: ProviderConfigCompletions,
    cached_identity_attributes: Vec<String>,
}

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
        WasmDiagnostic::new(source, false),
    )
}

impl WasmProviderFactory {
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
                source: WasmDiagnostic::new(e, false),
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

    /// Load provider bytes using the supplied per-user precompile cache.
    ///
    /// The verified load binds the resolver-retained bytes to their configured
    /// name and trust classification. An artifact reporting a different name
    /// is rejected.
    ///
    /// If the cache directory cannot be created, falls back to compiling without caching.
    pub async fn from_bytes(
        load: VerifiedLoad,
        user_dirs: &UserDirs,
    ) -> Result<Self, WasmProviderLoadError> {
        (async {
            VerifiedEgressDeclaration::read(&load)?;
            let configured = ConfiguredProvider::new(load.identity());
            let wasm_bytes = load.bytes().clone();
            let cache_dir = user_dirs.compiled_wasm_cache_dir().to_path_buf();
            let load: BoxFuture<'_, Result<Self, WasmProviderLoadError>> = Box::pin(async move {
                Self::from_bytes_with_cache_dir_configured(
                    wasm_bytes.as_bytes(),
                    &cache_dir,
                    configured,
                    GuestNetworkRequest::SchemaOnly,
                )
                .await
            });
            load.await
        })
        .await
    }

    /// Load provider bytes with an explicit precompile cache directory.
    pub async fn from_bytes_with_cache_dir(
        load: VerifiedLoad,
        cache_dir: &Path,
    ) -> Result<Self, WasmProviderLoadError> {
        (async {
            VerifiedEgressDeclaration::read(&load)?;
            let configured = ConfiguredProvider::new(load.identity());
            Self::from_bytes_with_cache_dir_configured(
                load.bytes().as_bytes(),
                cache_dir,
                configured,
                GuestNetworkRequest::SchemaOnly,
            )
            .await
        })
        .await
    }

    /// Load a provider for credentialed runtime use with consent already
    /// derived from this load's declaration and lock record.
    pub async fn from_bytes_with_egress(
        load: VerifiedLoad,
        user_dirs: &UserDirs,
        locked_egress: Option<&LockedEgress>,
        aws: &dyn AwsCredentialProvisioner,
    ) -> Result<Self, WasmProviderLoadError> {
        (async {
            let declaration = VerifiedEgressDeclaration::read(&load)?;
            let egress =
                ConsentedEgress::from_lock(locked_egress, declaration, load.carina_rs_signed())?;
            let configured = ConfiguredProvider::new(load.identity());
            let wasm_bytes = load.bytes().clone();
            let cache_dir = user_dirs.compiled_wasm_cache_dir().to_path_buf();
            Self::from_bytes_with_cache_dir_configured(
                wasm_bytes.as_bytes(),
                &cache_dir,
                configured,
                GuestNetworkRequest::Runtime { egress, aws },
            )
            .await
        })
        .await
    }

    /// Test/cache-dir form of [`Self::from_bytes_with_egress`].
    #[cfg(any(test, feature = "test-utils"))]
    pub async fn from_bytes_with_cache_dir_and_egress(
        load: VerifiedLoad,
        cache_dir: &Path,
        locked_egress: Option<&LockedEgress>,
        aws: &dyn AwsCredentialProvisioner,
    ) -> Result<Self, WasmProviderLoadError> {
        (async {
            let declaration = VerifiedEgressDeclaration::read(&load)?;
            let egress =
                ConsentedEgress::from_lock(locked_egress, declaration, load.carina_rs_signed())?;
            let configured = ConfiguredProvider::new(load.identity());
            Self::from_bytes_with_cache_dir_configured(
                load.bytes().as_bytes(),
                cache_dir,
                configured,
                GuestNetworkRequest::Runtime { egress, aws },
            )
            .await
        })
        .await
    }

    async fn from_bytes_with_cache_dir_configured(
        wasm_bytes: &[u8],
        cache_dir: &Path,
        configured: ConfiguredProvider,
        network_request: GuestNetworkRequest<'_>,
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
            return Self::new_uncached_configured(wasm_bytes, configured, network_request).await;
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
        Self::bring_up_component(engine, component, epoch_ticker, configured, network_request).await
    }

    /// Load provider bytes without any precompile caching.
    #[cfg(test)]
    async fn new_uncached(
        wasm_bytes: &[u8],
        identity: ProviderLoadIdentity,
        aws: &dyn AwsCredentialProvisioner,
    ) -> Result<Self, WasmProviderLoadError> {
        let load = VerifiedLoad::for_test(wasm_bytes, identity);
        let declaration = VerifiedEgressDeclaration::read(&load)?;
        let locked_egress = test_explicit_egress(&declaration);
        let configured = ConfiguredProvider::new(load.identity());
        let egress =
            ConsentedEgress::from_lock(Some(&locked_egress), declaration, load.carina_rs_signed())?;
        Self::new_uncached_configured(
            load.bytes().as_bytes(),
            configured,
            GuestNetworkRequest::Runtime { egress, aws },
        )
        .await
    }

    async fn new_uncached_configured(
        wasm_bytes: &[u8],
        configured: ConfiguredProvider,
        network_request: GuestNetworkRequest<'_>,
    ) -> Result<Self, WasmProviderLoadError> {
        let engine = Self::create_engine()?;
        let component = Self::load_component_from_bytes(&engine, wasm_bytes)?;
        let epoch_ticker = EpochTicker::start(engine.clone());

        Self::bring_up_component(engine, component, epoch_ticker, configured, network_request).await
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
    pub fn precompile(wasm_bytes: &[u8], cwasm_path: &Path) -> Result<(), DiagnosticText> {
        let config = build_engine_config();
        let engine =
            Engine::new(&config).map_err(|e| DiagnosticText::new(format!("Engine error: {e}")))?;
        let serialized = engine
            .precompile_component(wasm_bytes)
            .map_err(|e| DiagnosticText::new(format!("Precompile error: {e:#}")))?;
        if let Some(parent) = cwasm_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| DiagnosticText::new(format!("Mkdir error: {e}")))?;
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
            std::fs::write(&tmp_path, &serialized)
                .map_err(|e| DiagnosticText::new(format!("Write error: {e}")))?;
            std::fs::rename(&tmp_path, cwasm_path)
                .map_err(|e| DiagnosticText::new(format!("Rename error: {e}")))?;
        } else {
            std::fs::write(cwasm_path, &serialized)
                .map_err(|e| DiagnosticText::new(format!("Write error: {e}")))?;
        }
        Ok(())
    }

    /// Load from a precompiled .cwasm file.
    ///
    /// # Safety
    /// The .cwasm file must have been produced by `precompile()` using the same
    /// Wasmtime version. Deserializing an untrusted or corrupted file is unsafe.
    #[cfg(any(test, feature = "test-utils"))]
    pub async fn from_precompiled(
        cwasm_path: &Path,
        identity: ProviderLoadIdentity,
    ) -> Result<Self, WasmProviderLoadError> {
        (async {
            let engine = Self::create_engine()?;
            let component = Self::deserialize_precompiled_component(&engine, cwasm_path)
                .map_err(WasmProviderLoadError::PrecompiledDeserialization)?;
            let epoch_ticker = EpochTicker::start(engine.clone());

            Self::bring_up_component(
                engine,
                component,
                epoch_ticker,
                ConfiguredProvider::new(&identity),
                GuestNetworkRequest::SchemaOnly,
            )
            .await
        })
        .await
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
    #[cfg(test)]
    async fn get_or_create_shared_instance(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
    ) -> Result<Arc<FailFastInstance>, WasmProviderInstanceError> {
        self.get_or_create_shared_instance_in_context(binding, config, None)
            .await
    }
    async fn get_or_create_shared_instance_in_context(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
        text: Option<carina_core::provider::ProviderValidationInputs>,
    ) -> Result<Arc<FailFastInstance>, WasmProviderInstanceError> {
        let key: Option<String> = binding.map(|s| s.to_string());
        let mut guard = self.shared_instances.lock().await;
        if let Some(instance) = guard.get(&key)
            && same_inventory(&instance.text_context, &text)
        {
            return Ok(Arc::clone(instance));
        }
        let initialized = self.create_initialized_instance(config, text).await?;
        let instance = FailFastInstance::new(initialized);
        #[cfg(any(test, feature = "test-utils"))]
        let instance = instance.with_observer_for_test(self.poll_observer.clone());
        let instance = Arc::new(instance);
        guard.insert(key, Arc::clone(&instance));
        Ok(instance)
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub async fn initialized_reference_scope_for_test(
        &self,
        binding: Option<&str>,
    ) -> Option<proto::ReferenceScope> {
        let key = binding.map(str::to_owned);
        let guard = self.shared_instances.lock().await;
        guard
            .get(&key)
            .map(|instance| instance.reference_scope().clone())
    }
}

impl SchemaOnlyWasmProviderFactory {
    /// Load only schema metadata from resolver-verified provider bytes.
    pub async fn from_bytes(
        load: VerifiedLoad,
        _user_dirs: &UserDirs,
    ) -> Result<Self, WasmProviderLoadError> {
        (async {
            let configured = ConfiguredProvider::new(load.identity());
            let engine = WasmProviderFactory::create_engine()?;
            // A migration check is a literal read-only operation. Do not reuse the
            // ordinary precompile cache: a cold cache would create directories and
            // publish native code even though the command promises no writes.
            let component =
                WasmProviderFactory::load_component_from_bytes(&engine, load.bytes().as_bytes())?;
            Self::bring_up_component(engine, component, configured).await
        })
        .await
    }

    pub fn verify_version(&self, constraint_raw: &str) -> Result<(), String> {
        verify_provider_version(&self.name, &self.version, constraint_raw)
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

    /// Observe polling decisions without measuring scheduler time.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_poll_observer_for_test(
        mut self,
        observer: impl Fn(PollEventForTest) + Send + Sync + 'static,
    ) -> Self {
        self.poll_observer = Some(Arc::new(observer));
        self
    }

    /// Hold the configured store lock while tests enqueue operations. No guest
    /// call (and hence no guest-call timer) has started while this lock is held.
    #[cfg(any(test, feature = "test-utils"))]
    pub async fn hold_configured_instance_for_test(
        &self,
        entered: &tokio::sync::Notify,
        release: &tokio::sync::Notify,
    ) {
        let instance = self
            .shared_instances
            .lock()
            .await
            .get(&None)
            .cloned()
            .expect("initialize the default provider before holding its store");
        let _slot = instance.slot.lock().await;
        entered.notify_one();
        release.notified().await;
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

    /// Route one exact guest HTTP hostname and port to a test server.
    ///
    /// This exists only for host/guest integration tests. The override is
    /// factory-scoped and is never available in production builds.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_http_test_route(
        mut self,
        host: impl Into<String>,
        destination: SocketAddr,
    ) -> Self {
        let route = HttpTestRoute::new(host.into(), destination);
        self.init_instance.rebuild.http_test_route = Some(route.clone());
        self.http_test_route = Some(route);
        self
    }

    /// Exposes the validation instance's actual linker world to integration
    /// tests that exercise rebuild behavior.
    #[cfg(any(test, feature = "test-utils"))]
    pub async fn validation_instance_world_for_test(&self) -> Option<&'static str> {
        self.init_instance
            .live_world()
            .await
            .map(InstantiationWorld::test_name)
    }

    pub fn version(&self) -> &str {
        self.version.as_str()
    }

    /// Verify that this provider's version satisfies the given constraint.
    pub fn verify_version(&self, constraint_raw: &str) -> Result<(), String> {
        verify_provider_version(
            self.configured.label.name.as_str(),
            &self.version,
            constraint_raw,
        )
    }
}

fn verify_provider_version(
    provider_name: &str,
    version: &ProviderKey,
    constraint_raw: &str,
) -> Result<(), String> {
    let diagnostic = |text| DiagnosticText::new(text).to_string();
    let requirement = semver::VersionReq::parse(constraint_raw).map_err(|error| {
        diagnostic(format!(
            "Invalid version constraint '{constraint_raw}': {error}"
        ))
    })?;
    let actual = semver::Version::parse(version.as_str()).map_err(|error| {
        diagnostic(format!(
            "Provider '{provider_name}' reports invalid version '{version}': {error}"
        ))
    })?;
    if requirement.matches(&actual) {
        Ok(())
    } else {
        Err(diagnostic(format!(
            "Provider '{provider_name}' version {version} does not satisfy constraint '{constraint_raw}'"
        )))
    }
}

impl ProviderFactory for SchemaOnlyWasmProviderFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        self.schema_witness.config().clone()
    }

    fn validate_config(
        &self,
        _attributes: &CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn validate_custom_type(
        &self,
        _identity: &TypeIdentity,
        _value: &CanonicalCustomTypeValue,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn create_provider(
        &self,
        _config: &carina_core::provider::ProviderSession,
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async {
            Err(carina_core::provider::HostError::internal(
                "schema-only provider metadata cannot initialize a provider runtime",
            )
            .into())
        })
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        self.schema_witness.resources().to_vec()
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        self.schema_witness.as_ref().clone()
    }

    fn identity_attributes(&self) -> Vec<&str> {
        self.cached_identity_attributes
            .iter()
            .map(String::as_str)
            .collect()
    }

    fn config_completions(&self) -> HashMap<String, Vec<CompletionValue>> {
        self.cached_config_completions.clone()
    }
}

impl ProviderFactory for WasmProviderFactory {
    fn name(&self) -> &str {
        self.configured.label.name.as_str()
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        self.schema_witness.config().clone()
    }

    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.validate_config_in_context(attributes, Some(_session.clone()))
    }

    fn validate_custom_type(
        &self,
        identity: &TypeIdentity,
        value: &CanonicalCustomTypeValue,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.validate_custom_type_in_context(identity, value, Some(_session.clone()))
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
        config: &carina_core::provider::ProviderSession,
        initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        let Some(ready) = config.configuration() else {
            return Box::pin(async {
                Err(ProviderError::invalid_input(
                    "WASM providers require checked configuration",
                ))
            });
        };
        self.create_provider_in_context(
            config.binding(),
            ready,
            initialize_outcome,
            Some(config.guest_inputs()),
        )
    }

    fn create_normalizer(
        &self,
        config: &carina_core::provider::ProviderSession,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        let Some(ready) = config.configuration() else {
            return Box::pin(async {
                Err(ProviderError::invalid_input(
                    "WASM providers require checked configuration",
                ))
            });
        };
        self.create_normalizer_in_context(config.binding(), ready, Some(config.guest_inputs()))
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        self.schema_witness.resources().to_vec()
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        self.schema_witness.as_ref().clone()
    }
}

impl WasmProviderFactory {
    fn create_provider_in_context(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
        initialize_outcome: carina_core::provider::InitializeOutcomeMint,
        text: Option<carina_core::provider::ProviderValidationInputs>,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        // The validated config witness shares its immutable payload; no deep clone.
        let config = config.clone();
        let binding = binding.map(|s| s.to_string());
        Box::pin(async move {
            // Provider init rejections still surface their user-actionable
            // message verbatim (for example, allowed_account_ids mismatch;
            // see #2407). Instantiation failures take the typed mapper path.
            let instance = self
                .get_or_create_shared_instance_in_context(binding.as_deref(), &config, text)
                .await
                .map_err(|error| {
                    error.into_provider_error_with(&self.instantiation_error_mapper)
                })?;
            let outcome = match instance.reference_scope() {
                proto::ReferenceScope::Available(values) => initialize_outcome
                    .available(values.partition(), values.region(), values.account())
                    .map_err(|error| {
                        ProviderError::internal_caused_by(
                            "validated provider reference scope became invalid",
                            error,
                        )
                    })?,
                proto::ReferenceScope::Unavailable => InitializeOutcome::unavailable(),
            };
            let provider: Box<dyn Provider> = Box::new(WasmProvider {
                #[cfg(any(test, feature = "test-utils"))]
                poll_observer: self.poll_observer.clone(),
                instance,
                name: self.configured.label.name.clone(),
                schema_witness: Arc::clone(&self.schema_witness),
            });
            Ok(outcome.with_provider(provider))
        })
    }

    fn create_normalizer_in_context(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
        text: Option<carina_core::provider::ProviderValidationInputs>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        // The validated config witness shares its immutable payload; no deep clone.
        let config = config.clone();
        let binding = binding.map(|s| s.to_string());
        Box::pin(async move {
            let instance = self
                .get_or_create_shared_instance_in_context(binding.as_deref(), &config, text)
                .await
                .map_err(|error| {
                    error.into_provider_error_with(&self.instantiation_error_mapper)
                })?;
            Ok(Box::new(WasmProviderNormalizer { instance }) as Box<dyn ProviderNormalizer>)
        })
    }
}

// -- WasmProvider --

// The sole module allowed to see generated bindings and fresh guest values.
mod guest;
#[cfg(any(test, feature = "test-utils"))]
pub use guest::decode_initialize_string_error_for_test;
use guest::*;
