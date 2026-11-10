//! Provider - Trait abstracting resource operations
//!
//! A Provider defines operations for a specific infrastructure (AWS, GCP, etc.).
//! It is responsible for converting Effects into actual API calls.

#[cfg(any(test, feature = "test-utils"))]
use crate::schema::PatternResourceSchema;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use tokio::sync::Notify;

use carina_provider_protocol::CompletedOperation;
pub use carina_provider_protocol::ReadBackEnd;
use carina_provider_protocol::types as protocol_types;
pub use carina_provider_protocol::types::{
    AppliedIdentifier, CreateProgress, EmptyAppliedIdentifier, PendingToken, ReferenceScope,
    ReferenceScopeError, ReferenceScopeField, ReferenceScopeValues, Sent,
};
pub use carina_provider_protocol::types::{
    ProviderError as WireProviderError, ProviderErrorKind as WireProviderErrorKind,
};

use crate::binding_index::SecretPreservingAttributes;
use crate::effect::PlanOp;
use crate::parser::{ProviderConfig, StateBlockAddress};
#[cfg(test)]
use crate::provider_text::OpaqueFixtureDisplay as _;
use crate::provider_text::{
    Bound, Diagnostic, DiagnosticSource, DiagnosticText, ErrorText, ProviderDiagnostic,
    ProviderErrorText, ProviderKey, ProviderText, RedactionContext, RedactionSet, Retryability,
    TextState, Unbound,
};
pub use crate::resource::ProviderDirectiveSchema;
use crate::resource::{
    CanonicalEnumValue, ConcreteValue, Directives, PartialReadMarker, PendingReRead,
    PreventDestroyResolution, PreventDestroySource, ProviderResource, Resource, ResourceId,
    SnapshotState, SnapshotValue, State, StateValueLeaf, UnavailableSecretError, Value,
    ValueString,
};
#[cfg(test)]
use crate::resource::{DataSource, RedactedSecretHash};
use crate::schema::{
    CompiledAttributeType as AttributeType, CompiledResourceSchema as ResourceSchema, LeafPath,
    RawShape, RefinedScalarKind, SchemaRegistry, Shape, TypeIdentity, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use crate::value::{
    PersistedListView, PersistedValue, PersistedValueDecodeError, PersistedView, SerializationError,
};
use crate::wait::BindingPattern;
use crate::wait::predicate::AttrPath;

#[cfg(test)]
pub(crate) use crate::provider_text::boundary::prepare_provider_session;
pub use crate::provider_text::boundary::{
    ProviderBoundary, ProviderPlan, ProviderSession, ProviderSessionBuilder,
    ProviderValidationInputs,
};

pub use crate::executor::provider_ready::{
    CanonicalProviderConfigAttributes, ProviderReady, ProviderReadyConfig, ProviderReadyDataSource,
    ProviderReadyResource,
};
pub use crate::provider_wire::{
    ProviderWireError, ProviderWirePayload, ProviderWireTarget, ResourceWirePayload,
    WireAttributes, WirePatchOp, WireReady, WireValue,
};

/// Construct a validated provider identifier for fixtures shared by workspace
/// crates.
#[cfg(test)]
pub(crate) fn fixture_applied_identifier(identifier: impl Into<String>) -> AppliedIdentifier {
    AppliedIdentifier::new(identifier).expect("fixture provider identifier must be non-empty")
}

pub type ProviderError = ProviderFailure<Unbound>;
pub type HostError = ProviderFailure<Bound>;
pub type ErrorDetail = OperationErrorDetail<Unbound>;
pub type HostErrorDetail = OperationErrorDetail<Bound>;
pub type ProviderErrorResourceId = ErrorResourceId<Unbound>;
pub type HostCreateOutcome = CreateOutcome<Bound>;
pub type HostCreateOutcomeKind = CreateOutcomeKind<Bound>;
pub type HostUpdateOutcome = UpdateOutcome<Bound>;
pub type HostUpdateOutcomeKind = UpdateOutcomeKind<Bound>;
pub type HostPartialReadDiagnostic = PartialReadDiagnostic<Bound>;
pub type HostCreateObservation = CreateObservation<Bound>;
pub type HostUnobservedReason = UnobservedReason<Bound>;
pub type HostIdentifiedCreate = IdentifiedCreate<Bound>;
pub type HostCreateFailure = CreateFailure<Bound>;
pub type HostCreateFailureKind = CreateFailureKind<Bound>;
pub type HostResumeOutcome = ResumeOutcome<Bound>;
pub type HostResumeOutcomeKind = ResumeOutcomeKind<Bound>;
pub type HostCompletedResume = CompletedResume<Bound>;
pub type HostPartialResume = PartialResume<Bound>;
pub type HostResumeFailure = ResumeFailure<Bound>;
pub type HostResumeFailureKind = ResumeFailureKind<Bound>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("provider reported a missing attribute absent from the resource schema")]
pub struct UnknownMissingAttribute;

/// Contextual metadata attached to every [`ProviderError`] variant.
///
/// Mirrors `error-detail` in `wit/types.wit`. The typed terminal `cause` stays
/// host-side (boxed `dyn Error`) and is flattened to a string when the
/// error crosses the WIT boundary.
///
/// Construct errors through [`ProviderError`]; details expose metadata on an
/// existing error, rather than a second authoring route.
///
/// ```compile_fail,E0624
/// use carina_core::provider::ErrorDetail;
/// let _ = ErrorDetail::new("message");
/// ```
#[derive(Clone)]
pub struct OperationErrorDetail<S: TextState = Unbound> {
    body: FailureBody<S>,
    /// Retry policy captured before diagnostic text is escaped or truncated.
    throttling: ThrottlingClassification,
    retryability: Retryability,
    /// Resource the error pertains to, if known.
    pub resource_id: Option<Box<ErrorResourceId<S>>>,
    /// Host-side policy marker. This is deliberately not transported over the
    /// provider protocol because guests cannot originate host lifecycle state.
    host_cause: Option<HostErrorCause>,
    /// Provider name (e.g. `"aws"`, `"awscc"`) attached when the error
    /// is raised at provider-init / `create_provider` boundary points
    /// where the name is known but no specific resource is in scope.
    /// Display ignores this field; CLI renderers can read it to label
    /// structured output.
    pub provider_name: Option<ErrorText<S>>,
    /// Service-qualified cloud-API operation that failed, e.g.
    /// `"iam.ListRoles"`, `"s3.HeadBucket"`. Populating this field —
    /// or any of the sibling `status` / `code` / `request_id`
    /// fields — flips `Display` into the multi-line, labeled render
    /// shape introduced in carina#3242; see
    /// [`ErrorDetail::has_structured_cloud_fields`] for the exact gate.
    pub operation: Option<ErrorText<S>>,
    /// HTTP status code from the cloud-API response, when the error
    /// came from an HTTP-based service call.
    pub status: Option<u16>,
    /// Application-level error code, e.g. `"AccessDenied"`,
    /// `"NoSuchBucket"`. Distinct from `status` — multiple codes can
    /// share a status (HTTP 403 covers `AccessDenied`,
    /// `InvalidAccessKeyId`, `SignatureDoesNotMatch`, etc.), so both
    /// belong in the rendered output.
    code: Option<ErrorText<S>>,
    /// Correlation id from the cloud-API response (AWS
    /// `x-amzn-RequestId`, GCP `X-Goog-Request-Id`, Azure
    /// `x-ms-request-id`, …). Operators paste this into support
    /// tickets so the provider can look up server-side logs.
    pub request_id: Option<ErrorText<S>>,
}

// A captured source owns its complete rendering. Only contextual bodies add text.
#[derive(Clone)]
enum FailureBody<S: TextState> {
    Message(ErrorText<S>),
    Captured(Diagnostic<S>),
    Context {
        context: ErrorText<S>,
        cause: Diagnostic<S>,
    },
}
impl<S: TextState> Default for FailureBody<S> {
    fn default() -> Self {
        Self::Message(ErrorText::default())
    }
}
impl<S: TextState> FailureBody<S> {
    fn message(&self) -> &ErrorText<S> {
        match self {
            Self::Message(text) => text,
            Self::Captured(source) => source.text(),
            Self::Context { context, .. } => context,
        }
    }
    fn cause(&self) -> Option<&Diagnostic<S>> {
        match self {
            Self::Message(_) => None,
            Self::Captured(source) | Self::Context { cause: source, .. } => Some(source),
        }
    }
    fn rendered_cause(&self) -> Option<&Diagnostic<S>> {
        match self {
            Self::Context { cause, .. } => Some(cause),
            Self::Message(_) | Self::Captured(_) => None,
        }
    }
}
impl FailureBody<Unbound> {
    fn bind(self, secrets: &RedactionSet, context: &RedactionContext) -> FailureBody<Bound> {
        let text =
            |text: ErrorText<Unbound>| text.into_host(secrets).with_redaction_context(context);
        let diagnostic = |source: ProviderDiagnostic| {
            source.with_secrets(secrets).with_redaction_context(context)
        };
        match self {
            Self::Message(message) => FailureBody::Message(text(message)),
            Self::Captured(source) => FailureBody::Captured(diagnostic(source)),
            Self::Context { context, cause } => FailureBody::Context {
                context: text(context),
                cause: diagnostic(cause),
            },
        }
    }
}
impl FailureBody<Bound> {
    fn host_for_state<S: TextState>(self) -> FailureBody<S> {
        match self {
            Self::Message(text) => FailureBody::Message(text.host_for_state()),
            Self::Captured(source) => FailureBody::Captured(source.host_for_state()),
            Self::Context { context, cause } => FailureBody::Context {
                context: context.host_for_state(),
                cause: cause.host_for_state(),
            },
        }
    }
}

impl<S: TextState> fmt::Debug for OperationErrorDetail<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !S::BOUND {
            return f.write_str("ErrorDetail(<provider text>)");
        }
        f.debug_struct("ErrorDetail")
            .field("message", self.body.message())
            .field("resource_id", &self.resource_id)
            .field("cause", &self.body.cause())
            .field("provider_name", &self.provider_name)
            .field("operation", &self.operation)
            .field("status", &self.status)
            .field("code", &self.code)
            .field("request_id", &self.request_id)
            .finish()
    }
}

#[derive(Debug, Default, Clone)]
struct ThrottlingClassification {
    message_or_cause: bool,
    code: bool,
    delete_dependency: bool,
    dependency_code: bool,
}

impl ThrottlingClassification {
    #[cfg(any(test, all(feature = "test-utils", not(target_arch = "wasm32"))))]
    fn from_raw(message: &str, code: Option<&str>, cause: Option<&str>) -> Self {
        Self {
            message_or_cause: has_throttling_marker(message)
                || cause.is_some_and(has_throttling_marker),
            code: code.is_some_and(has_throttling_marker),
            delete_dependency: has_delete_dependency_marker(message)
                || cause.is_some_and(has_delete_dependency_marker),
            dependency_code: code.is_some_and(has_delete_dependency_marker),
        }
    }

    fn is_throttled(&self) -> bool {
        self.message_or_cause || self.code
    }
}

pub(crate) fn has_throttling_marker(text: &str) -> bool {
    text.contains("ThrottlingException") || text.contains("Rate exceeded")
}

pub(crate) fn has_delete_dependency_marker(text: &str) -> bool {
    [
        "DependencyViolation",
        "has dependent object",
        "has a dependent object",
        "resource has dependencies",
        "mapped public address",
        "Failed to detach",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

/// Original resource identity with a sanitized diagnostic header.
#[derive(Clone)]
pub struct ErrorResourceId<S: TextState = Unbound> {
    id: ResourceId,
    text: ErrorText<S>,
}

impl<S: TextState> PartialEq for ErrorResourceId<S> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl<S: TextState> Eq for ErrorResourceId<S> {}
impl<S: TextState> ErrorResourceId<S> {
    /// Borrow the semantic identity for matching; diagnostic formatting uses
    /// the separately protected provider text.
    pub fn as_resource_id(&self) -> &ResourceId {
        &self.id
    }
}
impl<S: TextState> From<ResourceId> for ErrorResourceId<S> {
    fn from(id: ResourceId) -> Self {
        let text = ErrorText::<S>::new(format!("{}.{}", id.resource_type, id.identity_display()));
        Self { id, text }
    }
}
impl std::fmt::Display for ErrorResourceId<Bound> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.text.fmt(f)
    }
}
impl<S: TextState> std::fmt::Debug for ErrorResourceId<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.text, f)
    }
}
/// Host-only classification for errors produced by provider lifecycle policy.
///
/// Unlike [`ProviderErrorKind`], this marker never crosses the WIT boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HostErrorCause {
    /// Repeated wall-clock timeouts suspended validation for one input.
    ValidationSuspended(SuspendedValidationId),
}

/// Opaque identity of a validation input suspended by host timeout policy.
///
/// The identity is built only from a keyed, one-way fingerprint. Its bytes are
/// intentionally private, have no accessor or serialization path, and are
/// redacted from `Debug`; callers can only compare or hash it for deduplication.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SuspendedValidationId([u8; 32]);

impl SuspendedValidationId {
    /// Wrap a keyed validation-input fingerprint without exposing it again.
    #[must_use]
    pub const fn from_keyed_fingerprint(keyed_fingerprint: [u8; 32]) -> Self {
        Self(keyed_fingerprint)
    }
}

impl std::fmt::Debug for SuspendedValidationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SuspendedValidationId(<redacted>)")
    }
}

/// Stable classification of a [`ProviderError`] for callers that need policy
/// decisions without parsing diagnostic text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderErrorKind {
    InvalidInput,
    ApiError,
    NotFound,
    Timeout,
    Internal,
}

/// Structured error returned by every provider operation.
///
/// Variants mirror `provider-error` in `wit/types.wit`. Host-side code
/// can match exhaustively to dispatch retry / abort / not-found /
/// escalate strategies in a type-safe way.
///
/// Each variant boxes its [`ErrorDetail`] payload so the enum itself
/// stays at 16 bytes regardless of how many optional fields
/// `ErrorDetail` accumulates (4 new ones in carina#3242). Without the
/// box, every `Result<T, ProviderError>` return type pays the cost of
/// the largest variant on the stack — `clippy::result_large_err`
/// flags this at ≥128 bytes. Boxing now keeps the lint quiet and
/// makes future field additions to `ErrorDetail` free at the variant
/// layer.
///
/// A dynamically built message-only error cannot later acquire a cause:
///
/// ```compile_fail,E0599
/// use carina_core::provider::ProviderError;
///
/// let _ = ProviderError::with_cause(
///     ProviderError::internal(String::from("x")),
///     std::io::Error::other("x"),
/// );
/// ```
#[derive(Debug, Clone)]
pub enum ProviderFailure<S: TextState = Unbound> {
    /// User-supplied input is invalid (bad attribute value, schema
    /// violation, configuration mismatch). Not retriable.
    InvalidInput(Box<OperationErrorDetail<S>>),
    /// The cloud API rejected the request (HTTP 4xx/5xx, server-side
    /// error). May be retriable depending on the cause.
    ApiError(Box<OperationErrorDetail<S>>),
    /// Resource was not found at the cloud API. `read` returns this
    /// instead of an empty state when the underlying resource has been
    /// deleted out-of-band. `delete` returns this when the resource is
    /// already gone.
    NotFound(Box<OperationErrorDetail<S>>),
    /// Operation timed out before completing. Retriable with backoff.
    Timeout(Box<OperationErrorDetail<S>>),
    /// Provider-internal failure (panic, unexpected state, missing
    /// schema entry, etc.). Should be escalated as a bug rather than
    /// retried.
    Internal(Box<OperationErrorDetail<S>>),
}

impl std::fmt::Display for ProviderFailure<Bound> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        DiagnosticText::new(self.host_diagnostic_text()).fmt(f)
    }
}
impl std::error::Error for ProviderFailure<Bound> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // The schema-install boundary privately mints this host-owned cause.
        // Arbitrary provider causes still require their opaque, bound projection.
        self.detail()
            .cause()
            .and_then(Diagnostic::event_schema_source)
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

impl<S: TextState> From<crate::value::EnumCanonicalizationError> for ProviderFailure<S> {
    fn from(error: crate::value::EnumCanonicalizationError) -> Self {
        Self::with_context_diagnostic(
            ProviderErrorKind::InvalidInput,
            ErrorText::new("Failed to canonicalize enum value"),
            Diagnostic::capture_host_or_provider(error),
        )
    }
}

impl<S: TextState> ProviderFailure<S> {
    pub(crate) fn diagnostic_text(&self) -> ProviderText {
        let detail = self.detail();
        let mut text = ProviderText::default();
        if let Some(id) = &detail.resource_id {
            text.append_host("[");
            text.append(&id.text.opaque());
            text.append_host("] ");
        }
        text.append(&detail.body.message().opaque());
        if detail.has_structured_cloud_fields() {
            if let Some(operation) = &detail.operation {
                text.append_host("\n  operation: ");
                text.append(&operation.opaque());
            }
            match (detail.status, &detail.code) {
                (Some(status), Some(code)) => {
                    text.append_host("\n  status: ");
                    text.append_status(status);
                    text.append_host(" ");
                    text.append(&code.opaque());
                }
                (Some(status), None) => {
                    text.append_host("\n  status: ");
                    text.append_status(status);
                }
                (None, Some(code)) => {
                    text.append_host("\n  code: ");
                    text.append(&code.opaque());
                }
                _ => {}
            }
            if let Some(request_id) = &detail.request_id {
                text.append_host("\n  request_id: ");
                text.append(&request_id.opaque());
            }
            if let Some(cause) = detail.body.rendered_cause() {
                text.append_host("\n  cause: ");
                text.append(&cause.opaque());
            }
        } else if let Some(cause) = detail.body.rendered_cause() {
            text.append_host(": ");
            text.append(&cause.opaque());
        }
        ErrorText::<S>::protect_opaque(text, &self.redaction_context())
    }
    pub(crate) fn segmented_text(&self) -> ErrorText<S> {
        let detail = self.detail();
        let mut text = ErrorText::<S>::default();
        if let Some(id) = &detail.resource_id {
            text.append_host("[");
            text.append(&id.text);
            text.append_host("] ");
        }
        text.append(detail.body.message());
        if detail.has_structured_cloud_fields() {
            if let Some(operation) = &detail.operation {
                text.append_host("\n  operation: ");
                text.append(operation);
            }
            if let Some(status) = detail.status {
                text.append_host(format!("\n  status: {status}"));
            }
            if let Some(code) = &detail.code {
                text.append_host(if detail.status.is_some() {
                    " "
                } else {
                    "\n  code: "
                });
                text.append(code);
            }
            if let Some(request_id) = &detail.request_id {
                text.append_host("\n  request_id: ");
                text.append(request_id);
            }
            if let Some(cause) = detail.body.rendered_cause() {
                text.append_host("\n  cause: ");
                text.append(cause.text());
            }
        } else if let Some(cause) = detail.body.rendered_cause() {
            text.append_host(": ");
            text.append(cause.text());
        }
        text.with_redaction_context(&self.redaction_context())
    }
    fn host_diagnostic_text(&self) -> String {
        let detail = self.detail();
        let mut text = String::new();
        if let Some(id) = &detail.resource_id {
            text.push('[');
            text.push_str(&id.text.redacted_str());
            text.push_str("] ");
        }
        text.push_str(&detail.body.message().redacted_str());
        if detail.has_structured_cloud_fields() {
            if let Some(operation) = &detail.operation {
                text.push_str("\n  operation: ");
                text.push_str(&operation.redacted_str());
            }
            match (detail.status, &detail.code) {
                (Some(status), Some(code)) => {
                    text.push_str(&format!("\n  status: {} ", status));
                    text.push_str(&code.redacted_str());
                }
                (Some(status), None) => text.push_str(&format!("\n  status: {}", status)),
                (None, Some(code)) => {
                    text.push_str("\n  code: ");
                    text.push_str(&code.redacted_str());
                }
                _ => {}
            }
            if let Some(request_id) = &detail.request_id {
                text.push_str("\n  request_id: ");
                text.push_str(&request_id.redacted_str());
            }
            if let Some(cause) = detail.body.rendered_cause() {
                text.push_str("\n  cause: ");
                text.push_str(&cause.redacted_str());
            }
        } else if let Some(cause) = detail.body.rendered_cause() {
            text.push_str(": ");
            text.push_str(&cause.redacted_str());
        }
        text
    }

    fn redaction_context(&self) -> RedactionContext {
        let detail = self.detail();
        let mut context = detail.body.message().redaction_context();
        if let Some(cause) = detail.cause() {
            context.extend(&cause.text().redaction_context());
        }
        for text in [
            &detail.provider_name,
            &detail.operation,
            &detail.code,
            &detail.request_id,
        ]
        .into_iter()
        .flatten()
        {
            context.extend(&text.redaction_context());
        }
        if let Some(id) = &detail.resource_id {
            context.extend(&id.text.redaction_context());
        }
        context
    }

    /// Preserve an already captured opaque source instead of formatting it.
    pub fn from_diagnostic(kind: ProviderErrorKind, source: impl DiagnosticSource<S>) -> Self {
        let source = source.into_diagnostic();
        let detail = OperationErrorDetail::captured(source);
        Self::from_detail(kind, detail)
    }
    fn from_detail(kind: ProviderErrorKind, detail: OperationErrorDetail<S>) -> Self {
        let detail = Box::new(detail);
        match kind {
            ProviderErrorKind::InvalidInput => Self::InvalidInput(detail),
            ProviderErrorKind::ApiError => Self::ApiError(detail),
            ProviderErrorKind::NotFound => Self::NotFound(detail),
            ProviderErrorKind::Timeout => Self::Timeout(detail),
            ProviderErrorKind::Internal => Self::Internal(detail),
        }
    }
    fn with_context_diagnostic(
        kind: ProviderErrorKind,
        context: ErrorText<S>,
        source: Diagnostic<S>,
    ) -> Self {
        Self::from_detail(kind, OperationErrorDetail::contextual(context, source))
    }

    /// Borrow the inner [`ErrorDetail`] regardless of variant.
    pub fn detail(&self) -> &OperationErrorDetail<S> {
        match self {
            ProviderFailure::InvalidInput(d)
            | ProviderFailure::ApiError(d)
            | ProviderFailure::NotFound(d)
            | ProviderFailure::Timeout(d)
            | ProviderFailure::Internal(d) => d,
        }
    }

    /// Mutably borrow the inner [`ErrorDetail`] regardless of variant.
    pub fn detail_mut(&mut self) -> &mut OperationErrorDetail<S> {
        match self {
            ProviderFailure::InvalidInput(d)
            | ProviderFailure::ApiError(d)
            | ProviderFailure::NotFound(d)
            | ProviderFailure::Timeout(d)
            | ProviderFailure::Internal(d) => d,
        }
    }

    /// Variant name as a static string (used by serialization layers).
    pub fn variant_name(&self) -> &'static str {
        match self {
            ProviderFailure::InvalidInput(_) => "invalid_input",
            ProviderFailure::ApiError(_) => "api_error",
            ProviderFailure::NotFound(_) => "not_found",
            ProviderFailure::Timeout(_) => "timeout",
            ProviderFailure::Internal(_) => "internal",
        }
    }

    /// Typed error classification for retry and diagnostic policy.
    pub const fn kind(&self) -> ProviderErrorKind {
        match self {
            ProviderFailure::InvalidInput(_) => ProviderErrorKind::InvalidInput,
            ProviderFailure::ApiError(_) => ProviderErrorKind::ApiError,
            ProviderFailure::NotFound(_) => ProviderErrorKind::NotFound,
            ProviderFailure::Timeout(_) => ProviderErrorKind::Timeout,
            ProviderFailure::Internal(_) => ProviderErrorKind::Internal,
        }
    }

    pub fn with_retryability(mut self, retryability: Retryability) -> Self {
        self.detail_mut().retryability = retryability;
        self
    }
    pub fn retryability(&self) -> Retryability {
        self.detail().retryability
    }
    pub fn is_transient(&self) -> bool {
        self.retryability() == Retryability::Transient
    }

    /// Whether the original message, code or cause identifies throttling.
    /// Diagnostic escaping, truncation and redaction do not affect retry policy.
    pub fn is_throttled(&self) -> bool {
        self.detail().throttling.is_throttled()
    }

    /// Delete dependency policy is captured before diagnostic redaction.
    pub fn is_delete_dependency(&self) -> bool {
        self.detail().throttling.delete_dependency || self.detail().throttling.dependency_code
    }

    /// User-supplied input is invalid.
    pub fn invalid_input(message: impl Into<S::Message>) -> Self {
        ProviderFailure::InvalidInput(Box::new(OperationErrorDetail::new(message)))
    }

    /// The cloud API rejected the request.
    pub fn api_error(message: impl Into<S::Message>) -> Self {
        ProviderFailure::ApiError(Box::new(OperationErrorDetail::new(message)))
    }

    /// Resource was not found at the cloud API.
    pub fn not_found(message: impl Into<S::Message>) -> Self {
        ProviderFailure::NotFound(Box::new(OperationErrorDetail::new(message)))
    }

    /// Operation timed out.
    pub fn timeout(message: impl Into<S::Message>) -> Self {
        ProviderFailure::Timeout(Box::new(OperationErrorDetail::new(message)))
    }

    /// Provider-internal failure / unexpected state.
    pub fn internal(message: impl Into<S::Message>) -> Self {
        ProviderFailure::Internal(Box::new(OperationErrorDetail::new(message)))
    }

    /// User-supplied input is invalid because of a typed source error.
    ///
    /// Runtime contexts must be explicitly opaque provider text.
    pub fn invalid_input_caused_by(
        context: impl Into<S::Context>,
        cause: impl DiagnosticSource<S>,
    ) -> Self {
        ProviderFailure::InvalidInput(Box::new(OperationErrorDetail::caused_by(context, cause)))
    }

    /// The cloud API rejected the request because of a typed source error.
    pub fn api_error_caused_by(
        context: impl Into<S::Context>,
        cause: impl DiagnosticSource<S>,
    ) -> Self {
        ProviderFailure::ApiError(Box::new(OperationErrorDetail::caused_by(context, cause)))
    }

    /// A resource was not found because of a typed source error.
    pub fn not_found_caused_by(
        context: impl Into<S::Context>,
        cause: impl DiagnosticSource<S>,
    ) -> Self {
        ProviderFailure::NotFound(Box::new(OperationErrorDetail::caused_by(context, cause)))
    }

    /// An operation timed out because of a typed source error.
    pub fn timeout_caused_by(
        context: impl Into<S::Context>,
        cause: impl DiagnosticSource<S>,
    ) -> Self {
        ProviderFailure::Timeout(Box::new(OperationErrorDetail::caused_by(context, cause)))
    }

    /// A provider-internal operation failed because of a typed source error.
    ///
    /// An arbitrary error cannot become bound host text through this context.
    ///
    /// ```compile_fail,E0277
    /// use carina_core::provider::HostError;
    /// let error = HostError::internal_caused_by("ctx", std::io::Error::other("guest password"));
    /// ```
    pub fn internal_caused_by(
        context: impl Into<S::Context>,
        cause: impl DiagnosticSource<S>,
    ) -> Self {
        ProviderFailure::Internal(Box::new(OperationErrorDetail::caused_by(context, cause)))
    }

    /// Encode this error for the provider protocol/WIT boundary.
    ///
    /// The complete typed cause chain is flattened into a string because the
    /// wire format cannot carry `dyn Error`. The returned type is the exact
    /// protocol record consumed by guest SDK encoders.
    fn to_wire_inner(&self) -> WireProviderError {
        let union = self.redaction_context();
        let mut projection = self.clone();
        projection.protect_fields(&union);
        let detail = projection.detail();
        let kind = match self.kind() {
            ProviderErrorKind::InvalidInput => WireProviderErrorKind::InvalidInput,
            ProviderErrorKind::ApiError => WireProviderErrorKind::ApiError,
            ProviderErrorKind::NotFound => WireProviderErrorKind::NotFound,
            ProviderErrorKind::Timeout => WireProviderErrorKind::Timeout,
            ProviderErrorKind::Internal => WireProviderErrorKind::Internal,
        };
        let mut wire = WireProviderError {
            kind,
            message: String::new(),
            resource_id: detail
                .resource_id
                .as_ref()
                .filter(|_| S::BOUND || cfg!(target_arch = "wasm32"))
                .map(|id| {
                    let mut wire =
                        crate::protocol_convert::resource_id_to_protocol(id.as_resource_id());
                    if let Some(secrets) = id.text.known_secrets() {
                        let redact = |text: String| {
                            secrets.redact(&ProviderText::new(text)).as_str().to_owned()
                        };
                        wire.provider = redact(wire.provider);
                        wire.resource_type = redact(wire.resource_type);
                        wire.identity = redact(wire.identity);
                    }
                    wire
                }),
            cause: None,
            provider_name: None,
            operation: None,
            status: detail.status,
            code: None,
            request_id: None,
        };
        crate::provider_text::ErrorWireText {
            message: detail.body.message(),
            cause: detail.body.rendered_cause(),
            provider_name: detail.provider_name.as_ref(),
            operation: detail.operation.as_ref(),
            code: detail.code.as_ref(),
            request_id: detail.request_id.as_ref(),
        }
        .write(&mut wire);
        wire
    }

    /// Attach a resource id to the inner detail.
    pub fn for_resource(mut self, id: ResourceId) -> Self {
        let id: ErrorResourceId<S> = id.into();
        self.detail_mut().resource_id = Some(Box::new(id));

        self
    }

    /// Attach a provider name (e.g. `"aws"`, `"awscc"`) for boundary
    /// errors that don't carry a specific resource id (e.g.
    /// `create_provider` / provider init failures). Used by the CLI
    /// renderer to label structured account-guard output.
    pub fn for_provider(mut self, provider_name: impl Into<String>) -> Self {
        self.detail_mut().provider_name = Some(self.new_host_field(provider_name.into()));

        self
    }

    /// Attach the service-qualified cloud-API operation that failed
    /// (e.g. `"iam.ListRoles"`, `"s3.HeadBucket"`). Populating this —
    /// or any of [`Self::with_status`] / [`Self::with_code`] /
    /// [`Self::with_request_id`] — flips `Display` into the multi-line
    /// labeled render shape introduced in carina#3242.
    pub fn with_operation(mut self, operation: impl Into<String>) -> Self {
        self.detail_mut().operation = Some(self.new_host_field(operation.into()));

        self
    }

    /// Attach the HTTP status code from the cloud-API response.
    pub fn with_status(mut self, status: u16) -> Self {
        self.detail_mut().status = Some(status);

        self
    }

    /// Attach the application-level error code from the cloud API
    /// (e.g. `"AccessDenied"`, `"NoSuchBucket"`).
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        let code = code.into();
        let text = self.new_host_field(code.clone());
        let detail = self.detail_mut();
        detail.throttling.code = has_throttling_marker(&code);
        detail.throttling.dependency_code = has_delete_dependency_marker(&code);
        detail.code = Some(text);

        self
    }

    /// Attach the correlation id from the cloud-API response so the
    /// operator can paste it into support tickets.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.detail_mut().request_id = Some(self.new_host_field(request_id.into()));

        self
    }

    /// Attach a host-only lifecycle classification.
    pub fn with_host_cause(mut self, cause: HostErrorCause) -> Self {
        self.detail_mut().host_cause = Some(cause);

        self
    }

    /// Replace the message, retaining independently contextualized causes.
    /// A captured diagnostic is the message itself, so it is suppressed in full.
    /// Preserve the error variant, retry policy, metadata, and redaction inventories.
    pub(crate) fn with_redacted_message(mut self, message: impl Into<String>) -> Self {
        let redactions = self.redaction_context();
        let text = self
            .new_host_field(message.into())
            .with_redaction_context(&redactions);
        self.detail_mut().body = match self.detail().body.clone() {
            FailureBody::Message(_) | FailureBody::Captured(_) => FailureBody::Message(text),
            FailureBody::Context { context: _, cause } => FailureBody::Context {
                context: text,
                cause,
            },
        };
        self.protect_fields(&redactions);
        self
    }
    fn protect_fields(&mut self, context: &RedactionContext) {
        let detail = self.detail_mut();
        detail.body = match detail.body.clone() {
            FailureBody::Message(text) => {
                FailureBody::Message(text.with_redaction_context(context))
            }
            FailureBody::Captured(source) => {
                FailureBody::Captured(source.with_redaction_context(context))
            }
            FailureBody::Context {
                context: text,
                cause,
            } => FailureBody::Context {
                context: text.with_redaction_context(context),
                cause: cause.with_redaction_context(context),
            },
        };
        for field in [
            &mut detail.provider_name,
            &mut detail.operation,
            &mut detail.code,
            &mut detail.request_id,
        ] {
            if let Some(text) = field.take() {
                *field = Some(text.with_redaction_context(context));
            }
        }
        if let Some(id) = &mut detail.resource_id {
            id.text = id.text.clone().with_redaction_context(context);
        }
    }
    fn new_host_field(&self, text: String) -> ErrorText<S> {
        ErrorText::<S>::new(text)
    }
}

impl<S: TextState> OperationErrorDetail<S> {
    pub(crate) fn new(message: impl Into<S::Message>) -> Self {
        let text = S::operations().message(message.into());
        let mut detail = Self::captured(Diagnostic::message_for_state(text.clone()));
        detail.body = FailureBody::Message(text);
        detail
    }
    fn captured(cause: Diagnostic<S>) -> Self {
        let throttling = ThrottlingClassification {
            message_or_cause: cause.has_throttling_marker(),
            delete_dependency: cause.has_delete_dependency_marker(),
            ..ThrottlingClassification::default()
        };
        Self {
            retryability: cause.retryability(),
            body: FailureBody::Captured(cause),
            throttling,
            resource_id: None,
            host_cause: None,
            provider_name: None,
            operation: None,
            status: None,
            code: None,
            request_id: None,
        }
    }
    fn caused_by(context: impl Into<S::Context>, cause: impl DiagnosticSource<S>) -> Self {
        Self::contextual(
            S::operations().context(context.into()),
            cause.into_diagnostic(),
        )
    }
    fn contextual(context: ErrorText<S>, cause: Diagnostic<S>) -> Self {
        let mut detail = Self::captured(cause.clone());
        detail.throttling.message_or_cause |= context.opaque().has_throttling_marker();
        detail.throttling.delete_dependency |= context.opaque().has_delete_dependency_marker();
        detail.body = FailureBody::Context { context, cause };
        detail
    }
    /// The complete captured diagnostic, without exposing a raw error reference.
    pub fn cause(&self) -> Option<&Diagnostic<S>> {
        self.body.cause()
    }

    /// Host-side lifecycle classification, when the host attached one.
    pub const fn host_cause(&self) -> Option<HostErrorCause> {
        self.host_cause
    }

    /// Returns `true` when at least one of the cloud-API metadata
    /// fields (`operation`, `status`, `code`, `request_id`) is set.
    ///
    /// `Display` uses this as the gate for the multi-line labeled
    /// render (carina#3242): any of the four flips the renderer into
    /// the structured shape. When none are set the error keeps the
    /// existing single-line cause render — that's the fallback path for
    /// non-cloud errors.
    ///
    /// `pub(crate)` until a second use site materializes outside
    /// `carina-core`; the name still has "cloud" baked in and is
    /// likely to refactor before stabilising as public API.
    pub(crate) fn has_structured_cloud_fields(&self) -> bool {
        self.operation.is_some()
            || self.status.is_some()
            || self.code.is_some()
            || self.request_id.is_some()
    }
}

impl OperationErrorDetail<Bound> {
    /// Opaque context for a caused error, or the complete message for a
    /// message-only error.
    pub fn context(&self) -> crate::provider_text::DiagnosticText {
        self.body.message().as_diagnostic()
    }

    /// Sanitized application-level code for diagnostics. Set it with
    /// [`ProviderError::with_code`] so raw-text retry classification stays valid.
    pub fn code(&self) -> Option<crate::provider_text::DiagnosticText> {
        self.code.as_ref().map(ErrorText::as_diagnostic)
    }
}

/// Synthetic typed source for a flattened cause in native wire fixtures.
#[cfg(any(test, all(feature = "test-utils", not(target_arch = "wasm32"))))]
#[derive(Debug)]
struct FlattenedWireCause(String);

#[cfg(any(test, all(feature = "test-utils", not(target_arch = "wasm32"))))]
impl std::fmt::Display for FlattenedWireCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(any(test, all(feature = "test-utils", not(target_arch = "wasm32"))))]
impl std::error::Error for FlattenedWireCause {}

pub type ProviderResult<T, S = Unbound> = Result<T, ProviderFailure<S>>;
pub type HostResult<T> = ProviderResult<T, Bound>;

/// Top-level desired attributes proven to have been sent by one provider
/// operation.
///
/// Construction is restricted to checked create requests, executed update
/// patches, and validated create typestate. Consumers can test membership but
/// cannot relabel an arbitrary string collection as send evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentAttributes {
    attributes: BTreeSet<String>,
    #[cfg(any(test, feature = "test-utils"))]
    all_for_test: bool,
}

impl SentAttributes {
    pub(crate) fn from_resource<S, D>(
        resource: &Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> Self {
        Self::only(resource.attributes.keys().cloned().collect())
    }

    pub(crate) fn only(attributes: BTreeSet<String>) -> Self {
        Self {
            attributes,
            #[cfg(any(test, feature = "test-utils"))]
            all_for_test: false,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn all_for_test() -> Self {
        Self {
            attributes: BTreeSet::new(),
            all_for_test: true,
        }
    }

    fn from_update_patch(patch: &UpdatePatch) -> Self {
        Self::only(
            patch
                .ops
                .iter()
                .filter_map(|op| op.sent_value().map(|_| op.key().to_string()))
                .collect(),
        )
    }

    fn from_create_sent<S, D>(
        sent: &Sent,
        resource: &Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> Self {
        match sent {
            Sent::AllRequestAttributes => Self::from_resource(resource),
            Sent::Only(attributes) => Self::only(attributes.clone()),
        }
    }

    /// Whether this operation sent `attribute` with a desired value.
    pub fn contains(&self, attribute: &str) -> bool {
        #[cfg(any(test, feature = "test-utils"))]
        if self.all_for_test {
            return true;
        }
        self.attributes.contains(attribute)
    }
}

/// Desired values paired with the send evidence from the same provider
/// operation.
///
/// Keeping these together prevents state writeback from combining checked
/// [`SentAttributes`] with an unrelated authored resource whose values may
/// still contain unresolved expressions.
#[derive(Debug, Clone)]
pub struct SentDesiredAttributes<'a> {
    attributes: Vec<(&'a String, SnapshotValue)>,
    sent: SentAttributes,
}

impl<'a> SentDesiredAttributes<'a> {
    pub(crate) fn new<P: StateValueLeaf + 'a>(
        attributes: impl IntoIterator<Item = (&'a String, &'a Value<P>)>,
        sent: SentAttributes,
    ) -> Self {
        Self {
            attributes: attributes
                .into_iter()
                .map(|(key, value)| (key, crate::resource::state_value_snapshot(value)))
                .collect(),
            sent,
        }
    }

    /// Provider-ready desired values retained by the accepted operation.
    pub fn values(&self) -> impl Iterator<Item = (&String, &SnapshotValue)> {
        self.attributes.iter().map(|(key, value)| (*key, value))
    }

    /// Whether this operation sent `attribute` with a desired value.
    pub fn contains(&self, attribute: &str) -> bool {
        self.sent.contains(attribute)
    }
}

/// Per-operation request record for [`Provider::create`].
///
/// Mirrors `create-request` in `wit/types.wit`.
/// A merely resolved resource is not sufficient to build this request:
///
/// ```compile_fail
/// use carina_core::provider::{CreateRequest, ProviderReadyResource};
///
/// fn forge(resource: ProviderReadyResource) -> CreateRequest {
///     CreateRequest { resource }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct CreateRequest {
    /// Full desired state for the new resource.
    resource: ProviderReadyResource,
}

impl CreateRequest {
    pub(crate) fn checked(resource: ProviderReadyResource) -> Self {
        Self { resource }
    }

    /// Full desired state after provider-boundary checks.
    pub fn resource(&self) -> &ProviderReadyResource {
        &self.resource
    }

    /// Effective directives resolved while the resource crossed the provider
    /// preparation boundary.
    pub fn directives(&self) -> &ProviderDirectives {
        self.resource.directives()
    }

    pub(crate) fn sent_attributes(&self) -> SentAttributes {
        SentAttributes::from_resource(self.resource.as_resource())
    }
}

/// Per-operation request record for [`Provider::read`].
///
/// Mirrors `read-request` in `wit/types.wit`. Has no operationally
/// meaningful fields today; the record exists so future fields (e.g.
/// a freshness hint or an attribute projection) can be added without
/// breaking the `read` signature.
#[derive(Debug, Clone, Default)]
pub struct ReadRequest;

/// Per-operation request record for [`Provider::update`].
///
/// Mirrors `update-request` in `wit/types.wit`. `from` is the current
/// provider-side state; `patch` carries the user's intended changes plus any
/// desired, updatable write-only values that must be reasserted alongside an
/// update. The patch is the sole source of truth for what the provider should
/// write. No desired resource or validation view is available outside the
/// checked patch (see #2559).
///
/// External callers cannot assemble an update request from raw state and
/// patch data; the private witness is supplied only by the checked provider
/// preparation path:
///
/// ```compile_fail
/// use carina_core::provider::{UpdatePatch, UpdateRequest};
/// use carina_core::resource::State;
///
/// fn forge(from: State, patch: UpdatePatch) -> UpdateRequest {
///     UpdateRequest { from, patch }
/// }
/// ```
///
/// A checked patch cannot be replaced after construction either:
///
/// ```compile_fail
/// use carina_core::provider::{UpdatePatch, UpdateRequest};
///
/// fn replace_patch(request: &mut UpdateRequest, replacement: UpdatePatch) {
///     request.patch = replacement;
/// }
/// ```
///
/// An update request cannot expose a desired resource or validation view:
///
/// ```compile_fail,E0599
/// use carina_core::provider::UpdateRequest;
/// fn bypass(request: &UpdateRequest) { let _ = request.resource(); }
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::provider::UpdateRequest;
/// fn bypass(request: &UpdateRequest) { let _ = request.validation_inputs(); }
/// ```
#[derive(Debug, Clone)]
pub struct UpdateRequest {
    /// Current provider-side state for read-modify-write paths and resolving
    /// server-assigned identifiers.
    from: SnapshotState,
    /// Structured description of the user's intended change and any write-only
    /// values reasserted alongside it.
    patch: WireReady<UpdatePatch>,
    /// Effective directives resolved from the managed resource schema.
    directives: ProviderDirectives,
    diagnostic_values: Vec<Value>,
}

impl UpdateRequest {
    pub(crate) fn checked(
        from: SnapshotState,
        changed_attributes: &[String],
        resource: &ProviderReadyResource,
        schema: &ResourceSchema,
    ) -> Result<Self, crate::executor::ProviderPreparationError> {
        let patch = build_update_patch(changed_attributes, resource, &from, schema)?;
        let patch = WireReady::try_new(patch, ProviderWireTarget::Resource(resource.id.clone()))?;
        let diagnostic_values = patch
            .as_inner()
            .ops
            .iter()
            .filter_map(|op| match op {
                PatchOp::Add { key, value }
                | PatchOp::Replace { key, value }
                | PatchOp::Reassert { key, value } => Some(resource.diagnostic_value(key, value)),
                PatchOp::Remove { .. } => None,
            })
            .collect();
        Ok(Self {
            from,
            patch,
            directives: *resource.directives(),
            diagnostic_values,
        })
    }

    /// Current provider-side state.
    pub fn from(&self) -> &SnapshotState {
        &self.from
    }

    /// Patch derived from the checked desired resource.
    pub fn patch(&self) -> &UpdatePatch {
        self.patch.as_inner()
    }

    pub(crate) fn diagnostic_values(&self) -> impl Iterator<Item = &Value> {
        self.diagnostic_values.iter()
    }

    /// The checked projection consumed by provider transport adapters.
    pub fn wire_patch(&self) -> &[WirePatchOp] {
        self.patch.wire()
    }

    /// Effective directives resolved while the desired resource crossed the
    /// provider preparation boundary.
    pub fn directives(&self) -> &ProviderDirectives {
        &self.directives
    }
}

/// Per-operation request record for [`Provider::delete`].
///
/// Mirrors `delete-request` in `wit/types.wit`.
///
/// ```compile_fail,E0599
/// use carina_core::provider::DeleteRequest;
///
/// let _ = DeleteRequest::default();
/// ```
#[derive(Debug, Clone)]
pub struct DeleteRequest {
    /// Effective provider-visible directives for the resource.
    directives: ProviderDirectives,
}

impl DeleteRequest {
    /// Resolve authored directives against the resource schema before crossing
    /// the provider boundary.
    pub fn resolve(directives: &Directives, schema: ProviderDirectiveSchema<'_>) -> Self {
        Self {
            directives: ProviderDirectives::resolve(directives, schema),
        }
    }

    /// Construct a delete request from directives already resolved through a
    /// schema registry or an explicitly named schema-unavailable path.
    pub fn from_resolved(directives: ProviderDirectives) -> Self {
        Self { directives }
    }

    /// Effective directives carried by this request.
    pub fn directives(&self) -> &ProviderDirectives {
        &self.directives
    }
}

/// Effective directive values transported to providers.
///
/// Callers cannot forge an effective value without resolving authored
/// directives against an explicitly available or unavailable schema:
///
/// ```compile_fail,E0451
/// use carina_core::provider::ProviderDirectives;
///
/// let _ = ProviderDirectives {
///     force_delete: false,
///     create_before_destroy: false,
///     prevent_destroy: false,
///     prevent_destroy_source: None,
/// };
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::provider::ProviderDirectives;
///
/// let _ = ProviderDirectives::default();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderDirectives {
    force_delete: bool,
    create_before_destroy: bool,
    prevent_destroy: bool,
    prevent_destroy_source: Option<PreventDestroySource>,
}

/// Read-only projection used at the provider protocol boundary.
///
/// The projection is intentionally constructed only by
/// [`ProviderDirectives::protocol_parts`]. Keeping the exhaustive projection
/// beside the private fields means adding an effective directive cannot be
/// silently omitted from the wire conversion.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProviderDirectiveProtocolParts {
    pub(crate) force_delete: bool,
    pub(crate) create_before_destroy: bool,
    pub(crate) prevent_destroy: bool,
    pub(crate) prevent_destroy_source: Option<PreventDestroySource>,
}

impl ProviderDirectives {
    /// Resolve the provider-visible directive subset against an explicitly
    /// available or unavailable resource schema.
    pub fn resolve(directives: &Directives, schema: ProviderDirectiveSchema<'_>) -> Self {
        let prevent_destroy_resolution = directives.prevent_destroy_setting().resolve(schema);
        let prevent_destroy_source = prevent_destroy_resolution.source();
        let prevent_destroy = matches!(
            prevent_destroy_resolution,
            PreventDestroyResolution::Protected(_)
        );
        Self {
            force_delete: directives.force_delete,
            create_before_destroy: directives.create_before_destroy,
            prevent_destroy,
            prevent_destroy_source,
        }
    }

    /// Decode values that have already crossed the effective-directives wire
    /// boundary. This is crate-private so authored callers cannot forge policy.
    #[cfg(test)]
    pub(crate) fn from_effective_wire(
        force_delete: bool,
        create_before_destroy: bool,
        prevent_destroy: bool,
    ) -> Self {
        Self {
            force_delete,
            create_before_destroy,
            prevent_destroy,
            prevent_destroy_source: prevent_destroy
                .then_some(PreventDestroySource::ExplicitDirective),
        }
    }

    pub fn force_delete(&self) -> bool {
        self.force_delete
    }

    pub fn create_before_destroy(&self) -> bool {
        self.create_before_destroy
    }

    pub fn prevent_destroy(&self) -> bool {
        self.prevent_destroy
    }

    /// Provenance for effective destruction protection, when enabled.
    pub fn prevent_destroy_source(&self) -> Option<PreventDestroySource> {
        self.prevent_destroy_source
    }

    pub(crate) fn protocol_parts(&self) -> ProviderDirectiveProtocolParts {
        let Self {
            force_delete,
            create_before_destroy,
            prevent_destroy,
            prevent_destroy_source,
        } = *self;
        ProviderDirectiveProtocolParts {
            force_delete,
            create_before_destroy,
            prevent_destroy,
            prevent_destroy_source,
        }
    }
}

/// A structured description of the user's intended change to a resource.
///
/// Mirrors `update-patch` in `wit/types.wit`. Each [`PatchOp`]
/// corresponds to a key the user explicitly specified or removed in the
/// desired state, or to an unchanged desired write-only key re-sent through
/// [`PatchOp::Reassert`] alongside another change. Fields the user has never
/// specified do not appear in the patch.
///
/// Providers MUST NOT modify any attribute that is not represented in
/// `ops`. An [`UpdateRequest`] cannot be constructed without the private
/// provider-readiness witness and its managed-resource schema.
#[derive(Debug, Clone, Default)]
pub struct UpdatePatch {
    /// Operations derived from the desired resource.
    pub ops: Vec<PatchOp>,
}

/// A single operation inside an [`UpdatePatch`].
///
/// Mirrors `patch-op` in `wit/types.wit`.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchOp {
    /// User added a key that did not previously appear in the desired state.
    Add { key: String, value: Value },
    /// User changed the value of an existing key in the desired state.
    Replace { key: String, value: Value },
    /// Unchanged; re-sent because it is write-only and the backend cannot read
    /// it back.
    Reassert { key: String, value: Value },
    /// User removed a key that previously appeared in the desired state.
    Remove { key: String },
}

/// Sending a top-level value would overwrite an ignored write-only leaf whose
/// current provider value cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "resource '{resource}' cannot send attribute '{attribute}' because ignored write-only path '{path}' must be preserved, but its current value cannot be known",
    resource = DiagnosticText::new(resource.to_string()), attribute = DiagnosticText::new(attribute), path = DiagnosticText::new(path.to_string())
)]
pub struct IgnoredWriteOnlyValueError {
    resource: Box<ResourceId>,
    attribute: String,
    path: Box<crate::resource::ParsedIgnorePath>,
}

impl IgnoredWriteOnlyValueError {
    pub fn resource_id(&self) -> &ResourceId {
        &self.resource
    }

    pub fn attribute(&self) -> &str {
        &self.attribute
    }

    pub fn path(&self) -> &crate::resource::ParsedIgnorePath {
        &self.path
    }
}

/// A patch cannot contain historical hash evidence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdatePatchError {
    #[error(transparent)]
    IgnoredWriteOnly(#[from] IgnoredWriteOnlyValueError),
    #[error(transparent)]
    UnavailableSecret(#[from] UnavailableSecretError),
}

impl PatchOp {
    /// Top-level attribute name. Nested-field patches are a future extension.
    pub fn key(&self) -> &str {
        match self {
            Self::Add { key, .. }
            | Self::Replace { key, .. }
            | Self::Reassert { key, .. }
            | Self::Remove { key } => key,
        }
    }

    /// Return the value sent by `Add`, `Replace`, or `Reassert`.
    pub fn sent_value(&self) -> Option<&Value> {
        match self {
            Self::Add { value, .. }
            | Self::Replace { value, .. }
            | Self::Reassert { value, .. } => Some(value),
            Self::Remove { .. } => None,
        }
    }
}

impl UpdatePatch {
    /// Desired values actually sent by this patch, including reasserted inputs.
    pub(crate) fn sent_projection(&self) -> IndexMap<String, Value> {
        self.ops
            .iter()
            .filter_map(|op| {
                op.sent_value()
                    .map(|value| (op.key().to_owned(), value.clone()))
            })
            .collect()
    }

    pub(crate) fn sent_attributes(&self) -> SentAttributes {
        SentAttributes::from_update_patch(self)
    }
}

/// Build an [`UpdatePatch`] from a list of changed attribute names plus
/// the desired (`to`) and current (`from`) views.
///
/// For each `key` in `changed_attributes`:
/// - present in `to` but missing in `from` → [`PatchOp::Add`]
/// - present in both                       → [`PatchOp::Replace`]
/// - missing in `to` but present in `from` → [`PatchOp::Remove`]
/// - missing from both                     → no operation
///
/// When at least one changed-key operation exists, desired attributes that the
/// schema marks write-only or write-only-bearing, but not create-only, are
/// appended as sorted
/// [`PatchOp::Reassert`] operations unless that key is already in the patch.
///
/// ```compile_fail
/// use carina_core::provider::build_update_patch;
/// use carina_core::resource::{ResolvedResource, Resource, State};
/// use carina_core::schema::ResourceSchema;
///
/// let resource = ResolvedResource::new(Resource::new("test", "example"));
/// let state: State = unimplemented!();
/// let schema = ResourceSchema::new("test");
/// let _patch = build_update_patch(&[], &resource, &state, &schema);
/// ```
pub fn build_update_patch<P: StateValueLeaf>(
    changed_attributes: &[String],
    to: &ProviderReadyResource,
    from: &State<P>,
    schema: &ResourceSchema,
) -> Result<UpdatePatch, UpdatePatchError> {
    plan_update_patch(changed_attributes, to.as_inner(), from, schema)
}

/// Decide every update operation from one resolved desired/current pair.
/// Planning discards the returned patch after validating it; apply retains it
/// as the provider request payload.
pub(crate) fn plan_update_patch<S, P: StateValueLeaf>(
    changed_attributes: &[String],
    to: &crate::resource::ResolvedResource<S>,
    from: &State<P>,
    schema: &ResourceSchema,
) -> Result<UpdatePatch, UpdatePatchError> {
    let ignore_changes = to.ignore_changes();
    let to = to.as_resource();
    let mut patched_keys = HashSet::new();
    let mut ops = Vec::new();
    for key in changed_attributes {
        if ignore_changes.ignores_whole_attribute(key) {
            continue;
        }
        let value = to.attributes.get(key);
        let in_from = from.attributes.contains_key(key);
        if value.is_none() && !in_from {
            continue;
        }
        if !patched_keys.insert(key.clone()) {
            continue;
        }
        if value.is_some()
            && let Some(path) = ignore_changes
                .for_attribute(key)
                .find(|path| path.is_write_only())
        {
            return Err(ignored_write_only_error(&to.id, key, path).into());
        }
        let observed = from
            .attributes
            .get(key)
            .map(crate::resource::state_value_snapshot);
        let sent_value = value
            .map(|value| {
                ignore_changes
                    .with_observed_readable_leaves(key, value, observed.as_ref())
                    .to_value_at(format!("{}.{}", to.id, key))
            })
            .transpose()?;
        match (sent_value, in_from) {
            (Some(value), false) => ops.push(PatchOp::Add {
                key: key.clone(),
                value,
            }),
            (Some(value), true) => ops.push(PatchOp::Replace {
                key: key.clone(),
                value,
            }),
            (None, true) => ops.push(PatchOp::Remove { key: key.clone() }),
            (None, false) => {}
        }
    }

    if ops.is_empty() {
        return Ok(UpdatePatch { ops });
    }

    let mut reasserts = Vec::new();
    for (key, value) in &to.attributes {
        let Some(attribute) = schema.attributes.get(key) else {
            continue;
        };
        if (!attribute.write_only && !schema.write_only_bearing(key))
            || attribute.create_only
            || patched_keys.contains(key)
        {
            continue;
        }
        let ignored_write_only = ignore_changes
            .for_attribute(key)
            .filter(|path| path.is_write_only())
            .collect::<Vec<_>>();
        if attribute.write_only {
            if ignore_changes.ignores_whole_attribute(key) {
                continue;
            }
            if let Some(path) = ignored_write_only.first() {
                return Err(ignored_write_only_error(&to.id, key, path).into());
            }
        } else if !ignored_write_only.is_empty() {
            let desired_write_only = schema.write_only_leaf_projection(key, value);
            if desired_write_only
                .iter()
                .all(|(leaf, _, _)| ignore_changes.ignores_leaf(key, leaf))
            {
                continue;
            }
            return Err(ignored_write_only_error(&to.id, key, ignored_write_only[0]).into());
        }
        reasserts.push(PatchOp::Reassert {
            key: key.clone(),
            value: ignore_changes
                .with_observed_readable_leaves(
                    key,
                    value,
                    from.attributes
                        .get(key)
                        .map(crate::resource::state_value_snapshot)
                        .as_ref(),
                )
                .to_value_at(format!("{}.{}", to.id, key))?,
        });
    }
    reasserts.sort_by(|left, right| left.key().cmp(right.key()));
    ops.extend(reasserts);
    Ok(UpdatePatch { ops })
}

fn ignored_write_only_error(
    resource: &ResourceId,
    attribute: &str,
    path: &crate::resource::ResolvedIgnorePath,
) -> IgnoredWriteOnlyValueError {
    IgnoredWriteOnlyValueError {
        resource: Box::new(resource.clone()),
        attribute: attribute.to_string(),
        path: Box::new(path.authored().clone()),
    }
}

/// Return type for async operations
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Saved attribute values keyed by resource ID.
///
/// The private field marks these values as originating in persisted state.
/// Provider transports may preserve typed secret hashes from this map using
/// their legacy state wire representation; desired resource maps have no
/// conversion into this type and remain subject to the stricter encoder.
///
/// Persisted provenance cannot be asserted by wrapping an arbitrary map:
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::provider::SavedAttrs;
///
/// let _saved: SavedAttrs = HashMap::new().into();
/// ```
///
/// Nor can callers add an arbitrary desired map after construction:
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::provider::SavedAttrs;
///
/// let mut saved = SavedAttrs::new();
/// saved.insert(todo!("resource id"), HashMap::new());
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SavedAttrs(HashMap<ResourceId, HashMap<String, crate::resource::SnapshotValue>>);

impl SavedAttrs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &ResourceId) -> Option<&HashMap<String, crate::resource::SnapshotValue>> {
        self.0.get(id)
    }

    pub fn iter(
        &self,
    ) -> impl Iterator<
        Item = (
            &ResourceId,
            &HashMap<String, crate::resource::SnapshotValue>,
        ),
    > {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a SavedAttrs {
    type Item = (
        &'a ResourceId,
        &'a HashMap<String, crate::resource::SnapshotValue>,
    );
    type IntoIter = std::collections::hash_map::Iter<
        'a,
        ResourceId,
        HashMap<String, crate::resource::SnapshotValue>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Attribute storage in the state-file JSON format.
///
/// State-file construction goes through [`Self::from_encoded`]; this type
/// deliberately implements serialization but not deserialization. The
/// invariant is that values enter the DSL value layer only through [`Self::get`]
/// or [`Self::to_values`], which decode structural `$secret` tags as typed
/// `RedactedSecret` values. Persisted strings always remain strings.
///
/// Raw JSON cannot be inserted after construction. State loaders call
/// [`Self::from_encoded`] after versioned migrations rewrite legacy encodings;
/// callers that already hold validated values use [`Self::from_persisted_values`].
///
/// ```compile_fail
/// use carina_core::provider::PersistedAttributes;
///
/// let mut attributes = PersistedAttributes::default();
/// attributes.insert_encoded("token".to_string(), serde_json::json!("plaintext"));
/// attributes.replace_encoded(Default::default());
/// ```
///
/// ```compile_fail
/// use carina_core::provider::PersistedAttributes;
///
/// let ordinary = serde_json::json!({"event_pattern": {"$or": []}});
/// let _: PersistedAttributes = serde_json::from_value(ordinary).unwrap();
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedAttributes {
    encoded: HashMap<String, PersistedValue>,
}

#[derive(Debug, thiserror::Error)]
pub enum PersistedAttributesError {
    #[error(transparent)]
    InvalidPersistedValue(PersistedAttributeDecodeError),
    #[error(transparent)]
    InvalidWireValue(PersistedWireValueError),
}

/// Attribute context for a persisted-value decode failure.
///
/// This error deliberately keeps the codec failure as its source without
/// repeating it in `Display`; the state-file serde boundary renders the full
/// chain once.
#[derive(Debug)]
pub struct PersistedAttributeDecodeError {
    attribute_key: String,
    error: PersistedValueDecodeError,
}

impl std::fmt::Display for PersistedAttributeDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            DiagnosticText::new(format!(
                "persisted attribute {:?} is not valid persisted JSON",
                self.attribute_key
            ))
        )
    }
}

impl std::error::Error for PersistedAttributeDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Attribute context for a persisted-state wire encoding failure.
#[derive(Debug)]
pub struct PersistedWireValueError {
    attribute_key: String,
    error: SerializationError,
}

impl std::fmt::Display for PersistedWireValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            DiagnosticText::new(format!(
                "persisted wire attribute {:?} cannot be encoded",
                self.attribute_key
            ))
        )
    }
}

impl std::error::Error for PersistedWireValueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl PersistedAttributesError {
    pub fn attribute_key(&self) -> &str {
        match self {
            Self::InvalidPersistedValue(error) => &error.attribute_key,
            Self::InvalidWireValue(error) => &error.attribute_key,
        }
    }
}

impl Serialize for PersistedAttributes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.encoded.serialize(serializer)
    }
}

/// Which existing state hashes may classify provider plaintext as secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviousSecretHashAuthority<'a> {
    None,
    /// Protect every key that already contains a stored secret hash.
    AllPreviouslyHashedKeys(&'a PersistedAttributes),
    /// Protect stored hashes only for keys absent from the desired resource.
    OnlyKeysAbsentFromDesired(&'a PersistedAttributes),
}

/// Select the saved-plan hash for an attribute, falling back to the current
/// state's hash when the plan did not capture one.
pub fn previous_hash<'a>(
    preferred: Option<&'a PersistedAttributes>,
    current: Option<&'a PersistedAttributes>,
    key: &str,
) -> Option<&'a PersistedValue> {
    preferred
        .and_then(|attributes| attributes.encoded_value(key))
        .or_else(|| current.and_then(|attributes| attributes.encoded_value(key)))
}

impl PersistedAttributes {
    /// Encode the typed placeholders captured in a saved-plan resource into
    /// their structural persisted representation.
    pub fn from_saved_plan_attributes<'a>(
        attributes: impl IntoIterator<Item = (&'a String, &'a crate::resource::SnapshotValue)>,
    ) -> Self {
        let encoded = attributes
            .into_iter()
            .filter_map(|(key, value)| {
                PersistedValue::saved_plan_secret_inventory(value).map(|value| (key.clone(), value))
            })
            .collect();
        Self { encoded }
    }

    /// Reconstruct saved attributes from an already-typed persisted-state
    /// transport. Redacted secrets arrive as `RedactedSecret`; strings are
    /// always encoded as strings without content inspection.
    pub fn from_typed_persisted_wire_values(
        values: HashMap<String, crate::resource::SnapshotValue>,
    ) -> Result<Self, PersistedAttributesError> {
        let mut values: Vec<_> = values.into_iter().collect();
        values.sort_by(|(left, _), (right, _)| left.cmp(right));

        let mut encoded = HashMap::with_capacity(values.len());
        for (attribute_key, value) in values {
            let value = PersistedValue::encode_provider_value(&value, None).map_err(|error| {
                PersistedAttributesError::InvalidWireValue(PersistedWireValueError {
                    attribute_key: attribute_key.clone(),
                    error,
                })
            })?;
            encoded.insert(attribute_key, value);
        }
        Ok(Self { encoded })
    }

    /// Construct a persisted attribute map from complete values produced by
    /// the core persisted encoder.
    pub fn from_persisted_values(
        encoded: HashMap<String, PersistedValue>,
    ) -> Result<Self, PersistedAttributesError> {
        Ok(Self { encoded })
    }

    /// Validate JSON entering through the state-row load seam after versioned
    /// migrations have rewritten legacy encodings.
    pub fn from_encoded(
        encoded: HashMap<String, serde_json::Value>,
    ) -> Result<Self, PersistedAttributesError> {
        let encoded = encoded
            .into_iter()
            .map(|(attribute_key, value)| {
                PersistedValue::from_serialized_json(value)
                    .map(|value| (attribute_key.clone(), value))
                    .map_err(|error| {
                        PersistedAttributesError::InvalidPersistedValue(
                            PersistedAttributeDecodeError {
                                attribute_key,
                                error,
                            },
                        )
                    })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { encoded })
    }

    /// Decode one stored attribute with structural persisted-secret semantics.
    pub fn get(&self, key: &str) -> Option<crate::resource::SnapshotValue> {
        self.encoded.get(key)?.decode()
    }

    /// Decode every non-null stored attribute with structural persisted-secret semantics.
    pub fn to_values(&self) -> HashMap<String, crate::resource::SnapshotValue> {
        self.encoded
            .iter()
            .filter_map(|(key, value)| value.decode().map(|value| (key.clone(), value)))
            .collect()
    }

    /// Decode every stored value into ordinary JSON for display surfaces.
    pub fn to_decoded_json(&self) -> HashMap<String, serde_json::Value> {
        self.encoded
            .iter()
            .map(|(key, value)| (key.clone(), value.decoded_json()))
            .collect()
    }

    /// Decode one stored value into ordinary JSON for display surfaces.
    pub fn decoded_json_value(&self, key: &str) -> Option<serde_json::Value> {
        Some(self.encoded.get(key)?.decoded_json())
    }

    /// Iterate stored attribute names without exposing their encoded values.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.encoded.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.encoded.is_empty()
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.encoded.contains_key(key)
    }

    /// Borrow the persisted JSON representation for one attribute.
    pub fn encoded_value(&self, key: &str) -> Option<&PersistedValue> {
        self.encoded.get(key)
    }

    /// Retain top-level persisted attributes selected by `keep`.
    pub fn retain_keys(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.encoded.retain(|key, _| keep(key));
    }

    /// Return the opaque hash for a top-level persisted secret attribute.
    pub fn redacted_secret_hash(&self, key: &str) -> Option<crate::resource::RedactedSecretHash> {
        self.encoded.get(key)?.view().redacted_secret_hash()
    }

    /// Return ordinary persisted string attributes for identity-prefix
    /// reconciliation. Typed secret hashes are deliberately excluded.
    pub fn plain_string_values(&self) -> HashMap<String, String> {
        self.encoded
            .keys()
            .filter_map(|key| match self.get(key) {
                Some(Value::Concrete(ConcreteValue::String(value))) => {
                    Some((key.clone(), value.into_plaintext()))
                }
                _ => None,
            })
            .collect()
    }

    pub(crate) fn encoded(&self) -> &HashMap<String, PersistedValue> {
        &self.encoded
    }

    /// Merge another already-validated persisted value.
    ///
    /// Taking `Self` rather than raw JSON keeps context-free and state-row
    /// decoding as the only construction boundaries.
    pub fn extend(&mut self, other: Self) {
        self.encoded.extend(other.encoded);
    }
}

/// Accepted desired values for nested write-only fields, grouped by their
/// top-level attribute.
///
/// Values stay in the typed persisted encoding so secrets are represented by
/// hashes and cannot accidentally be decoded as ordinary provider strings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NestedWriteOnlyRecord(BTreeMap<String, Vec<(LeafPath, PersistedValue)>>);

impl NestedWriteOnlyRecord {
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&[(LeafPath, PersistedValue)]> {
        self.0.get(key).map(Vec::as_slice)
    }

    /// Replace leaf records for write-only-bearing attributes that were sent
    /// to the provider. Ordinary keys and absent values are ignored. Taking
    /// the resource-owned `IndexMap` prevents an encoded persisted envelope
    /// from being used as the projection source.
    pub fn record_sent<'key>(
        &mut self,
        schema: &ResourceSchema,
        keys: impl IntoIterator<Item = &'key str>,
        desired_attributes: &IndexMap<String, Value>,
        previous: Option<&Self>,
    ) -> Result<(), SerializationError> {
        for key in keys {
            if !schema.write_only_bearing(key) {
                continue;
            }
            let Some(desired) = desired_attributes.get(key) else {
                continue;
            };
            let previous_leaves = previous.and_then(|record| record.get(key));
            let leaves = schema
                .write_only_leaf_projection(key, desired)
                .into_iter()
                .map(|(path, value, _field_type)| {
                    let previous_value = previous_leaves.and_then(|leaves| {
                        leaves
                            .iter()
                            .find_map(|(saved_path, saved)| (saved_path == &path).then_some(saved))
                    });
                    PersistedValue::encode_provider_value(&value, previous_value)
                        .map(|value| (path, value))
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.0.insert(key.to_string(), leaves);
        }
        Ok(())
    }

    pub fn remove_attribute(&mut self, key: &str) {
        self.0.remove(key);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretHashShape {
    None,
    TopLevel,
    Nested,
}

fn secret_hash_shape(value: &PersistedValue) -> SecretHashShape {
    secret_hash_shape_view(value.view())
}

fn secret_hash_shape_view(value: PersistedView<'_>) -> SecretHashShape {
    if value.is_redacted_secret() {
        SecretHashShape::TopLevel
    } else if value
        .as_list()
        .is_some_and(|items| items.iter().any(contains_secret_hash_view))
        || value.as_map().is_some_and(|values| {
            values
                .iter()
                .any(|(_, value)| contains_secret_hash_view(value))
        })
    {
        SecretHashShape::Nested
    } else {
        SecretHashShape::None
    }
}

fn contains_secret_hash_view(value: PersistedView<'_>) -> bool {
    secret_hash_shape_view(value) != SecretHashShape::None
}

/// Preserve secret hashes from a previous persisted value while retaining
/// provider material at every unprotected path. Returns `None` when the
/// previous value contains no secret hash. Unavailable provider plaintext
/// returns its typed serialization error rather than retaining stale evidence.
pub fn merge_previous_secret_hashes_into_provider_value(
    previous_value: &PersistedValue,
    provider_value: Option<&PersistedValue>,
) -> Result<Option<PersistedValue>, SerializationError> {
    Ok(match (secret_hash_shape(previous_value), provider_value) {
        (SecretHashShape::None, _) => None,
        (SecretHashShape::TopLevel, Some(provider_value))
            if secret_hash_shape(provider_value) == SecretHashShape::TopLevel =>
        {
            Some(provider_value.clone())
        }
        (SecretHashShape::TopLevel, Some(provider_value)) if !provider_value.view().is_null() => {
            Some(hash_provider_json_as_secret(
                provider_value,
                Some(previous_value),
            )?)
        }
        (SecretHashShape::TopLevel, _) => Some(previous_value.clone()),
        (SecretHashShape::Nested, provider_value) => Some(
            merge_previous_secret_hashes_into_provider_json(previous_value, provider_value)?,
        ),
    })
}

fn merge_previous_secret_hashes_into_provider_json(
    previous_value: &PersistedValue,
    provider_value: Option<&PersistedValue>,
) -> Result<PersistedValue, SerializationError> {
    let merged = merge_previous_secret_hashes_into_provider_json_inner(
        previous_value.view(),
        provider_value.map(PersistedValue::view),
    )?;
    Ok(PersistedValue::from_serialized_json(merged)
        .expect("merging validated persisted trees preserves the host namespace"))
}

fn merge_previous_secret_hashes_into_provider_json_inner(
    previous_value: PersistedView<'_>,
    provider_value: Option<PersistedView<'_>>,
) -> Result<serde_json::Value, SerializationError> {
    if previous_value.is_redacted_secret() {
        let Some(provider_value) = provider_value.filter(|value| !value.is_null()) else {
            return Ok(previous_value.to_owned_json());
        };
        if secret_hash_shape_view(provider_value) == SecretHashShape::TopLevel {
            return Ok(provider_value.to_owned_json());
        }
        return hash_provider_view_as_secret(provider_value, Some(previous_value));
    }

    if let Some(previous_map) = previous_value.as_map() {
        let Some(provider_map) = provider_value.and_then(PersistedView::as_map) else {
            return Ok(previous_value.to_owned_json());
        };
        let replacements = previous_map
            .iter()
            .filter(|(_, previous_child)| contains_secret_hash_view(*previous_child))
            .map(|(key, previous_child)| {
                merge_previous_secret_hashes_into_provider_json_inner(
                    previous_child,
                    provider_map.get(key),
                )
                .map(|value| (key.to_string(), value))
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(provider_map.replacing_children(replacements));
    }

    if let Some(previous_items) = previous_value.as_list() {
        let Some(provider_value) = provider_value else {
            return Ok(previous_value.to_owned_json());
        };
        let Some(provider_items) = provider_value.as_list() else {
            return Ok(previous_value.to_owned_json());
        };
        if !previous_secret_hash_array_alignment_is_trusted(previous_items, provider_items) {
            return hash_provider_view_as_secret(provider_value, Some(previous_value));
        }
        let replacements = previous_items
            .iter()
            .enumerate()
            .filter(|(_, previous_child)| contains_secret_hash_view(*previous_child))
            .map(|(index, previous_child)| {
                merge_previous_secret_hashes_into_provider_json_inner(
                    previous_child,
                    provider_items.get(index),
                )
                .map(|value| (index, value))
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(provider_items.replacing_children(replacements));
    }

    Ok(provider_value.map_or_else(
        || previous_value.to_owned_json(),
        PersistedView::to_owned_json,
    ))
}

fn previous_secret_hash_array_alignment_is_trusted(
    previous_items: PersistedListView<'_>,
    provider_items: PersistedListView<'_>,
) -> bool {
    previous_items.len() == provider_items.len()
        && previous_items
            .iter()
            .zip(provider_items.iter())
            .filter(|(previous, _)| contains_secret_hash_view(*previous))
            .all(|(previous, provider)| {
                let (Some(previous_object), Some(provider_object)) =
                    (previous.as_map(), provider.as_map())
                else {
                    return false;
                };
                let anchors: Vec<_> = previous_object
                    .iter()
                    .filter(|(_, value)| !contains_secret_hash_view(*value))
                    .collect();
                !anchors.is_empty()
                    && anchors.into_iter().all(|(key, value)| {
                        provider_object
                            .get(key)
                            .is_some_and(|provider| provider.encoded_eq(value))
                    })
            })
}

pub(crate) fn hash_provider_json_as_secret(
    provider_value: &PersistedValue,
    previous: Option<&PersistedValue>,
) -> Result<PersistedValue, SerializationError> {
    hash_provider_view_as_secret(provider_value.view(), previous.map(PersistedValue::view)).map(
        |encoded| {
            PersistedValue::from_serialized_json(encoded)
                .expect("a structural persisted secret is valid persisted JSON")
        },
    )
}

fn hash_provider_view_as_secret(
    provider_value: PersistedView<'_>,
    previous: Option<PersistedView<'_>>,
) -> Result<serde_json::Value, SerializationError> {
    let plaintext = provider_value
        .decode_value()
        .ok_or(SerializationError::NullProviderSecret)?;
    crate::value::persist_secret_json(&plaintext, previous)
}

/// Saved attributes decoded from persisted state but not yet reconciled
/// with the current provider schemas for enum or structural value shapes.
///
/// This type cannot be passed to planning or refresh hydration. Consume it
/// with [`RawSavedAttrs::lift`] to obtain [`LiftedSavedAttrs`].
/// Persisted attributes cannot be admitted without their nested write-only
/// provenance map:
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::provider::{PersistedAttributes, RawSavedAttrs};
/// use carina_core::resource::ResourceId;
///
/// let persisted: HashMap<ResourceId, PersistedAttributes> = HashMap::new();
/// let _saved = RawSavedAttrs::from_persisted(persisted);
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawSavedAttrs {
    attributes: SavedAttrs,
    nested_write_only: HashMap<ResourceId, NestedWriteOnlyRecord>,
}

impl RawSavedAttrs {
    /// Collect persisted attributes and their nested write-only provenance as
    /// one planning input so callers cannot accidentally drop the leaf record.
    pub fn from_persisted(
        saved_attrs: HashMap<ResourceId, PersistedAttributes>,
        nested_write_only: HashMap<ResourceId, NestedWriteOnlyRecord>,
    ) -> Self {
        Self {
            attributes: SavedAttrs(
                saved_attrs
                    .into_iter()
                    .map(|(id, attributes)| {
                        let values = attributes.to_values();
                        (id, values)
                    })
                    .collect(),
            ),
            nested_write_only,
        }
    }

    /// Construct saved attributes directly for tests that exercise invalid
    /// or otherwise non-JSON-representable provider-boundary values.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn from_decoded_values_for_test(
        saved_attrs: HashMap<ResourceId, HashMap<String, crate::resource::SnapshotValue>>,
    ) -> Self {
        Self {
            attributes: SavedAttrs(saved_attrs),
            nested_write_only: HashMap::new(),
        }
    }

    /// Lift enum leaves and structurally canonicalize every persisted entry
    /// using the schema selected by its resource ID.
    ///
    /// carina#3740: both rewrites live at this single raw-to-lifted seam so a
    /// saved map cannot reach hydration or the differ with only one applied.
    pub fn lift(mut self, registry: &SchemaRegistry) -> LiftedSavedAttrs {
        crate::utils::lift_saved_state_enum_leaves(&mut self.attributes.0, registry);
        LiftedSavedAttrs {
            attributes: self.attributes,
            nested_write_only: self.nested_write_only,
        }
    }

    /// Read a raw resource entry without changing its typestate.
    pub fn get(&self, id: &ResourceId) -> Option<&HashMap<String, crate::resource::SnapshotValue>> {
        self.attributes.get(id)
    }
}

/// Saved attributes whose enum leaves and structural value shapes have been
/// canonicalized against the current schemas and are safe for hydration and
/// planning.
///
/// carina#3739: the private field deliberately prevents construction from a
/// plain map. The only public conversion from persisted data is
/// [`RawSavedAttrs::lift`].
///
/// ```compile_fail
/// use carina_core::provider::{LiftedSavedAttrs, RawSavedAttrs};
///
/// fn requires_lifted(_: &LiftedSavedAttrs) {}
/// let raw = RawSavedAttrs::default();
/// requires_lifted(&raw);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct LiftedSavedAttrs {
    attributes: SavedAttrs,
    nested_write_only: HashMap<ResourceId, NestedWriteOnlyRecord>,
}

impl LiftedSavedAttrs {
    /// Borrow the plugin-ABI representation at a provider hydration boundary.
    pub fn as_provider_saved_attrs(&self) -> &SavedAttrs {
        &self.attributes
    }

    /// Read saved attributes for one resource.
    pub fn get(&self, id: &ResourceId) -> Option<&HashMap<String, crate::resource::SnapshotValue>> {
        self.attributes.get(id)
    }

    /// Read the accepted nested write-only leaf record for one resource.
    pub fn nested_write_only(&self, id: &ResourceId) -> &NestedWriteOnlyRecord {
        static EMPTY: NestedWriteOnlyRecord = NestedWriteOnlyRecord::new();
        self.nested_write_only.get(id).unwrap_or(&EMPTY)
    }

    /// Iterate resource IDs without exposing mutable map access.
    pub fn resource_ids(&self) -> impl Iterator<Item = &ResourceId> {
        self.attributes.0.keys()
    }

    /// Transfer a resource's saved attributes to a new ID.
    pub fn remap_resource_id(&mut self, from: &ResourceId, to: ResourceId) {
        if let Some(attrs) = self.attributes.0.remove(from) {
            self.attributes.0.insert(to.clone(), attrs);
        }
        if let Some(record) = self.nested_write_only.remove(from) {
            self.nested_write_only.insert(to, record);
        }
    }
}

/// State known to identify an existing provider-side resource.
///
/// Successful reads and mutations always represent an existing resource and
/// carry a non-empty provider-assigned identifier. Conversion to generic
/// [`State`] is centralized in [`AppliedState::into_state_for_writeback`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedState {
    pub(crate) id: ResourceId,
    pub(crate) identifier: AppliedIdentifier,
    pub(crate) attributes: HashMap<String, crate::resource::SnapshotValue>,
}

impl AppliedState {
    pub fn new(
        id: ResourceId,
        identifier: impl Into<String>,
        attributes: HashMap<String, Value>,
    ) -> Result<Self, EmptyAppliedIdentifier> {
        Ok(Self {
            id,
            identifier: AppliedIdentifier::new(identifier)?,
            attributes: attributes
                .into_iter()
                .map(|(key, value)| (key, crate::resource::state_value_snapshot(&value)))
                .collect(),
        })
    }

    pub fn from_identifier(
        id: ResourceId,
        identifier: AppliedIdentifier,
        attributes: HashMap<String, Value>,
    ) -> Self {
        Self {
            id,
            identifier,
            attributes: attributes
                .into_iter()
                .map(|(key, value)| (key, crate::resource::state_value_snapshot(&value)))
                .collect(),
        }
    }

    pub fn from_snapshot_parts(
        id: ResourceId,
        identifier: AppliedIdentifier,
        attributes: HashMap<String, crate::resource::SnapshotValue>,
    ) -> Self {
        Self {
            id,
            identifier,
            attributes,
        }
    }

    pub fn id(&self) -> &ResourceId {
        &self.id
    }

    pub fn identifier(&self) -> &AppliedIdentifier {
        &self.identifier
    }

    pub fn attributes(&self) -> &HashMap<String, crate::resource::SnapshotValue> {
        &self.attributes
    }

    pub fn into_parts(
        self,
    ) -> (
        ResourceId,
        AppliedIdentifier,
        HashMap<String, crate::resource::SnapshotValue>,
    ) {
        (self.id, self.identifier, self.attributes)
    }

    /// Convert a successful managed-resource observation to the generic state
    /// persisted by the executor. This is the sole applied-state to read-state
    /// conversion seam.
    pub fn into_state_for_writeback(self) -> crate::resource::SnapshotState {
        let Self {
            id,
            identifier,
            attributes,
        } = self;
        State::existing(id, attributes).with_identifier(identifier.into_inner())
    }
}

/// Result of reading a managed resource by its non-empty provider identifier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReadOutcome {
    /// The provider identifier no longer resolves to a resource.
    NotFound { id: ResourceId },
    /// The provider observed an existing resource with a non-empty identifier.
    Found { state: AppliedState },
}

impl ReadOutcome {
    pub fn not_found(id: ResourceId) -> Self {
        Self::NotFound { id }
    }

    pub fn found(state: AppliedState) -> Self {
        Self::Found { state }
    }

    /// Convert the typed provider observation to the generic state consumed by
    /// refresh, planning, and persistence. This is the only read-result seam
    /// that constructs generic managed state.
    pub fn into_state(self) -> crate::resource::SnapshotState {
        match self {
            Self::NotFound { id } => State::not_found(id),
            Self::Found { state } => state.into_state_for_writeback(),
        }
    }
}

/// Convert a generic state fixture into the typed managed-read outcome.
#[cfg(test)]
pub(crate) fn fixture_read_outcome<P: StateValueLeaf>(state: State<P>) -> ReadOutcome {
    if !state.exists {
        return ReadOutcome::not_found(state.id);
    }
    let identifier = state
        .identifier
        .expect("existing read fixture must carry a provider identifier");
    ReadOutcome::found(AppliedState::from_snapshot_parts(
        state.id,
        AppliedIdentifier::new(identifier)
            .expect("existing read fixture identifier must be non-empty"),
        crate::test_support::snapshot_attributes(state.attributes),
    ))
}

/// Convert a generic state fixture into a managed-read outcome, using the
/// provider identifier supplied to the read when the fixture omits one.
#[cfg(test)]
pub(crate) fn fixture_read_outcome_for_identifier<P: StateValueLeaf>(
    state: State<P>,
    identifier: &AppliedIdentifier,
) -> ReadOutcome {
    if !state.exists {
        return ReadOutcome::not_found(state.id);
    }
    let identifier = match state.identifier {
        Some(identifier) => AppliedIdentifier::new(identifier)
            .expect("existing read fixture identifier must be non-empty"),
        None => identifier.clone(),
    };
    ReadOutcome::found(AppliedState::from_snapshot_parts(
        state.id,
        identifier,
        crate::test_support::snapshot_attributes(state.attributes),
    ))
}

/// Result of a provider create operation.
///
/// Providers must finish the executor-issued attempt instead of constructing
/// a success-shaped value directly.
///
/// ```compile_fail
/// use carina_core::provider::{AppliedState, CreateOutcome};
///
/// fn forge_success(state: AppliedState) -> CreateOutcome {
///     CreateOutcome::Success { state }
/// }
/// ```
#[derive(Debug)]
pub struct CreateOutcome<S: TextState = Unbound> {
    kind: CreateOutcomeKind<S>,
    read_back_end: ReadBackEnd,
}

/// Read-only classification exposed by an opaque [`CreateOutcome`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum CreateOutcomeKind<S: TextState = Unbound> {
    /// Create completed and the returned state is complete.
    Success { state: AppliedState },
    /// Create completed but the provider could not fully observe state.
    PartialSuccess {
        state: AppliedState,
        diagnostic: PartialReadDiagnostic<S>,
    },
}

/// Result of a provider update operation.
#[derive(Debug, Clone)]
pub struct UpdateOutcome<S: TextState = Unbound> {
    kind: UpdateOutcomeKind<S>,
    read_back_end: ReadBackEnd,
}

/// Read-only classification exposed by an opaque [`UpdateOutcome`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "PartialReadDiagnostic<S>: Serialize",
    deserialize = "PartialReadDiagnostic<S>: Deserialize<'de>"
))]
pub enum UpdateOutcomeKind<S: TextState = Unbound> {
    /// Update completed and the returned state is complete.
    Success { state: AppliedState },
    /// Update completed but the provider could not fully observe state.
    PartialSuccess {
        state: AppliedState,
        diagnostic: PartialReadDiagnostic<S>,
    },
}

/// Missing attribute names validated before a partial-read reason is consumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonEmptyMissingAttributes(Vec<String>);
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("a partial read requires at least one missing attribute")]
pub struct EmptyMissingAttributes;
impl TryFrom<Vec<String>> for NonEmptyMissingAttributes {
    type Error = EmptyMissingAttributes;
    fn try_from(attributes: Vec<String>) -> Result<Self, Self::Error> {
        if attributes.is_empty() {
            Err(EmptyMissingAttributes)
        } else {
            Ok(Self(attributes))
        }
    }
}

/// Diagnostic details for a partial post-mutation read.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "Diagnostic<S>: Serialize",
    deserialize = "Diagnostic<S>: Deserialize<'de>"
))]
pub struct PartialReadDiagnostic<S: TextState = Unbound> {
    pub(crate) reason: Diagnostic<S>,
    pub(crate) missing_attributes: Vec<ProviderKey>,
}

impl<S: TextState> fmt::Debug for PartialReadDiagnostic<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PartialReadDiagnostic")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

/// Validated UTC RFC3339 timestamp used only to describe when a persisted
/// create first entered the in-flight state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Timestamp(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("timestamp must be a valid UTC RFC3339 value")]
pub struct InvalidTimestamp;

impl Timestamp {
    pub fn now() -> Self {
        Self(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Timestamp {
    type Error = InvalidTimestamp;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_utc_rfc3339(&value)?;
        Ok(Self(value))
    }
}

impl TryFrom<&str> for Timestamp {
    type Error = InvalidTimestamp;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_from(value.to_string())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn validate_utc_rfc3339(value: &str) -> Result<(), InvalidTimestamp> {
    if !value.ends_with('Z') {
        return Err(InvalidTimestamp);
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|_timestamp| ())
        .map_err(|_error| InvalidTimestamp)
}

/// An unfinished create step minted only by core create typestate handles.
/// Linear pending-create authority. Polling or classifying it consumes the
/// value, and it cannot be duplicated.
///
/// ```compile_fail
/// use carina_core::provider::PendingCreate;
///
/// fn duplicate(pending: PendingCreate) {
///     let _second_authority = pending.clone();
/// }
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct PendingCreate {
    id: ResourceId,
    token: PendingToken,
    progress: CreateProgress,
    sent: Sent,
    poll_after_secs: u32,
    /// Time the current create request first returned a pending step. A
    /// re-pend preserves it; a create sent after `resend` receives a new one.
    started_at: Timestamp,
}

/// Read-only record retained after a provider returns a fresh pending create.
/// Unlike [`PendingCreate`], this is storage, not transition authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshPendingCreate {
    id: ResourceId,
    token: PendingToken,
    progress: CreateProgress,
    sent: Sent,
    poll_after_secs: u32,
    started_at: Timestamp,
}

impl From<PendingCreate> for FreshPendingCreate {
    fn from(pending: PendingCreate) -> Self {
        Self {
            id: pending.id,
            token: pending.token,
            progress: pending.progress,
            sent: pending.sent,
            poll_after_secs: pending.poll_after_secs,
            started_at: pending.started_at,
        }
    }
}

impl FreshPendingCreate {
    pub fn token(&self) -> &PendingToken {
        &self.token
    }

    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        self.progress.identifier()
    }

    pub fn sent(&self) -> &Sent {
        &self.sent
    }

    /// Desired values and send evidence retained by this fresh continuation.
    pub fn sent_desired_attributes_for<'a, S, D>(
        &self,
        resource: &'a Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> SentDesiredAttributes<'a> {
        SentDesiredAttributes::new(
            resource.attributes.iter(),
            SentAttributes::from_create_sent(&self.sent, resource),
        )
    }

    pub fn poll_after_secs(&self) -> u32 {
        self.poll_after_secs
    }

    pub fn progress(&self) -> &CreateProgress {
        &self.progress
    }

    pub fn phase(&self) -> carina_provider_protocol::PendingPhase {
        self.progress.phase()
    }

    pub fn started_at(&self) -> &Timestamp {
        &self.started_at
    }

    pub fn persist(
        &self,
        write_only: PersistedAttributes,
        secrets: PersistedAttributes,
    ) -> InFlightCreate {
        InFlightCreate {
            id: self.id.clone(),
            pending: PendingRecord::Persisted(PendingEnvelope::new(
                self.token.clone(),
                self.identifier().cloned(),
                write_only,
                secrets,
            )),
            started_at: self.started_at.clone(),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn into_authority(self) -> PendingCreate {
        PendingCreate {
            id: self.id,
            token: self.token,
            progress: self.progress,
            sent: self.sent,
            poll_after_secs: self.poll_after_secs,
            started_at: self.started_at,
        }
    }
}

impl PendingCreate {
    fn new(
        id: ResourceId,
        token: PendingToken,
        progress: CreateProgress,
        sent: Sent,
        poll_after_secs: u32,
        started_at: Timestamp,
    ) -> Self {
        Self {
            id,
            token,
            progress,
            sent,
            poll_after_secs,
            started_at,
        }
    }

    pub fn token(&self) -> &PendingToken {
        &self.token
    }

    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        self.progress.identifier()
    }

    pub fn sent(&self) -> &Sent {
        &self.sent
    }

    pub fn poll_after_secs(&self) -> u32 {
        self.poll_after_secs
    }

    pub fn progress(&self) -> &CreateProgress {
        &self.progress
    }

    pub fn phase(&self) -> carina_provider_protocol::PendingPhase {
        self.progress.phase()
    }

    pub fn started_at(&self) -> &Timestamp {
        &self.started_at
    }

    /// Preserve an issued pending create and end an identifier-less pending
    /// create when the provider reports that it was not created.
    /// Consuming the witness makes the two outcomes mutually exclusive.
    pub fn not_created(self, error: ProviderError) -> CreateFailure {
        if self.identifier().is_some() {
            InFlightCreate::from_pending(self).into_create_failure(error)
        } else {
            CreateFailure::from_kind(CreateFailureKind::NoResource(error))
        }
    }

    pub fn preserve(self, error: ProviderError) -> CreateFailure {
        InFlightCreate::from_pending(self).into_create_failure(error)
    }

    /// Bind the state-file encoding of this continuation to the same route
    /// that minted the pending witness.
    pub fn persist(
        &self,
        write_only: PersistedAttributes,
        secrets: PersistedAttributes,
    ) -> InFlightCreate {
        InFlightCreate {
            id: self.id.clone(),
            pending: PendingRecord::Persisted(PendingEnvelope::new(
                self.token.clone(),
                self.identifier().cloned(),
                write_only,
                secrets,
            )),
            started_at: self.started_at.clone(),
        }
    }

    /// Consume the continuation and recover the only poll witness valid for
    /// its identifier state.
    pub fn into_poll(self) -> Poll {
        match self.progress.into_identifier() {
            Some(identifier) => Poll::Issued(IssuedCreate {
                id: self.id,
                identifier,
                sent: self.sent,
                started_at: Some(self.started_at),
            }),
            None => Poll::Unissued(PollAttempt {
                id: self.id,
                sent: self.sent,
                started_at: self.started_at,
            }),
        }
    }
}

/// Typestate selected from a pending create immediately before polling.
#[derive(Debug)]
pub enum Poll {
    Unissued(PollAttempt),
    Issued(IssuedCreate),
}

/// Provider-facing poll witness for a create without an identifier.
///
/// ```compile_fail
/// use carina_core::provider::PollAttempt;
///
/// let _ = PollAttempt::new();
/// ```
#[derive(Debug)]
pub struct PollAttempt {
    id: ResourceId,
    sent: Sent,
    started_at: Timestamp,
}

impl PollAttempt {
    pub fn not_created(self, error: ProviderError) -> CreateFailure {
        CreateFailure::from_kind(CreateFailureKind::NoResource(error))
    }

    pub fn issued(self, identifier: AppliedIdentifier) -> IssuedCreate {
        IssuedCreate {
            id: self.id,
            identifier,
            sent: self.sent,
            started_at: Some(self.started_at),
        }
    }

    pub fn pending(self, token: PendingToken, poll_after_secs: u32) -> PendingCreate {
        PendingCreate::new(
            self.id,
            token,
            CreateProgress::Operating { identifier: None },
            self.sent,
            poll_after_secs,
            self.started_at,
        )
    }
}

/// Persisted continuation and sent-value encoding for an unfinished create.
///
/// The provider's `poll_after_secs` hint is deliberately not persisted. The
/// next apply resumes immediately; the token and identifier route that resume
/// to the same operation, and the provider's fresh pending answer drives the
/// in-process poll loop from there. The worst case is one early status call per
/// user-run apply, while a persisted deadline would put the wall clock into
/// state and still need a policy for an apply that arrives early.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingEnvelope {
    token: PendingToken,
    identifier: Option<AppliedIdentifier>,
    write_only: PersistedAttributes,
    secrets: PersistedAttributes,
}

impl PendingEnvelope {
    pub(crate) fn new(
        token: PendingToken,
        identifier: Option<AppliedIdentifier>,
        write_only: PersistedAttributes,
        secrets: PersistedAttributes,
    ) -> Self {
        Self {
            token,
            identifier,
            write_only,
            secrets,
        }
    }

    pub fn token(&self) -> &PendingToken {
        &self.token
    }

    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        self.identifier.as_ref()
    }

    pub fn write_only(&self) -> &PersistedAttributes {
        &self.write_only
    }

    pub fn secrets(&self) -> &PersistedAttributes {
        &self.secrets
    }

    fn with_pending_record(
        mut self,
        token: PendingToken,
        identifier: Option<AppliedIdentifier>,
    ) -> Self {
        self.token = token;
        self.identifier = identifier;
        self
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedPendingEnvelope {
    token: PendingToken,
    #[serde(default)]
    identifier: Option<AppliedIdentifier>,
    write_only: HashMap<String, serde_json::Value>,
    secrets: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EncodedStateRowCurrent {
    CreateInFlight {
        envelope: EncodedPendingEnvelope,
        started_at: Timestamp,
    },
    CreateOutcomeUnknown {
        started_at: Timestamp,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct EncodedStateRowWithCurrent {
    resource_type: String,
    identity: crate::resource::ResourceIdentity,
    provider: String,
    #[serde(default)]
    directives: Directives,
    current: EncodedStateRowCurrent,
}

struct ProviderErrorSourceChain<'a>(&'a (dyn std::error::Error + 'static));

impl std::fmt::Display for ProviderErrorSourceChain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(error) = source {
            write!(f, ": {error}")?;
            source = error.source();
        }
        Ok(())
    }
}

/// Values known to have been sent by the accepted create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentRecord {
    Request(Sent),
    Persisted(PendingEnvelope),
}

impl SentRecord {
    /// Recover attribute send evidence while it is still retained in request
    /// form. Persisted continuations intentionally omit ordinary observable
    /// attribute names, so they cannot claim a sent desired basis.
    fn sent_attributes_for<S, D>(
        &self,
        resource: &Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> Option<SentAttributes> {
        match self {
            Self::Request(sent) => Some(SentAttributes::from_create_sent(sent, resource)),
            Self::Persisted(_envelope) => None,
        }
    }
}

/// What carina-core knows about a create for which the provider reported an
/// identifier.
#[derive(Debug)]
pub enum CreateObservation<S: TextState = Unbound> {
    /// The read-back found the resource.
    Observed(AppliedState),
    /// The resource could not be observed after the identifier was reported.
    Unobserved(UnobservedReason<S>),
}

/// Why a create with a provider-reported identifier could not be observed.
#[derive(Debug)]
pub enum UnobservedReason<S: TextState = Unbound> {
    /// The read-back reported that the identifier did not resolve.
    NotFound,
    /// The read-back itself failed.
    ReadFailed(ProviderFailure<S>),
}

impl std::fmt::Display for UnobservedReason<Bound> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("provider read after create returned not found"),
            Self::ReadFailed(error) => write!(f, "{error}"),
        }
    }
}

/// A provider-reported identifier, its observation, and the accepted create's sent values.
#[derive(Debug)]
pub struct IdentifiedCreate<S: TextState = Unbound> {
    id: ResourceId,
    identifier: AppliedIdentifier,
    observation: CreateObservation<S>,
    sent: SentRecord,
    partial_read: Option<Box<PartialReadDiagnostic<S>>>,
    resolved_desired_attributes: Option<SecretPreservingAttributes>,
}

impl<S: TextState> IdentifiedCreate<S> {
    fn new(
        id: ResourceId,
        identifier: AppliedIdentifier,
        observation: Observation<S>,
        sent: SentRecord,
    ) -> Self {
        let observation = match observation {
            Observation::Read(ReadOutcome::Found { state }) => CreateObservation::Observed(state),
            Observation::Read(ReadOutcome::NotFound { id: _id }) => {
                CreateObservation::Unobserved(UnobservedReason::NotFound)
            }
            Observation::ReadFailed(error) => {
                CreateObservation::Unobserved(UnobservedReason::ReadFailed(error))
            }
        };
        Self::from_create_observation(id, identifier, observation, sent)
    }

    fn from_create_observation(
        id: ResourceId,
        identifier: AppliedIdentifier,
        observation: CreateObservation<S>,
        sent: SentRecord,
    ) -> Self {
        Self {
            id,
            identifier,
            observation,
            sent,
            partial_read: None,
            resolved_desired_attributes: None,
        }
    }

    /// Construct the observation produced by a resumed in-flight create in
    /// cross-crate tests.
    #[cfg(feature = "test-utils")]
    #[doc(hidden)]
    pub fn from_in_flight_for_test(
        pending: InFlightCreate,
        identifier: AppliedIdentifier,
        observation: CreateObservation<S>,
    ) -> Self {
        identified_from_resume(pending, identifier, observation)
    }

    pub fn id(&self) -> &ResourceId {
        &self.id
    }

    pub fn identifier(&self) -> &AppliedIdentifier {
        &self.identifier
    }

    pub fn observation(&self) -> &CreateObservation<S> {
        &self.observation
    }

    pub fn sent(&self) -> &SentRecord {
        &self.sent
    }

    pub fn partial_read_diagnostic(&self) -> Option<&PartialReadDiagnostic<S>> {
        self.partial_read.as_deref()
    }

    /// Resolved desired values and send evidence retained by the accepted
    /// create observation.
    pub fn sent_desired_attributes_for<'a, M, D>(
        &'a self,
        fallback_resource: &'a Resource<
            M,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> Option<SentDesiredAttributes<'a>> {
        let sent = self.sent.sent_attributes_for(fallback_resource)?;
        Some(match self.resolved_desired_attributes.as_ref() {
            Some(attributes) => SentDesiredAttributes::new(attributes.values().iter(), sent),
            None => SentDesiredAttributes::new(fallback_resource.attributes.iter(), sent),
        })
    }

    pub(crate) fn with_resolved_desired_attributes(
        mut self,
        attributes: SecretPreservingAttributes,
    ) -> Self {
        self.resolved_desired_attributes = Some(attributes);
        self
    }

    fn with_partial_read(mut self, diagnostic: PartialReadDiagnostic<S>) -> Self {
        self.partial_read = Some(Box::new(diagnostic));
        self
    }

    /// Attach a partial read-back diagnostic in cross-crate tests.
    #[cfg(feature = "test-utils")]
    #[doc(hidden)]
    pub fn with_partial_read_for_test(self, diagnostic: PartialReadDiagnostic<S>) -> Self {
        self.with_partial_read(diagnostic)
    }

    pub fn resolved_desired_attributes(&self) -> Option<&SecretPreservingAttributes> {
        self.resolved_desired_attributes.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        ResourceId,
        AppliedIdentifier,
        CreateObservation<S>,
        SentRecord,
    ) {
        (self.id, self.identifier, self.observation, self.sent)
    }

    pub(crate) fn post_error_note(&self) -> DiagnosticText {
        match &self.observation {
            CreateObservation::Observed(_state) => {
                "resource created; recorded in state, the next apply completes it".into()
            }
            CreateObservation::Unobserved(reason) => {
                let reason = match reason {
                    UnobservedReason::NotFound => {
                        "provider read after create returned not found".to_owned()
                    }
                    UnobservedReason::ReadFailed(error) => error.host_diagnostic_text(),
                }
                .replace('\n', "\n  ");
                format!(
                    "{} reported identifier '{}' but the resource could not be observed; recorded in state so the next plan re-checks it\n  reason: {reason}",
                    self.id.provider, self.identifier,
                ).into()
            }
        }
    }

    pub(crate) fn validate_request_sent(
        mut self,
        request_attributes: &std::collections::BTreeSet<String>,
    ) -> (Self, bool) {
        let valid = match &mut self.sent {
            SentRecord::Request(sent) => sent.clip_to(request_attributes),
            SentRecord::Persisted(_envelope) => true,
        };
        (self, valid)
    }
}

impl IdentifiedCreate<Bound> {
    /// Materialize only the provider observation for runtime publication.
    /// Sent desired values are intentionally excluded.
    pub fn observed_state(&self) -> SnapshotState {
        let attributes = match &self.observation {
            CreateObservation::Observed(state) => state.attributes().clone(),
            CreateObservation::Unobserved(_reason) => HashMap::new(),
        };
        let state = State::existing(self.id.clone(), attributes)
            .with_identifier(self.identifier.as_str().to_string());
        match &self.partial_read {
            Some(diagnostic) => diagnostic.as_ref().clone().into_state_for_writeback(state),
            None => state,
        }
    }
}

pub type Observation<S = Unbound> = protocol_types::Observation<ReadOutcome, ProviderFailure<S>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingRecord {
    Fresh(FreshPendingCreate),
    Persisted(PendingEnvelope),
}

/// Exhaustive borrowed view of how an in-flight create entered this process.
pub enum InFlightPersistence<'a> {
    /// Produced by a provider call in this process and not encoded yet.
    Fresh(&'a FreshPendingCreate),
    /// Loaded from a validated persisted-state row.
    Persisted,
}

/// A create continuation bound to the exact resource route it was issued for.
///
/// Persisted continuations are produced only by decoding their complete state
/// row, so no API can bind an already-decoded envelope to an arbitrary route.
///
/// ```compile_fail
/// use carina_core::provider::{InFlightCreate, PendingEnvelope, Timestamp};
/// use carina_core::resource::ResourceId;
///
/// fn forge_binding(
///     id: ResourceId,
///     envelope: PendingEnvelope,
///     started_at: Timestamp,
/// ) -> InFlightCreate {
///     InFlightCreate::from_persisted(id, envelope, started_at)
/// }
/// ```
///
/// Re-pending is likewise available only after selecting a resume witness:
///
/// ```compile_fail
/// use carina_core::provider::{InFlightCreate, PendingToken};
///
/// fn bypass_resume_witness(pending: InFlightCreate, token: PendingToken) {
///     let _ = pending.repend(token);
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlightCreate {
    id: ResourceId,
    pending: PendingRecord,
    started_at: Timestamp,
}

impl Serialize for InFlightCreate {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Encoded<'a> {
            envelope: &'a PendingEnvelope,
            started_at: &'a Timestamp,
        }

        let PendingRecord::Persisted(envelope) = &self.pending else {
            return Err(serde::ser::Error::custom(
                "a fresh create continuation must be persisted with its saved attributes first",
            ));
        };
        Encoded {
            envelope,
            started_at: &self.started_at,
        }
        .serialize(serializer)
    }
}

/// Loader-produced binding of an ID-less persisted envelope to its containing
/// state row. The tuple field is private, and [`InFlightCreate`] itself does
/// not implement `Deserialize`; state loaders must decode the complete row.
///
/// ```compile_fail
/// use carina_core::provider::{InFlightCreate, StateRowInFlightCreate};
///
/// fn bypass(pending: InFlightCreate) -> StateRowInFlightCreate {
///     StateRowInFlightCreate(pending)
/// }
/// ```
#[derive(Debug)]
pub struct StateRowInFlightCreate(InFlightCreate);

impl StateRowInFlightCreate {
    pub fn into_inner(self) -> InFlightCreate {
        self.0
    }
}

/// Loader-produced binding of an outcome-unknown timestamp to its containing
/// state-row address.
#[derive(Debug)]
pub struct StateRowUnknownCreateOutcome(UnknownCreateOutcome);

impl StateRowUnknownCreateOutcome {
    pub fn into_inner(self) -> UnknownCreateOutcome {
        self.0
    }
}

impl<'de> Deserialize<'de> for StateRowUnknownCreateOutcome {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let row = EncodedStateRowWithCurrent::deserialize(deserializer)?;
        let EncodedStateRowCurrent::CreateOutcomeUnknown { started_at } = row.current else {
            return Err(serde::de::Error::custom(
                "state row does not contain an outcome-unknown create",
            ));
        };
        let id = ResourceId::with_provider_identity(
            row.provider,
            row.resource_type,
            row.identity,
            row.directives.provider_instance,
        );
        Ok(Self(UnknownCreateOutcome::new(id, started_at)))
    }
}

impl<'de> Deserialize<'de> for StateRowInFlightCreate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let row = EncodedStateRowWithCurrent::deserialize(deserializer)?;
        let EncodedStateRowCurrent::CreateInFlight {
            envelope,
            started_at,
        } = row.current
        else {
            return Err(serde::de::Error::custom(
                "state row does not contain an in-flight create",
            ));
        };
        let write_only = PersistedAttributes::from_encoded(envelope.write_only)
            .map_err(|error| serde::de::Error::custom(ProviderErrorSourceChain(&error)))?;
        let secrets = PersistedAttributes::from_encoded(envelope.secrets)
            .map_err(|error| serde::de::Error::custom(ProviderErrorSourceChain(&error)))?;
        let id = ResourceId::with_provider_identity(
            row.provider,
            row.resource_type,
            row.identity,
            row.directives.provider_instance,
        );
        Ok(Self(InFlightCreate {
            id,
            pending: PendingRecord::Persisted(PendingEnvelope::new(
                envelope.token,
                envelope.identifier,
                write_only,
                secrets,
            )),
            started_at,
        }))
    }
}

impl InFlightCreate {
    pub(crate) fn from_pending(pending: PendingCreate) -> Self {
        let id = pending.id.clone();
        let started_at = pending.started_at.clone();
        Self {
            id,
            pending: PendingRecord::Fresh(pending.into()),
            started_at,
        }
    }

    /// Convert a continuation produced by a native provider or host adapter
    /// into the classified failure channel for an initial create.
    pub(crate) fn into_create_failure<S: TextState>(
        self,
        error: ProviderFailure<S>,
    ) -> CreateFailure<S> {
        CreateFailure::from_kind(CreateFailureKind::InFlight {
            pending: self,
            error,
        })
    }

    /// Preserve this continuation in the classified resume failure channel.
    pub(crate) fn into_resume_failure<S: TextState>(
        self,
        error: ProviderFailure<S>,
    ) -> ResumeFailure<S> {
        ResumeFailure::from_kind(ResumeFailureKind::InFlight {
            pending: self,
            error,
        })
    }

    pub fn id(&self) -> &ResourceId {
        &self.id
    }

    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        match &self.pending {
            PendingRecord::Fresh(pending) => pending.identifier(),
            PendingRecord::Persisted(envelope) => envelope.identifier(),
        }
    }

    pub fn token(&self) -> &PendingToken {
        match &self.pending {
            PendingRecord::Fresh(pending) => pending.token(),
            PendingRecord::Persisted(envelope) => envelope.token(),
        }
    }

    pub fn sent(&self) -> SentRecord {
        match &self.pending {
            PendingRecord::Fresh(pending) => SentRecord::Request(pending.sent().clone()),
            PendingRecord::Persisted(envelope) => SentRecord::Persisted(envelope.clone()),
        }
    }

    pub fn started_at(&self) -> &Timestamp {
        &self.started_at
    }

    pub fn persistence(&self) -> InFlightPersistence<'_> {
        match &self.pending {
            PendingRecord::Fresh(pending) => InFlightPersistence::Fresh(pending),
            PendingRecord::Persisted(_envelope) => InFlightPersistence::Persisted,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn into_fresh(self) -> Result<PendingCreate, Box<Self>> {
        match self.pending {
            PendingRecord::Fresh(pending) => Ok(pending.into_authority()),
            PendingRecord::Persisted(envelope) => Err(Box::new(Self {
                id: self.id,
                pending: PendingRecord::Persisted(envelope),
                started_at: self.started_at,
            })),
        }
    }

    fn repend_preserving_identifier(self, token: PendingToken) -> Self {
        let pending = match self.pending {
            PendingRecord::Fresh(FreshPendingCreate {
                id,
                token: _old_token,
                progress,
                sent,
                poll_after_secs,
                started_at,
            }) => PendingRecord::Fresh(FreshPendingCreate {
                id,
                token,
                progress,
                sent,
                poll_after_secs,
                started_at,
            }),
            PendingRecord::Persisted(envelope) => {
                let identifier = envelope.identifier().cloned();
                PendingRecord::Persisted(envelope.with_pending_record(token, identifier))
            }
        };
        Self {
            id: self.id,
            pending,
            started_at: self.started_at,
        }
    }

    fn record_identifier(self, identifier: AppliedIdentifier) -> Self {
        let pending = match self.pending {
            PendingRecord::Fresh(FreshPendingCreate {
                id,
                token,
                progress,
                sent,
                poll_after_secs,
                started_at,
            }) => PendingRecord::Fresh(FreshPendingCreate {
                id,
                token,
                progress: match progress {
                    CreateProgress::Operating { .. } => CreateProgress::Operating {
                        identifier: Some(identifier),
                    },
                    CreateProgress::ReadingBack { .. } => {
                        CreateProgress::ReadingBack { identifier }
                    }
                },
                sent,
                poll_after_secs,
                started_at,
            }),
            PendingRecord::Persisted(envelope) => {
                let token = envelope.token().clone();
                PendingRecord::Persisted(envelope.with_pending_record(token, Some(identifier)))
            }
        };
        Self {
            id: self.id,
            pending,
            started_at: self.started_at,
        }
    }

    /// Re-key a continuation only inside core's checked move seams.
    ///
    /// `StateBlockAddress` has no provider-instance field. The issuing route
    /// is therefore preserved by construction while the persisted address is
    /// changed.
    ///
    /// ```compile_fail
    /// use carina_core::provider::InFlightCreate;
    /// use carina_core::parser::StateBlockAddress;
    /// fn bypass_route_check(pending: InFlightCreate, to: StateBlockAddress) {
    ///     let _ = pending.rebind_for_move(to);
    /// }
    /// ```
    pub(crate) fn rebind_for_move(mut self, to: StateBlockAddress) -> Self {
        let to = ResourceId::with_provider_identity(
            to.provider,
            to.resource_type,
            to.name,
            self.id.provider_instance.clone(),
        );
        self.id = to.clone();
        if let PendingRecord::Fresh(pending) = &mut self.pending {
            pending.id = to;
        }
        self
    }

    pub(crate) fn validate_fresh_sent(
        mut self,
        request_attributes: &std::collections::BTreeSet<String>,
    ) -> (Self, bool) {
        let valid = match &mut self.pending {
            PendingRecord::Fresh(pending) => pending.sent.clip_to(request_attributes),
            PendingRecord::Persisted(_envelope) => true,
        };
        (self, valid)
    }
}

/// Proof that no outgoing request was dispatched during one guest call.
///
/// The field is private so only the host-owned call recorder can mint this
/// witness.
#[derive(Debug)]
pub struct NoEgress(());

/// Proof that at least one outgoing request was dispatched during one guest
/// call.
///
/// The field is private so providers cannot upgrade an ordinary failure to an
/// outcome-unknown failure without the host's observation.
#[derive(Debug)]
pub struct EgressSent(());

impl EgressSent {
    /// Construct an egress witness in cross-crate tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn for_test() -> Self {
        Self(())
    }
}

/// Host observation for exactly one guest call.
#[derive(Debug)]
pub enum CallEgress {
    None(NoEgress),
    Sent(EgressSent),
}

/// Cloneable handle installed by the host for one guest call.
///
/// Clones retain the identity of the call that minted them. A delayed request
/// therefore cannot mark a later call's recorder.
#[derive(Clone, Debug)]
pub struct CallEgressRecorder {
    state: Arc<CallEgressState>,
}

#[derive(Debug)]
struct CallEgressState {
    dispatched: AtomicBool,
    pending: AtomicUsize,
    settled: Notify,
}

/// One outgoing request whose host policy and dispatch decision is pending.
///
/// Dropping the guard records that the request was not dispatched. The host
/// consumes it with [`PendingCallEgress::dispatched`] immediately before
/// handing an approved request to the HTTP transport.
#[derive(Debug)]
#[must_use = "pending egress is settled when dispatched, rejected, or dropped"]
pub struct PendingCallEgress {
    recorder: CallEgressRecorder,
    settled: bool,
}

impl CallEgressRecorder {
    fn new() -> Self {
        Self {
            state: Arc::new(CallEgressState {
                dispatched: AtomicBool::new(false),
                pending: AtomicUsize::new(0),
                settled: Notify::new(),
            }),
        }
    }

    /// Start tracking a request before its asynchronous destination checks.
    ///
    /// The returned guard belongs to this call's recorder. Policy rejection,
    /// DNS failure, and task cancellation leave it undispatched.
    pub fn begin_request(&self) -> PendingCallEgress {
        self.state.pending.fetch_add(1, Ordering::AcqRel);
        PendingCallEgress {
            recorder: self.clone(),
            settled: false,
        }
    }

    fn mark_dispatched(&self) {
        self.state.dispatched.store(true, Ordering::Release);
        self.state.settled.notify_one();
    }

    async fn settled_snapshot(&self) -> CallEgress {
        loop {
            let settled = self.state.settled.notified();
            let pending = self.state.pending.load(Ordering::Acquire);
            if self.state.dispatched.load(Ordering::Acquire) {
                return CallEgress::Sent(EgressSent(()));
            }
            if pending == 0 {
                return CallEgress::None(NoEgress(()));
            }
            settled.await;
        }
    }

    fn may_have_dispatched(&self) -> bool {
        self.state.pending.load(Ordering::Acquire) > 0
            || self.state.dispatched.load(Ordering::Acquire)
    }
}

impl PendingCallEgress {
    /// Record that this host-approved request is being dispatched.
    pub fn dispatched(mut self) {
        self.recorder.mark_dispatched();
        self.settle();
    }

    fn settle(&mut self) {
        if self.settled {
            return;
        }
        self.settled = true;
        let previous = self.recorder.state.pending.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "a pending egress guard settles exactly once");
        if previous == 1 {
            self.recorder.state.settled.notify_one();
        }
    }
}

impl Drop for PendingCallEgress {
    fn drop(&mut self) {
        self.settle();
    }
}

/// Address and start time retained when a create may have reached the cloud
/// but returned no identifier or continuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownCreateOutcome {
    id: ResourceId,
    started_at: Timestamp,
}

impl Serialize for UnknownCreateOutcome {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Encoded<'a> {
            started_at: &'a Timestamp,
        }

        Encoded {
            started_at: &self.started_at,
        }
        .serialize(serializer)
    }
}

impl UnknownCreateOutcome {
    fn new(id: ResourceId, started_at: Timestamp) -> Self {
        Self { id, started_at }
    }

    pub fn id(&self) -> &ResourceId {
        &self.id
    }

    pub fn started_at(&self) -> &Timestamp {
        &self.started_at
    }

    pub(crate) fn rebind_for_move(&mut self, id: ResourceId) {
        self.id = id;
    }
}

#[derive(Debug)]
struct CreateCallCancellationReporter {
    sender: mpsc::Sender<CancelledCreateReport>,
}

/// The only two states a dropped started guest create can report.
#[derive(Debug)]
pub(crate) enum CancelledCreateReport {
    NotCreated,
    OutcomeUnknown(UnknownCreateOutcome),
}

/// Executor-owned receiver for a classified create failure emitted when a
/// started guest call is dropped during cleanup.
#[derive(Debug)]
#[doc(hidden)]
pub struct CreateCallCancellationReceiver {
    receiver: mpsc::Receiver<CancelledCreateReport>,
}

impl CreateCallCancellationReceiver {
    pub(crate) fn drain(&self) -> Vec<CancelledCreateReport> {
        self.receiver.try_iter().collect()
    }
}

/// A guest create call that has started and therefore must be finished or
/// reported as cancelled.
#[derive(Debug)]
pub struct StartedGuestCreateCall {
    inner: Option<GuestCreateCallInner>,
}

#[derive(Debug)]
struct GuestCreateCallInner {
    attempt: CreateAttempt,
    recorder: CallEgressRecorder,
}

impl StartedGuestCreateCall {
    pub fn egress_recorder(&self) -> CallEgressRecorder {
        self.inner
            .as_ref()
            .expect("a started guest create call owns its inner state")
            .recorder
            .clone()
    }

    pub fn finish(mut self) -> FinishedGuestCreateCall {
        let inner = self
            .inner
            .take()
            .expect("a started guest create call owns its inner state");
        FinishedGuestCreateCall { inner: Some(inner) }
    }
}

impl Drop for StartedGuestCreateCall {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        report_cancelled_guest_create(inner.attempt, inner.recorder.may_have_dispatched());
    }
}

/// A completed guest call whose returned boundary value has not yet been
/// decoded.
#[derive(Debug)]
pub struct FinishedGuestCreateCall {
    inner: Option<GuestCreateCallInner>,
}

impl FinishedGuestCreateCall {
    pub async fn failed(mut self, error: ProviderError) -> GuestCallFailure {
        let egress = self
            .inner
            .as_ref()
            .expect("a finished guest create call owns its inner state")
            .recorder
            .settled_snapshot()
            .await;
        let GuestCreateCallInner {
            attempt,
            recorder: _recorder,
        } = self
            .inner
            .take()
            .expect("a finished guest create call owns its inner state");
        GuestCallFailure {
            inner: Some(GuestCallFailureInner {
                attempt,
                egress,
                error,
            }),
        }
    }

    /// Decode the complete guest result before releasing the provider-facing
    /// attempt. A decode error is classified against this call's egress
    /// record inside this method.
    pub async fn decode<T, U>(
        mut self,
        value: T,
        decode: impl FnOnce(T) -> Result<U, ProviderError>,
    ) -> Result<DecodedGuestCreateCall<U>, CreateFailure> {
        match decode(value) {
            Ok(value) => {
                let inner = self
                    .inner
                    .take()
                    .expect("a finished guest create call owns its inner state");
                Ok(DecodedGuestCreateCall {
                    attempt: inner.attempt,
                    value,
                })
            }
            Err(error) => Err(self.failed(error).await.classify()),
        }
    }
}

impl Drop for FinishedGuestCreateCall {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        report_cancelled_guest_create(inner.attempt, inner.recorder.may_have_dispatched());
    }
}

/// Fully decoded guest result paired with the only attempt that may interpret
/// the provider-declared step or verdict.
#[derive(Debug)]
pub struct DecodedGuestCreateCall<T> {
    attempt: CreateAttempt,
    value: T,
}

impl<T> DecodedGuestCreateCall<T> {
    pub fn into_parts(self) -> (CreateAttempt, T) {
        (self.attempt, self.value)
    }
}

/// A guest-call failure that cannot expose its bare provider error. It must be
/// consumed through [`GuestCallFailure::classify`].
#[derive(Debug)]
pub struct GuestCallFailure {
    inner: Option<GuestCallFailureInner>,
}

#[derive(Debug)]
struct GuestCallFailureInner {
    attempt: CreateAttempt,
    egress: CallEgress,
    error: ProviderError,
}

impl GuestCallFailure {
    pub fn classify(mut self) -> CreateFailure {
        let GuestCallFailureInner {
            attempt,
            egress,
            error,
        } = self
            .inner
            .take()
            .expect("a guest call failure owns its inner state");
        match egress {
            CallEgress::None(_no_egress) => attempt.not_created(error),
            CallEgress::Sent(sent) => attempt.outcome_unknown(sent, error),
        }
    }
}

fn report_cancelled_guest_create(attempt: CreateAttempt, may_have_dispatched: bool) {
    let CreateAttempt {
        id,
        started_at,
        cancellation,
    } = attempt;
    let report = if may_have_dispatched {
        CancelledCreateReport::OutcomeUnknown(UnknownCreateOutcome::new(id, started_at))
    } else {
        CancelledCreateReport::NotCreated
    };
    let _send_result = cancellation.sender.send(report);
}

/// Inspectable classification carried by an opaque [`CreateFailure`].
#[derive(Debug)]
pub enum CreateFailureKind<S: TextState = Unbound> {
    /// No cloud resource remains. A `not-created` verdict means nothing was
    /// created: no request left the provider, or the cloud rejected it without
    /// creating anything.
    NoResource(ProviderFailure<S>),
    /// A request may have reached the cloud, with no identifier or continuation available.
    OutcomeUnknown {
        unknown: UnknownCreateOutcome,
        error: ProviderFailure<S>,
    },
    /// The provider reported an identifier; resource existence may be unobserved.
    IdentifierReported {
        identified: IdentifiedCreate<S>,
        error: ProviderFailure<S>,
    },
    /// A resumable continuation is available, with an optional cloud identifier.
    InFlight {
        pending: InFlightCreate,
        error: ProviderFailure<S>,
    },
}

/// Failure from an initial create, classified by what can safely be retained.
///
/// The wrapper is intentionally opaque: provider implementations can obtain
/// a `NoResource` failure only by consuming a core-minted create witness.
/// Constructing the public inspection enum is not enough to return a failure.
///
/// ```compile_fail
/// use carina_core::provider::{CreateFailure, CreateFailureKind, ProviderError};
///
/// let kind = CreateFailureKind::NoResource(ProviderError::internal("lost"));
/// let _failure = CreateFailure(kind);
/// ```
#[derive(Debug)]
pub struct CreateFailure<S: TextState = Unbound>(Box<CreateFailureKind<S>>);

impl<S: TextState> std::fmt::Display for CreateFailure<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind() {
            CreateFailureKind::NoResource(_error) => {
                f.write_str("create failed and left no resource")
            }
            CreateFailureKind::OutcomeUnknown {
                unknown: _unknown,
                error: _error,
            } => f.write_str("create outcome is unknown after a request left the provider"),
            CreateFailureKind::IdentifierReported {
                identified: _identified,
                error: _error,
            } => f.write_str("the provider reported an identifier but the create did not complete"),
            CreateFailureKind::InFlight {
                pending: _pending,
                error: _error,
            } => f.write_str("create is still in progress"),
        }
    }
}

impl std::error::Error for CreateFailure<Bound> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.kind() {
            CreateFailureKind::NoResource(error) => Some(error),
            CreateFailureKind::OutcomeUnknown {
                unknown: _unknown,
                error,
            } => Some(error),
            CreateFailureKind::IdentifierReported {
                identified: _identified,
                error,
            } => Some(error),
            CreateFailureKind::InFlight {
                pending: _pending,
                error,
            } => Some(error),
        }
    }
}

impl<S: TextState> CreateFailure<S> {
    fn from_kind(kind: CreateFailureKind<S>) -> Self {
        Self(Box::new(kind))
    }

    pub fn kind(&self) -> &CreateFailureKind<S> {
        self.0.as_ref()
    }

    pub fn into_kind(self) -> CreateFailureKind<S> {
        *self.0
    }

    pub fn provider_error(&self) -> &ProviderFailure<S> {
        match self.kind() {
            CreateFailureKind::NoResource(error) => error,
            CreateFailureKind::OutcomeUnknown {
                unknown: _unknown,
                error,
            } => error,
            CreateFailureKind::IdentifierReported {
                identified: _identified,
                error,
            } => error,
            CreateFailureKind::InFlight {
                pending: _pending,
                error,
            } => error,
        }
    }

    pub(crate) fn identifier_reported(
        identified: IdentifiedCreate<S>,
        error: ProviderFailure<S>,
    ) -> Self {
        Self::from_kind(CreateFailureKind::IdentifierReported { identified, error })
    }

    pub(crate) fn in_flight(pending: InFlightCreate, error: ProviderFailure<S>) -> Self {
        Self::from_kind(CreateFailureKind::InFlight { pending, error })
    }

    pub(crate) fn not_created_at_dispatch(error: ProviderFailure<S>) -> Self {
        Self::from_kind(CreateFailureKind::NoResource(error))
    }

    pub(crate) fn outcome_unknown_at_dispatch(
        unknown: UnknownCreateOutcome,
        error: ProviderFailure<S>,
    ) -> Self {
        Self::from_kind(CreateFailureKind::OutcomeUnknown { unknown, error })
    }
}

/// Successful result from resuming a persisted create.
///
/// A resent success must be finished through the resend attempt minted by the
/// matching resume witness.
///
/// ```compile_fail
/// use carina_core::provider::{CreateOutcome, ResumeOutcome};
///
/// fn forge_resent(outcome: CreateOutcome) -> ResumeOutcome {
///     ResumeOutcome::Resent(Box::new(outcome))
/// }
/// ```
#[derive(Debug)]
pub struct ResumeOutcome<S: TextState = Unbound> {
    kind: ResumeOutcomeKind<S>,
}

/// A resumed create that completed with an applied state.
///
/// The wrapped [`IdentifiedCreate`] is private so callers cannot put an
/// unobserved identifier-reported failure in the successful `Resumed` arm.
///
/// ```compile_fail
/// use carina_core::provider::{CompletedResume, IdentifiedCreate};
///
/// fn forge(identified: IdentifiedCreate) -> CompletedResume {
///     CompletedResume(identified)
/// }
/// ```
#[derive(Debug)]
pub struct CompletedResume<S: TextState = Unbound>(IdentifiedCreate<S>);

impl<S: TextState> CompletedResume<S> {
    fn new(pending: InFlightCreate, identifier: AppliedIdentifier, state: AppliedState) -> Self {
        let (_state_id, _observed_identifier, attributes) = state.into_parts();
        let state =
            AppliedState::from_snapshot_parts(pending.id().clone(), identifier.clone(), attributes);
        Self(identified_from_resume(
            pending,
            identifier,
            CreateObservation::Observed(state),
        ))
    }

    fn id(&self) -> &ResourceId {
        self.0.id()
    }

    pub fn identifier(&self) -> &AppliedIdentifier {
        self.0.identifier()
    }

    pub(crate) fn with_resolved_desired_attributes(
        self,
        attributes: SecretPreservingAttributes,
    ) -> Self {
        Self(self.0.with_resolved_desired_attributes(attributes))
    }

    pub(crate) fn into_identified_create(self) -> IdentifiedCreate<S> {
        self.0
    }
}

impl CompletedResume<Bound> {
    pub fn observed_state(&self) -> SnapshotState {
        self.0.observed_state()
    }
}

/// A resumed create whose resource is known but whose read-back is partial.
///
/// The completed observation retains the persisted continuation's sent-value
/// authority; unlike a resent create, this type never invents a new `Sent`.
#[derive(Debug)]
pub struct PartialResume<S: TextState = Unbound> {
    completed: CompletedResume<S>,
    diagnostic: PartialReadDiagnostic<S>,
    read_back_end: ReadBackEnd,
}

impl<S: TextState> PartialResume<S> {
    fn new(
        pending: InFlightCreate,
        identifier: AppliedIdentifier,
        attributes: HashMap<String, Value>,
        diagnostic: PartialReadDiagnostic<S>,
    ) -> Self {
        let state =
            AppliedState::from_identifier(pending.id().clone(), identifier.clone(), attributes);
        Self {
            completed: CompletedResume::new(pending, identifier, state),
            diagnostic,
            read_back_end: ReadBackEnd::ProviderReported,
        }
    }

    fn id(&self) -> &ResourceId {
        self.completed.id()
    }

    pub fn identifier(&self) -> &AppliedIdentifier {
        self.completed.identifier()
    }

    pub fn diagnostic(&self) -> &PartialReadDiagnostic<S> {
        &self.diagnostic
    }

    pub(crate) fn with_resolved_desired_attributes(
        self,
        attributes: SecretPreservingAttributes,
    ) -> Self {
        Self {
            completed: self.completed.with_resolved_desired_attributes(attributes),
            diagnostic: self.diagnostic,
            read_back_end: self.read_back_end,
        }
    }
}

impl PartialResume<Bound> {
    pub fn observed_state(&self) -> SnapshotState {
        self.diagnostic
            .clone()
            .into_state_for_writeback(self.completed.observed_state())
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        HostIdentifiedCreate,
        SnapshotState,
        HostPartialReadDiagnostic,
        ReadBackEnd,
    ) {
        let Self {
            completed,
            diagnostic,
            read_back_end,
        } = self;
        let created = completed
            .into_identified_create()
            .with_partial_read(diagnostic.clone());
        let state = created.observed_state();
        (created, state, diagnostic, read_back_end)
    }
}

/// Read-only classification exposed by an opaque [`ResumeOutcome`].
#[derive(Debug)]
pub enum ResumeOutcomeKind<S: TextState = Unbound> {
    Resumed(Box<CompletedResume<S>>),
    /// The persisted create completed, but its read-back remains incomplete.
    /// This is distinct from `Resent`: no new create request or `sent` value
    /// was fabricated to finish the resumed continuation.
    PartialSuccess(Box<PartialResume<S>>),
    Resent(Box<CreateOutcome<S>>),
}

impl<S: TextState> ResumeOutcome<S> {
    fn resumed(completed: CompletedResume<S>) -> Self {
        Self {
            kind: ResumeOutcomeKind::Resumed(Box::new(completed)),
        }
    }

    fn resent(outcome: CreateOutcome<S>) -> Self {
        Self {
            kind: ResumeOutcomeKind::Resent(Box::new(outcome)),
        }
    }

    fn partial(outcome: PartialResume<S>) -> Self {
        Self {
            kind: ResumeOutcomeKind::PartialSuccess(Box::new(outcome)),
        }
    }

    /// Attach host-owned polling provenance from the pending driver.
    pub fn from_host_completion(completed: CompletedOperation<Self>) -> Self {
        let (mut outcome, read_back_end) = completed.into_parts();
        match &mut outcome.kind {
            ResumeOutcomeKind::Resumed(_) => {}
            ResumeOutcomeKind::PartialSuccess(partial) => {
                partial.read_back_end = read_back_end;
            }
            ResumeOutcomeKind::Resent(resent) => {
                resent.read_back_end = read_back_end;
            }
        }
        outcome
    }

    pub fn kind(&self) -> &ResumeOutcomeKind<S> {
        &self.kind
    }

    pub fn into_kind(self) -> ResumeOutcomeKind<S> {
        self.kind
    }

    pub fn id(&self) -> &ResourceId {
        match &self.kind {
            ResumeOutcomeKind::Resumed(completed) => completed.id(),
            ResumeOutcomeKind::PartialSuccess(outcome) => outcome.id(),
            ResumeOutcomeKind::Resent(outcome) => outcome.id(),
        }
    }
}

/// Inspectable classification carried by an opaque [`ResumeFailure`].
#[derive(Debug)]
pub enum ResumeFailureKind<S: TextState = Unbound> {
    /// No cloud resource remains. A `not-created` verdict means nothing was
    /// created: no request left the provider, or the cloud rejected it without
    /// creating anything.
    NoResource(ProviderFailure<S>),
    /// The provider reported an identifier; resource existence may be unobserved.
    IdentifierReported {
        identified: IdentifiedCreate<S>,
        error: ProviderFailure<S>,
    },
    /// A resumable continuation is available, with an optional cloud identifier.
    InFlight {
        pending: InFlightCreate,
        error: ProviderFailure<S>,
    },
}

/// Failure from resuming a persisted create.
///
/// Like [`CreateFailure`], this is opaque so an ordinary provider cannot
/// synthesize a `NoResource` verdict by naming an enum variant.
///
/// ```compile_fail
/// use carina_core::provider::{ProviderError, ResumeFailure, ResumeFailureKind};
///
/// let kind = ResumeFailureKind::NoResource(ProviderError::internal("lost"));
/// let _failure = ResumeFailure(kind);
/// ```
#[derive(Debug)]
pub struct ResumeFailure<S: TextState = Unbound>(Box<ResumeFailureKind<S>>);

impl<S: TextState> std::fmt::Display for ResumeFailure<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind() {
            ResumeFailureKind::NoResource(_error) => f.write_str("resumed create left no resource"),
            ResumeFailureKind::IdentifierReported {
                identified: _identified,
                error: _error,
            } => f.write_str(
                "the provider reported an identifier but the resumed create did not complete",
            ),
            ResumeFailureKind::InFlight {
                pending: _pending,
                error: _error,
            } => f.write_str("resumed create is still in progress"),
        }
    }
}

impl std::error::Error for ResumeFailure<Bound> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.kind() {
            ResumeFailureKind::NoResource(error) => Some(error),
            ResumeFailureKind::IdentifierReported {
                identified: _identified,
                error,
            } => Some(error),
            ResumeFailureKind::InFlight {
                pending: _pending,
                error,
            } => Some(error),
        }
    }
}

impl<S: TextState> ResumeFailure<S> {
    fn from_kind(kind: ResumeFailureKind<S>) -> Self {
        Self(Box::new(kind))
    }

    pub fn kind(&self) -> &ResumeFailureKind<S> {
        self.0.as_ref()
    }

    pub fn into_kind(self) -> ResumeFailureKind<S> {
        *self.0
    }

    pub fn provider_error(&self) -> &ProviderFailure<S> {
        match self.kind() {
            ResumeFailureKind::NoResource(error) => error,
            ResumeFailureKind::IdentifierReported {
                identified: _identified,
                error,
            } => error,
            ResumeFailureKind::InFlight {
                pending: _pending,
                error,
            } => error,
        }
    }

    pub(crate) fn no_resource_from_verdict(error: ProviderFailure<S>) -> Self {
        Self::from_kind(ResumeFailureKind::NoResource(error))
    }

    pub(crate) fn identifier_reported(
        identified: IdentifiedCreate<S>,
        error: ProviderFailure<S>,
    ) -> Self {
        Self::from_kind(ResumeFailureKind::IdentifierReported { identified, error })
    }

    pub(crate) fn in_flight(pending: InFlightCreate, error: ProviderFailure<S>) -> Self {
        Self::from_kind(ResumeFailureKind::InFlight { pending, error })
    }
}

/// Provider-facing update witness, minted only when dispatch consumes the
/// exact request that passed input discharge. It cannot be copied or forged.
///
/// ```compile_fail,E0624
/// use carina_core::{provider::UpdateAttempt, replacement::UpdatePermit};
/// fn forge(permit: UpdatePermit) { let _ = UpdateAttempt::new(permit); }
/// ```
/// ```compile_fail,E0423
/// use carina_core::provider::UpdateAttempt;
/// let attempt = UpdateAttempt(());
/// ```
pub struct UpdateAttempt {
    _permit: crate::replacement::UpdatePermit,
}

impl std::fmt::Debug for UpdateAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateAttempt").finish_non_exhaustive()
    }
}

impl UpdateAttempt {
    pub(crate) fn new(permit: crate::replacement::UpdatePermit) -> Self {
        Self { _permit: permit }
    }
}

/// Provider-facing delete authority for one scheduler-released attempt.
/// It owns the permit, including any paired creates' input-discharge receipts.
/// Deletes have no desired values and therefore no input rules of their own.
///
/// ```compile_fail,E0624
/// use carina_core::{provider::DeleteAttempt, executor::DeletePermit, schema::SchemaRegistry};
/// fn forge(permit: DeletePermit, schemas: &SchemaRegistry) {
///     let _ = DeleteAttempt::new(permit, schemas);
/// }
/// ```
/// ```compile_fail,E0451
/// use carina_core::provider::DeleteAttempt;
/// let attempt = DeleteAttempt { permit: todo!(), request: todo!() };
/// ```
pub struct DeleteAttempt {
    permit: crate::executor::DeletePermit,
    request: DeleteRequest,
}

impl fmt::Debug for DeleteAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeleteAttempt")
            .field("id", self.id())
            .finish_non_exhaustive()
    }
}

impl DeleteAttempt {
    pub(crate) fn new(permit: crate::executor::DeletePermit, schemas: &SchemaRegistry) -> Self {
        let request = DeleteRequest::from_resolved(schemas.resolve_directives(
            permit.id(),
            crate::schema::SchemaKind::Resource,
            permit.directives(),
        ));
        Self { permit, request }
    }

    pub fn id(&self) -> &ResourceId {
        self.permit.id()
    }

    pub fn identifier(&self) -> &AppliedIdentifier {
        self.permit.identifier()
    }

    pub fn request(&self) -> &DeleteRequest {
        &self.request
    }
}

/// Provider-facing initial-create witness. Its constructor is core-private;
/// implementations can only consume the value supplied by the executor.
///
/// A provider cannot mint a fresh attempt after consuming the real one:
///
/// ```compile_fail
/// use carina_core::provider::{
///     AppliedIdentifier, CreateAttempt, CreateFailure, ProviderError, Sent,
/// };
/// use carina_core::resource::ResourceId;
///
/// fn lose_after_issue(
///     attempt: CreateAttempt,
///     identifier: AppliedIdentifier,
///     error: ProviderError,
/// ) -> CreateFailure {
///     let _issued = attempt.issued(identifier, Sent::AllRequestAttributes);
///     CreateAttempt::new(ResourceId::with_provider_identity("p", "t", "n", None)).0.not_created(error)
/// }
/// ```
///
/// Test utilities do not expose a witness constructor either:
///
/// ```compile_fail,E0599
/// use carina_core::{provider::CreateAttempt, resource::ResourceId};
/// let attempt = CreateAttempt::for_test(ResourceId::with_provider_identity("p", "t", "n", None));
/// ```
#[derive(Debug)]
pub struct CreateAttempt {
    id: ResourceId,
    started_at: Timestamp,
    cancellation: CreateCallCancellationReporter,
}

impl CreateAttempt {
    pub(crate) fn new(id: ResourceId) -> (Self, CreateCallCancellationReceiver) {
        let (sender, receiver) = mpsc::channel();
        (
            Self {
                id,
                started_at: Timestamp::now(),
                cancellation: CreateCallCancellationReporter { sender },
            },
            CreateCallCancellationReceiver { receiver },
        )
    }

    pub fn not_created(self, error: ProviderError) -> CreateFailure {
        CreateFailure::from_kind(CreateFailureKind::NoResource(error))
    }

    pub fn outcome_unknown(self, _sent: EgressSent, error: ProviderError) -> CreateFailure {
        CreateFailure::from_kind(CreateFailureKind::OutcomeUnknown {
            unknown: UnknownCreateOutcome::new(self.id, self.started_at),
            error,
        })
    }

    pub fn start_guest_call(self) -> StartedGuestCreateCall {
        StartedGuestCreateCall {
            inner: Some(GuestCreateCallInner {
                attempt: self,
                recorder: CallEgressRecorder::new(),
            }),
        }
    }

    pub fn issued(self, identifier: AppliedIdentifier, sent: Sent) -> IssuedCreate {
        IssuedCreate {
            id: self.id,
            identifier,
            sent,
            started_at: Some(self.started_at),
        }
    }

    pub fn pending(self, token: PendingToken, sent: Sent, poll_after_secs: u32) -> PendingCreate {
        PendingCreate::new(
            self.id,
            token,
            CreateProgress::Operating { identifier: None },
            sent,
            poll_after_secs,
            self.started_at,
        )
    }
}

/// Provider-facing issued-create witness.
#[derive(Debug)]
pub struct IssuedCreate {
    id: ResourceId,
    identifier: AppliedIdentifier,
    sent: Sent,
    started_at: Option<Timestamp>,
}

impl IssuedCreate {
    pub fn identifier(&self) -> &AppliedIdentifier {
        &self.identifier
    }

    pub fn canonicalize(self, identifier: AppliedIdentifier) -> FinishingCreate {
        FinishingCreate {
            id: self.id,
            identifier,
            sent: self.sent,
        }
    }

    pub fn pending(self, token: PendingToken, poll_after_secs: u32) -> PendingCreate {
        self.pending_in_phase(
            token,
            poll_after_secs,
            carina_provider_protocol::PendingPhase::Operation,
        )
    }

    pub fn pending_read_back(self, token: PendingToken, poll_after_secs: u32) -> PendingCreate {
        self.pending_in_phase(
            token,
            poll_after_secs,
            carina_provider_protocol::PendingPhase::ReadBack,
        )
    }

    fn pending_in_phase(
        self,
        token: PendingToken,
        poll_after_secs: u32,
        phase: carina_provider_protocol::PendingPhase,
    ) -> PendingCreate {
        PendingCreate::new(
            self.id,
            token,
            match phase {
                carina_provider_protocol::PendingPhase::Operation => CreateProgress::Operating {
                    identifier: Some(self.identifier),
                },
                carina_provider_protocol::PendingPhase::ReadBack => CreateProgress::ReadingBack {
                    identifier: self.identifier,
                },
            },
            self.sent,
            poll_after_secs,
            self.started_at.unwrap_or_else(Timestamp::now),
        )
    }

    pub fn succeeded(self, attributes: HashMap<String, Value>) -> CreateOutcome {
        CreateOutcome::provider_reported(CreateOutcomeKind::Success {
            state: AppliedState::from_identifier(self.id, self.identifier, attributes),
        })
    }

    pub fn partial(
        self,
        attributes: HashMap<String, Value>,
        diagnostic: PartialReadDiagnostic,
    ) -> CreateOutcome {
        CreateOutcome::provider_reported(CreateOutcomeKind::PartialSuccess {
            state: AppliedState::from_identifier(self.id, self.identifier, attributes),
            diagnostic,
        })
    }

    pub fn identifier_reported(
        self,
        observation: Observation,
        error: ProviderError,
    ) -> CreateFailure {
        CreateFailure::from_kind(CreateFailureKind::IdentifierReported {
            identified: IdentifiedCreate::new(
                self.id,
                self.identifier,
                observation,
                SentRecord::Request(self.sent),
            ),
            error,
        })
    }

    pub fn no_resource(self, error: ProviderError) -> CreateFailure {
        CreateFailure::from_kind(CreateFailureKind::NoResource(error))
    }
}

/// Canonicalized issued-create witness. It intentionally has no `pending`.
#[derive(Debug)]
pub struct FinishingCreate {
    id: ResourceId,
    identifier: AppliedIdentifier,
    sent: Sent,
}

impl FinishingCreate {
    pub fn succeeded(self, attributes: HashMap<String, Value>) -> CreateOutcome {
        CreateOutcome::provider_reported(CreateOutcomeKind::Success {
            state: AppliedState::from_identifier(self.id, self.identifier, attributes),
        })
    }

    pub fn partial(
        self,
        attributes: HashMap<String, Value>,
        diagnostic: PartialReadDiagnostic,
    ) -> CreateOutcome {
        CreateOutcome::provider_reported(CreateOutcomeKind::PartialSuccess {
            state: AppliedState::from_identifier(self.id, self.identifier, attributes),
            diagnostic,
        })
    }

    pub fn identifier_reported(
        self,
        observation: Observation,
        error: ProviderError,
    ) -> CreateFailure {
        CreateFailure::from_kind(CreateFailureKind::IdentifierReported {
            identified: IdentifiedCreate::new(
                self.id,
                self.identifier,
                observation,
                SentRecord::Request(self.sent),
            ),
            error,
        })
    }

    pub fn no_resource(self, error: ProviderError) -> CreateFailure {
        CreateFailure::from_kind(CreateFailureKind::NoResource(error))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot resume create for {desired}; persisted operation belongs to {persisted}", desired = DiagnosticText::new(desired.to_string()), persisted = DiagnosticText::new(persisted.to_string()))]
pub struct ResumeRoutingMismatch {
    persisted: Box<ResourceId>,
    desired: Box<ResourceId>,
}

/// A checked pairing of one persisted continuation and the desired request
/// for the same complete provider route.
///
/// The resume witness itself owns the only authority to preserve or finish
/// the persisted continuation; no second re-pend handle is handed out.
///
/// ```compile_fail
/// use carina_core::provider::{Repend, ResumeCreateRequest};
///
/// fn split_authority(request: ResumeCreateRequest) -> Repend {
///     let (_resume, _create_request, repend) = request.into_parts();
///     repend
/// }
/// ```
#[derive(Debug)]
pub struct ResumeCreateRequest {
    pending: InFlightCreate,
    request: CreateRequest,
    authorization: ResumeAuthorization,
}

#[derive(Debug)]
enum ResumeAuthorization {
    Resend(crate::replacement::CreatePermit),
    Poll(AppliedIdentifier),
}

/// Poll an already identified create. This request cannot produce an
/// identifierless resume witness or send permission.
///
/// ```compile_fail,E0599
/// use carina_core::provider::PollCreateRequest;
/// fn resend(request: PollCreateRequest) {
///     let (poll, _) = request.into_parts();
///     poll.resend();
/// }
/// ```
#[derive(Debug)]
pub struct PollCreateRequest {
    resumed: ResumedCreate,
    request: CreateRequest,
}

impl PollCreateRequest {
    pub(crate) fn new(pending: InFlightCreate, request: CreateRequest) -> Result<Self, HostError> {
        if pending.id() != &request.resource().id {
            return Err(
                HostError::internal("poll request belongs to another resource")
                    .for_resource(pending.id().clone()),
            );
        }
        let Some(identifier) = pending.identifier().cloned() else {
            return Err(
                HostError::internal("poll-only continuation has no provider identifier")
                    .for_resource(pending.id().clone()),
            );
        };
        Ok(Self {
            resumed: ResumedCreate {
                pending,
                identifier,
            },
            request,
        })
    }

    pub fn into_parts(self) -> (ResumedCreate, CreateRequest) {
        (self.resumed, self.request)
    }

    pub(crate) fn into_resume_request(self) -> ResumeCreateRequest {
        ResumeCreateRequest {
            pending: self.resumed.pending,
            request: self.request,
            authorization: ResumeAuthorization::Poll(self.resumed.identifier),
        }
    }
}

/// Resume typestate selected from the persisted continuation.
#[derive(Debug)]
pub enum Resume {
    Unissued(ResumeAttempt),
    Issued(ResumedCreate),
}

impl Resume {
    pub fn id(&self) -> &ResourceId {
        match self {
            Self::Unissued(attempt) => attempt.id(),
            Self::Issued(resumed) => resumed.id(),
        }
    }

    pub fn token(&self) -> &PendingToken {
        match self {
            Self::Unissued(attempt) => attempt.token(),
            Self::Issued(resumed) => resumed.token(),
        }
    }

    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        match self {
            Self::Unissued(_attempt) => None,
            Self::Issued(resumed) => Some(resumed.identifier()),
        }
    }

    pub fn preserve(self, error: ProviderError) -> ResumeFailure {
        match self {
            Self::Unissued(attempt) => attempt.preserve(error),
            Self::Issued(resumed) => resumed.preserve(error),
        }
    }

    pub fn not_created(self, error: ProviderError) -> ResumeFailure {
        match self {
            Self::Unissued(attempt) => attempt.not_created(error),
            Self::Issued(resumed) => resumed.not_created(error),
        }
    }

    pub fn no_resource(self, error: ProviderError) -> ResumeFailure {
        match self {
            Self::Unissued(attempt) => attempt.no_resource(error),
            Self::Issued(resumed) => resumed.no_resource(error),
        }
    }
}

/// Resume witness for a persisted create without an identifier.
///
/// ```compile_fail
/// use carina_core::provider::ResumeAttempt;
///
/// let _ = ResumeAttempt::new();
/// ```
#[derive(Debug)]
pub struct ResumeAttempt {
    pending: InFlightCreate,
    permit: crate::replacement::CreatePermit,
}

impl ResumeAttempt {
    pub fn id(&self) -> &ResourceId {
        self.pending.id()
    }

    pub fn token(&self) -> &PendingToken {
        self.pending.token()
    }

    pub fn issued(self, identifier: AppliedIdentifier) -> ResumedCreate {
        let pending = self.pending.record_identifier(identifier.clone());
        ResumedCreate {
            pending,
            identifier,
        }
    }

    pub fn repend(self, token: PendingToken) -> Resume {
        Resume::Unissued(Self {
            pending: self.pending.repend_preserving_identifier(token),
            permit: self.permit,
        })
    }

    pub fn preserve(self, error: ProviderError) -> ResumeFailure {
        self.pending.into_resume_failure(error)
    }

    pub fn not_created(self, error: ProviderError) -> ResumeFailure {
        ResumeFailure::no_resource_from_verdict(error)
    }

    pub fn no_resource(self, error: ProviderError) -> ResumeFailure {
        ResumeFailure::no_resource_from_verdict(error)
    }

    pub fn resend(self) -> ResentCreateAttempt {
        ResentCreateAttempt {
            previous: self.pending,
            _permit: self.permit,
        }
    }
}

/// Resume witness for a persisted create whose identifier is known.
///
/// Completion consumes the continuation owned by this exact witness; a
/// continuation from another request cannot be supplied.
///
/// ```compile_fail
/// use carina_core::provider::{AppliedState, Repend, ResumedCreate, ResumeOutcome};
///
/// fn mix_requests(
///     resumed: ResumedCreate,
///     state: AppliedState,
///     other: Repend,
/// ) -> ResumeOutcome {
///     resumed.completed(state, other)
/// }
/// ```
#[derive(Debug)]
pub struct ResumedCreate {
    pending: InFlightCreate,
    identifier: AppliedIdentifier,
}

impl ResumedCreate {
    pub fn id(&self) -> &ResourceId {
        self.pending.id()
    }

    pub fn token(&self) -> &PendingToken {
        self.pending.token()
    }

    pub fn identifier(&self) -> &AppliedIdentifier {
        &self.identifier
    }

    pub fn canonicalize(self, identifier: AppliedIdentifier) -> FinishingResume {
        FinishingResume {
            pending: self.pending.record_identifier(identifier.clone()),
            identifier,
        }
    }

    pub fn repend(self, token: PendingToken) -> Resume {
        Resume::Issued(Self {
            pending: self.pending.repend_preserving_identifier(token),
            identifier: self.identifier,
        })
    }

    pub fn completed(self, state: AppliedState) -> ResumeOutcome {
        ResumeOutcome::resumed(CompletedResume::new(self.pending, self.identifier, state))
    }

    pub fn partial(
        self,
        attributes: HashMap<String, Value>,
        diagnostic: PartialReadDiagnostic,
    ) -> ResumeOutcome {
        ResumeOutcome::partial(PartialResume::new(
            self.pending,
            self.identifier,
            attributes,
            diagnostic,
        ))
    }

    pub fn identifier_reported(
        self,
        observation: Observation,
        error: ProviderError,
    ) -> ResumeFailure {
        let sent = self.pending.sent();
        let identified = IdentifiedCreate::new(self.pending.id, self.identifier, observation, sent);
        ResumeFailure::identifier_reported(identified, error)
    }

    pub fn preserve(self, error: ProviderError) -> ResumeFailure {
        self.pending.into_resume_failure(error)
    }

    pub fn not_created(self, error: ProviderError) -> ResumeFailure {
        self.preserve(error)
    }

    pub fn no_resource(self, error: ProviderError) -> ResumeFailure {
        ResumeFailure::no_resource_from_verdict(error)
    }
}

/// Canonicalized resumed-create witness. It intentionally cannot re-pend.
#[derive(Debug)]
pub struct FinishingResume {
    pending: InFlightCreate,
    identifier: AppliedIdentifier,
}

impl FinishingResume {
    pub fn completed(self, state: AppliedState) -> ResumeOutcome {
        ResumeOutcome::resumed(CompletedResume::new(self.pending, self.identifier, state))
    }

    pub fn partial(
        self,
        attributes: HashMap<String, Value>,
        diagnostic: PartialReadDiagnostic,
    ) -> ResumeOutcome {
        ResumeOutcome::partial(PartialResume::new(
            self.pending,
            self.identifier,
            attributes,
            diagnostic,
        ))
    }

    pub fn identifier_reported(
        self,
        observation: Observation,
        error: ProviderError,
    ) -> ResumeFailure {
        let sent = self.pending.sent();
        ResumeFailure::identifier_reported(
            IdentifiedCreate::new(self.pending.id, self.identifier, observation, sent),
            error,
        )
    }

    pub fn no_resource(self, error: ProviderError) -> ResumeFailure {
        ResumeFailure::no_resource_from_verdict(error)
    }
}

fn identified_from_resume<S: TextState>(
    pending: InFlightCreate,
    identifier: AppliedIdentifier,
    observation: CreateObservation<S>,
) -> IdentifiedCreate<S> {
    let sent = pending.sent();
    IdentifiedCreate::from_create_observation(pending.id, identifier, observation, sent)
}

impl ResumeCreateRequest {
    /// A send-capable continuation needs the same admission permission as a
    /// fresh create; a persisted token alone grants no resend authority.
    ///
    /// ```compile_fail,E0308
    /// use carina_core::provider::{CreateRequest, InFlightCreate, Resume, ResumeCreateRequest};
    /// fn bypass(pending: InFlightCreate, request: CreateRequest) {
    ///     let (resume, _) = ResumeCreateRequest::new(pending, request).unwrap().into_parts();
    ///     if let Resume::Unissued(attempt) = resume {
    ///         let _ = attempt.resend();
    ///     }
    /// }
    /// ```
    pub fn new(
        pending: InFlightCreate,
        permit: crate::replacement::CreatePermit,
    ) -> Result<Self, ResumeRoutingMismatch> {
        let request = permit.request().clone();
        let desired = request.resource().id.clone();
        if pending.id != desired {
            return Err(ResumeRoutingMismatch {
                persisted: Box::new(pending.id),
                desired: Box::new(desired),
            });
        }
        Ok(Self {
            pending,
            request,
            authorization: ResumeAuthorization::Resend(permit),
        })
    }

    /// Construct a checked resume request in cross-crate tests.
    #[cfg(feature = "test-utils")]
    #[doc(hidden)]
    pub fn for_test(
        pending: InFlightCreate,
        request: CreateRequest,
    ) -> Result<Self, ResumeRoutingMismatch> {
        let permit = crate::replacement::create_permit_for_test(request);
        Self::new(pending, permit)
    }

    pub fn id(&self) -> &ResourceId {
        self.pending.id()
    }

    pub(crate) fn request(&self) -> &CreateRequest {
        &self.request
    }

    pub(crate) fn pending_for_dispatch(&self) -> InFlightCreate {
        self.pending.clone()
    }

    pub fn into_parts(self) -> (Resume, CreateRequest) {
        let resume = match self.authorization {
            ResumeAuthorization::Poll(identifier) => Resume::Issued(ResumedCreate {
                pending: self.pending,
                identifier,
            }),
            ResumeAuthorization::Resend(permit) => match self.pending.identifier().cloned() {
                Some(identifier) => Resume::Issued(ResumedCreate {
                    pending: self.pending,
                    identifier,
                }),
                None => Resume::Unissued(ResumeAttempt {
                    pending: self.pending,
                    permit,
                }),
            },
        };
        (resume, self.request)
    }

    pub(crate) fn into_in_flight(self) -> InFlightCreate {
        self.pending
    }
}

/// Linear attempt for a create explicitly re-sent by an unissued resume.
#[derive(Debug)]
pub struct ResentCreateAttempt {
    previous: InFlightCreate,
    _permit: crate::replacement::CreatePermit,
}

impl ResentCreateAttempt {
    pub fn id(&self) -> &ResourceId {
        self.previous.id()
    }

    pub fn preserve(self, error: ProviderError) -> ResumeFailure {
        self.previous.into_resume_failure(error)
    }

    pub fn not_created(self, error: ProviderError) -> ResumeFailure {
        ResumeFailure::no_resource_from_verdict(error)
    }

    pub fn issued(self, identifier: AppliedIdentifier, sent: Sent) -> ResentIssuedCreate {
        ResentIssuedCreate(IssuedCreate {
            id: self.previous.id().clone(),
            identifier,
            sent,
            started_at: Some(Timestamp::now()),
        })
    }

    pub fn pending(
        self,
        token: PendingToken,
        sent: Sent,
        poll_after_secs: u32,
    ) -> ResentPendingCreate {
        ResentPendingCreate(PendingCreate::new(
            self.previous.id().clone(),
            token,
            CreateProgress::Operating { identifier: None },
            sent,
            poll_after_secs,
            Timestamp::now(),
        ))
    }
}

#[derive(Debug)]
pub struct ResentIssuedCreate(IssuedCreate);

impl ResentIssuedCreate {
    pub fn identifier(&self) -> &AppliedIdentifier {
        self.0.identifier()
    }

    pub fn canonicalize(self, identifier: AppliedIdentifier) -> ResentFinishingCreate {
        ResentFinishingCreate(self.0.canonicalize(identifier))
    }

    pub fn pending(self, token: PendingToken, poll_after_secs: u32) -> ResentPendingCreate {
        ResentPendingCreate(self.0.pending(token, poll_after_secs))
    }

    pub fn pending_read_back(
        self,
        token: PendingToken,
        poll_after_secs: u32,
    ) -> ResentPendingCreate {
        ResentPendingCreate(self.0.pending_read_back(token, poll_after_secs))
    }

    pub fn succeeded(self, attributes: HashMap<String, Value>) -> ResumeOutcome {
        ResumeOutcome::resent(self.0.succeeded(attributes))
    }

    pub fn partial(
        self,
        attributes: HashMap<String, Value>,
        diagnostic: PartialReadDiagnostic,
    ) -> ResumeOutcome {
        ResumeOutcome::resent(self.0.partial(attributes, diagnostic))
    }

    pub fn identifier_reported(
        self,
        observation: Observation,
        error: ProviderError,
    ) -> ResumeFailure {
        let IssuedCreate {
            id,
            identifier,
            sent,
            started_at: _started_at,
        } = self.0;
        ResumeFailure::identifier_reported(
            IdentifiedCreate::new(id, identifier, observation, SentRecord::Request(sent)),
            error,
        )
    }

    pub fn no_resource(self, error: ProviderError) -> ResumeFailure {
        ResumeFailure::no_resource_from_verdict(error)
    }
}

#[derive(Debug)]
pub struct ResentFinishingCreate(FinishingCreate);

impl ResentFinishingCreate {
    pub fn succeeded(self, attributes: HashMap<String, Value>) -> ResumeOutcome {
        ResumeOutcome::resent(self.0.succeeded(attributes))
    }

    pub fn partial(
        self,
        attributes: HashMap<String, Value>,
        diagnostic: PartialReadDiagnostic,
    ) -> ResumeOutcome {
        ResumeOutcome::resent(self.0.partial(attributes, diagnostic))
    }

    pub fn identifier_reported(
        self,
        observation: Observation,
        error: ProviderError,
    ) -> ResumeFailure {
        let FinishingCreate {
            id,
            identifier,
            sent,
        } = self.0;
        ResumeFailure::identifier_reported(
            IdentifiedCreate::new(id, identifier, observation, SentRecord::Request(sent)),
            error,
        )
    }

    pub fn no_resource(self, error: ProviderError) -> ResumeFailure {
        ResumeFailure::no_resource_from_verdict(error)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ResentPendingCreate(PendingCreate);

impl ResentPendingCreate {
    pub fn id(&self) -> &ResourceId {
        &self.0.id
    }

    pub fn token(&self) -> &PendingToken {
        self.0.token()
    }

    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        self.0.identifier()
    }

    pub fn sent(&self) -> &Sent {
        self.0.sent()
    }

    pub fn poll_after_secs(&self) -> u32 {
        self.0.poll_after_secs()
    }

    pub fn progress(&self) -> &CreateProgress {
        self.0.progress()
    }

    pub fn phase(&self) -> carina_provider_protocol::PendingPhase {
        self.0.phase()
    }

    pub fn into_poll(self) -> ResentPoll {
        match self.0.into_poll() {
            Poll::Unissued(attempt) => ResentPoll::Unissued(ResentPollAttempt(attempt)),
            Poll::Issued(issued) => ResentPoll::Issued(ResentIssuedCreate(issued)),
        }
    }

    pub fn preserve(self, error: ProviderError) -> ResumeFailure {
        InFlightCreate::from_pending(self.0).into_resume_failure(error)
    }

    pub fn not_created(self, error: ProviderError) -> ResumeFailure {
        if self.0.identifier().is_some() {
            InFlightCreate::from_pending(self.0).into_resume_failure(error)
        } else {
            ResumeFailure::no_resource_from_verdict(error)
        }
    }
}

#[derive(Debug)]
pub enum ResentPoll {
    Unissued(ResentPollAttempt),
    Issued(ResentIssuedCreate),
}

#[derive(Debug)]
pub struct ResentPollAttempt(PollAttempt);

impl ResentPollAttempt {
    pub fn not_created(self, error: ProviderError) -> ResumeFailure {
        let _attempt = self.0;
        ResumeFailure::no_resource_from_verdict(error)
    }

    pub fn issued(self, identifier: AppliedIdentifier) -> ResentIssuedCreate {
        ResentIssuedCreate(self.0.issued(identifier))
    }

    pub fn pending(self, token: PendingToken, poll_after_secs: u32) -> ResentPendingCreate {
        ResentPendingCreate(self.0.pending(token, poll_after_secs))
    }
}

impl<S: TextState> CreateOutcome<S> {
    fn provider_reported(kind: CreateOutcomeKind<S>) -> Self {
        Self {
            kind,
            read_back_end: ReadBackEnd::ProviderReported,
        }
    }

    /// Attach host-owned polling provenance from the pending driver.
    pub fn from_host_completion(completed: CompletedOperation<Self>) -> Self {
        let (mut outcome, read_back_end) = completed.into_parts();
        outcome.read_back_end = read_back_end;
        outcome
    }

    #[cfg(test)]
    pub(crate) fn fixture_success(state: AppliedState) -> Self {
        Self::provider_reported(CreateOutcomeKind::Success { state })
    }

    #[cfg(test)]
    pub(crate) fn partial_success(
        state: AppliedState,
        reason: S::PartialReadReason,
        missing_attributes: NonEmptyMissingAttributes,
    ) -> Self {
        let diagnostic = PartialReadDiagnostic::new(reason, missing_attributes);
        Self::provider_reported(CreateOutcomeKind::PartialSuccess { state, diagnostic })
    }

    pub fn kind(&self) -> &CreateOutcomeKind<S> {
        &self.kind
    }

    pub fn into_kind(self) -> CreateOutcomeKind<S> {
        self.kind
    }

    pub fn read_back_end(&self) -> ReadBackEnd {
        self.read_back_end
    }

    pub fn id(&self) -> &ResourceId {
        match &self.kind {
            CreateOutcomeKind::Success { state }
            | CreateOutcomeKind::PartialSuccess {
                state,
                diagnostic: _,
            } => state.id(),
        }
    }

    /// Provider-assigned identifier carried by every successful create.
    pub fn identifier(&self) -> &AppliedIdentifier {
        match &self.kind {
            CreateOutcomeKind::Success { state }
            | CreateOutcomeKind::PartialSuccess {
                state,
                diagnostic: _,
            } => state.identifier(),
        }
    }

    pub fn diagnostic(&self) -> Option<&PartialReadDiagnostic<S>> {
        match &self.kind {
            CreateOutcomeKind::Success { .. } => None,
            CreateOutcomeKind::PartialSuccess { diagnostic, .. } => Some(diagnostic),
        }
    }
}

impl CreateOutcome<Bound> {
    /// Consume the outcome and produce a State ready for writeback.
    pub(crate) fn into_state_for_writeback(self) -> SnapshotState {
        match self.kind {
            CreateOutcomeKind::Success { state } => state.into_state_for_writeback(),
            CreateOutcomeKind::PartialSuccess { state, diagnostic } => {
                diagnostic.into_state_for_writeback(state.into_state_for_writeback())
            }
        }
    }
}

impl<S: TextState> UpdateOutcome<S> {
    pub fn success(state: AppliedState) -> Self {
        Self::provider_reported(UpdateOutcomeKind::Success { state })
    }

    /// Report an incomplete update read-back without stringifying its cause.
    /// Provider reasons must remain opaque until they cross the boundary.
    ///
    /// ```compile_fail,E0308
    /// use carina_core::provider::{AppliedState, UpdateOutcome};
    /// use carina_core::provider_text::Unbound;
    /// fn raw_reason(state: AppliedState) {
    ///     let _ = UpdateOutcome::<Unbound>::partial_success(
    ///         state, String::from("provider reason"), vec!["name".into()],
    ///     );
    /// }
    /// ```
    pub fn partial_success(
        state: AppliedState,
        reason: S::PartialReadReason,
        missing_attributes: NonEmptyMissingAttributes,
    ) -> Self {
        let diagnostic = PartialReadDiagnostic::new(reason, missing_attributes);
        Self::provider_reported(UpdateOutcomeKind::PartialSuccess { state, diagnostic })
    }

    fn provider_reported(kind: UpdateOutcomeKind<S>) -> Self {
        Self {
            kind,
            read_back_end: ReadBackEnd::ProviderReported,
        }
    }

    /// Attach host-owned polling provenance from the pending driver.
    pub fn from_host_completion(completed: CompletedOperation<Self>) -> Self {
        let (mut outcome, read_back_end) = completed.into_parts();
        outcome.read_back_end = read_back_end;
        outcome
    }

    pub fn kind(&self) -> &UpdateOutcomeKind<S> {
        &self.kind
    }

    pub fn into_kind(self) -> UpdateOutcomeKind<S> {
        self.kind
    }

    pub fn read_back_end(&self) -> ReadBackEnd {
        self.read_back_end
    }

    pub fn diagnostic(&self) -> Option<&PartialReadDiagnostic<S>> {
        match &self.kind {
            UpdateOutcomeKind::Success { .. } => None,
            UpdateOutcomeKind::PartialSuccess { diagnostic, .. } => Some(diagnostic),
        }
    }
}

impl UpdateOutcome<Bound> {
    /// Consume the outcome and produce a State ready for writeback.
    pub fn into_state_for_writeback(self) -> SnapshotState {
        match self.kind {
            UpdateOutcomeKind::Success { state } => state.into_state_for_writeback(),
            UpdateOutcomeKind::PartialSuccess { state, diagnostic } => {
                diagnostic.into_state_for_writeback(state.into_state_for_writeback())
            }
        }
    }
}

impl<S: TextState> PartialReadDiagnostic<S> {
    pub fn new(
        reason: S::PartialReadReason,
        missing_attributes: NonEmptyMissingAttributes,
    ) -> Self {
        Self {
            reason: S::operations().partial_read_reason(reason),
            missing_attributes: missing_attributes
                .0
                .into_iter()
                .map(ProviderKey::from)
                .collect(),
        }
    }
    pub fn diagnostic(&self) -> &Diagnostic<S> {
        &self.reason
    }
    pub fn missing_attributes(&self) -> &[ProviderKey] {
        &self.missing_attributes
    }
}

impl<S: TextState> PartialReadDiagnostic<S> {
    fn to_wire_inner(&self) -> protocol_types::PartialReadDiagnostic {
        self.reason.text().partial_read_wire(
            self.missing_attributes
                .iter()
                .map(|name| name.as_str().to_owned())
                .collect(),
        )
    }
}
impl PartialReadDiagnostic<Unbound> {
    #[cfg(all(test, not(target_arch = "wasm32")))]
    pub fn to_wire(&self) -> protocol_types::PartialReadDiagnostic {
        self.to_wire_inner()
    }
    /// Flatten this reason for transport. Typed causes and retry policy do not
    /// survive this projection; retain the diagnostic for further composition.
    #[cfg(target_arch = "wasm32")]
    pub fn to_wire(&self) -> protocol_types::PartialReadDiagnostic {
        self.to_wire_inner()
    }
}
impl PartialReadDiagnostic<Bound> {
    pub fn to_wire(&self) -> protocol_types::PartialReadDiagnostic {
        self.to_wire_inner()
    }
    /// Bounded diagnostic projection; the original reason also feeds identity data.
    pub fn reason(&self) -> DiagnosticText {
        DiagnosticText::new(self.reason.redacted_str())
    }

    /// Original reason for persistence, transport and identity material.
    pub fn raw_reason(&self) -> String {
        self.reason.redacted_str()
    }

    pub fn into_state_for_writeback(self, mut state: SnapshotState) -> SnapshotState {
        state.partial_read = PartialReadMarker::new(
            PendingReRead::incomplete(self.reason.redacted_str()),
            self.missing_attributes
                .into_iter()
                .map(ProviderKey::into_string)
                .collect(),
        );
        state
    }
}

impl ProviderFailure<Unbound> {
    /// Decode a native boundary fixture. Production wire decoding lives in the
    /// host's private WIT conversion module. Neither test-feature unification
    /// nor a guest target makes this constructor public.
    #[cfg(any(test, all(feature = "test-utils", not(target_arch = "wasm32"))))]
    pub(crate) fn from_wire(error: WireProviderError) -> Self {
        let WireProviderError {
            kind,
            message,
            resource_id,
            cause,
            provider_name,
            operation,
            status,
            code,
            request_id,
        } = error;
        let resource_id = resource_id
            .map(|id| Box::new(crate::protocol_convert::protocol_to_resource_id(&id).into()));
        let throttling =
            ThrottlingClassification::from_raw(&message, code.as_deref(), cause.as_deref());
        let detail = Box::new(OperationErrorDetail {
            body: match cause {
                Some(cause) => FailureBody::Context {
                    context: message.into(),
                    cause: ProviderDiagnostic::new(FlattenedWireCause(cause)),
                },
                None => FailureBody::Message(message.into()),
            },
            throttling,
            retryability: Retryability::Unclassified,
            resource_id,
            // Host lifecycle markers are intentionally not part of the wire
            // record and untrusted guest errors cannot acquire one on decode.
            host_cause: None,
            provider_name: provider_name.map(ProviderErrorText::new),
            operation: operation.map(ProviderErrorText::new),
            status,
            code: code.map(ProviderErrorText::new),
            request_id: request_id.map(ProviderErrorText::new),
        });
        match kind {
            WireProviderErrorKind::InvalidInput => ProviderFailure::InvalidInput(detail),
            WireProviderErrorKind::ApiError => ProviderFailure::ApiError(detail),
            WireProviderErrorKind::NotFound => ProviderFailure::NotFound(detail),
            WireProviderErrorKind::Timeout => ProviderFailure::Timeout(detail),
            WireProviderErrorKind::Internal => ProviderFailure::Internal(detail),
        }
    }
    pub(crate) fn with_secrets(self, secrets: &RedactionSet) -> HostError {
        let mut context = self.redaction_context();
        context.include(secrets);
        let convert =
            |text: ProviderErrorText| text.into_host(secrets).with_redaction_context(&context);
        let kind = self.kind();
        let detail = match self {
            Self::InvalidInput(d)
            | Self::ApiError(d)
            | Self::NotFound(d)
            | Self::Timeout(d)
            | Self::Internal(d) => *d,
        };
        let detail = Box::new(OperationErrorDetail {
            body: detail.body.bind(secrets, &context),
            throttling: detail.throttling,
            retryability: detail.retryability,
            resource_id: detail.resource_id.map(|id| {
                Box::new(ErrorResourceId {
                    id: id.id,
                    text: convert(id.text),
                })
            }),
            host_cause: detail.host_cause,
            provider_name: detail.provider_name.map(convert),
            operation: detail.operation.map(convert),
            status: detail.status,
            code: detail.code.map(convert),
            request_id: detail.request_id.map(convert),
        });
        match kind {
            ProviderErrorKind::InvalidInput => HostError::InvalidInput(detail),
            ProviderErrorKind::ApiError => HostError::ApiError(detail),
            ProviderErrorKind::NotFound => HostError::NotFound(detail),
            ProviderErrorKind::Timeout => HostError::Timeout(detail),
            ProviderErrorKind::Internal => HostError::Internal(detail),
        }
    }
}
impl From<HostError> for ProviderError {
    fn from(host: HostError) -> Self {
        host.host_for_state()
    }
}
impl ProviderFailure<Bound> {
    pub(crate) fn host_for_state<T: TextState>(self) -> ProviderFailure<T> {
        let kind = self.kind();
        let detail = match self {
            Self::InvalidInput(d)
            | Self::ApiError(d)
            | Self::NotFound(d)
            | Self::Timeout(d)
            | Self::Internal(d) => *d,
        };
        let detail = Box::new(OperationErrorDetail {
            body: detail.body.host_for_state(),
            throttling: detail.throttling,
            retryability: detail.retryability,
            resource_id: detail.resource_id.map(|id| {
                Box::new(ErrorResourceId {
                    id: id.id,
                    text: id.text.host_for_state(),
                })
            }),
            host_cause: detail.host_cause,
            provider_name: detail.provider_name.map(|text| text.host_for_state()),
            operation: detail.operation.map(|text| text.host_for_state()),
            status: detail.status,
            code: detail.code.map(|text| text.host_for_state()),
            request_id: detail.request_id.map(|text| text.host_for_state()),
        });
        match kind {
            ProviderErrorKind::InvalidInput => ProviderFailure::InvalidInput(detail),
            ProviderErrorKind::ApiError => ProviderFailure::ApiError(detail),
            ProviderErrorKind::NotFound => ProviderFailure::NotFound(detail),
            ProviderErrorKind::Timeout => ProviderFailure::Timeout(detail),
            ProviderErrorKind::Internal => ProviderFailure::Internal(detail),
        }
    }

    pub fn to_wire(&self) -> WireProviderError {
        self.to_wire_inner()
    }
}
impl ProviderFailure<Unbound> {
    #[cfg(all(test, not(target_arch = "wasm32")))]
    pub fn to_wire(&self) -> WireProviderError {
        self.to_wire_inner()
    }
    /// Flatten this error for transport. Typed causes and retry policy do not
    /// survive this projection; retain the error for further composition.
    /// Wire decoding is private to the host, not a provider authoring route.
    #[cfg(target_arch = "wasm32")]
    pub fn to_wire(&self) -> WireProviderError {
        self.to_wire_inner()
    }
}

impl PartialReadDiagnostic<Unbound> {
    pub(crate) fn validate(&self, id: &ResourceId, schemas: &SchemaRegistry) -> ProviderResult<()> {
        let schema = schemas.get(
            &id.provider,
            &id.resource_type,
            crate::schema::SchemaKind::Resource,
        );
        if self.missing_attributes.iter().any(|name| {
            !schema
                .as_ref()
                .is_some_and(|schema| schema.as_schema().attributes.contains_key(name.as_str()))
        }) {
            return Err(ProviderError::invalid_input_caused_by(
                "invalid partial-read diagnostic",
                UnknownMissingAttribute,
            ));
        }
        Ok(())
    }

    pub(crate) fn with_secrets(self, secrets: &RedactionSet) -> HostPartialReadDiagnostic {
        PartialReadDiagnostic {
            reason: self.reason.with_secrets(secrets),
            missing_attributes: self.missing_attributes,
        }
    }
}
impl IdentifiedCreate<Unbound> {
    fn admit(&mut self, schemas: &SchemaRegistry) -> ProviderResult<()> {
        if let Some(diagnostic) = self.partial_read.take() {
            diagnostic.validate(&self.id, schemas)?;
            self.partial_read = Some(diagnostic);
        }
        Ok(())
    }

    #[cfg(feature = "test-utils")]
    pub fn failure_for_test(self) -> CreateFailure {
        CreateFailure::identifier_reported(self, ProviderError::internal("fixture"))
    }

    pub(crate) fn with_secrets(self, secrets: &RedactionSet) -> HostIdentifiedCreate {
        IdentifiedCreate {
            id: self.id,
            identifier: self.identifier,
            sent: self.sent,
            resolved_desired_attributes: self.resolved_desired_attributes,
            partial_read: self
                .partial_read
                .map(|diagnostic| Box::new(diagnostic.with_secrets(secrets))),
            observation: match self.observation {
                CreateObservation::Observed(state) => CreateObservation::Observed(state),
                CreateObservation::Unobserved(UnobservedReason::NotFound) => {
                    CreateObservation::Unobserved(UnobservedReason::NotFound)
                }
                CreateObservation::Unobserved(UnobservedReason::ReadFailed(error)) => {
                    CreateObservation::Unobserved(UnobservedReason::ReadFailed(
                        error.with_secrets(secrets),
                    ))
                }
            },
        }
    }
}
impl CreateOutcome<Unbound> {
    pub(crate) fn admit(
        self,
        sent: SentAttributes,
        schemas: &SchemaRegistry,
    ) -> Result<Self, CreateFailure> {
        if let CreateOutcomeKind::PartialSuccess { state, diagnostic } = &self.kind
            && let Err(error) = diagnostic.validate(&state.id, schemas)
        {
            let state = match self.kind {
                CreateOutcomeKind::Success { state }
                | CreateOutcomeKind::PartialSuccess { state, .. } => state,
            };
            let identified = IdentifiedCreate::new(
                state.id.clone(),
                state.identifier.clone(),
                Observation::Read(ReadOutcome::found(state)),
                SentRecord::Request(Sent::Only(sent.attributes)),
            );
            return Err(CreateFailure::identifier_reported(identified, error));
        }
        Ok(self)
    }

    pub(crate) fn with_secrets(self, secrets: &RedactionSet) -> HostCreateOutcome {
        CreateOutcome {
            read_back_end: self.read_back_end,
            kind: match self.kind {
                CreateOutcomeKind::Success { state } => CreateOutcomeKind::Success { state },
                CreateOutcomeKind::PartialSuccess { state, diagnostic } => {
                    CreateOutcomeKind::PartialSuccess {
                        state,
                        diagnostic: diagnostic.with_secrets(secrets),
                    }
                }
            },
        }
    }
}
impl UpdateOutcome<Unbound> {
    pub(crate) fn with_secrets(self, secrets: &RedactionSet) -> HostUpdateOutcome {
        UpdateOutcome {
            read_back_end: self.read_back_end,
            kind: match self.kind {
                UpdateOutcomeKind::Success { state } => UpdateOutcomeKind::Success { state },
                UpdateOutcomeKind::PartialSuccess { state, diagnostic } => {
                    UpdateOutcomeKind::PartialSuccess {
                        state,
                        diagnostic: diagnostic.with_secrets(secrets),
                    }
                }
            },
        }
    }
}
impl CreateFailure<Unbound> {
    pub(crate) fn admit(mut self, schemas: &SchemaRegistry) -> Self {
        if let CreateFailureKind::IdentifierReported { identified, error } = self.0.as_mut()
            && let Err(rejected) = identified.admit(schemas)
        {
            *error = rejected;
        }
        self
    }

    pub(crate) fn with_secrets(self, secrets: &RedactionSet) -> HostCreateFailure {
        HostCreateFailure::from_kind(match self.into_kind() {
            CreateFailureKind::NoResource(error) => {
                CreateFailureKind::NoResource(error.with_secrets(secrets))
            }
            CreateFailureKind::OutcomeUnknown { unknown, error } => {
                CreateFailureKind::OutcomeUnknown {
                    unknown,
                    error: error.with_secrets(secrets),
                }
            }
            CreateFailureKind::IdentifierReported { identified, error } => {
                CreateFailureKind::IdentifierReported {
                    identified: identified.with_secrets(secrets),
                    error: error.with_secrets(secrets),
                }
            }
            CreateFailureKind::InFlight { pending, error } => CreateFailureKind::InFlight {
                pending,
                error: error.with_secrets(secrets),
            },
        })
    }
}
impl ResumeFailure<Unbound> {
    pub(crate) fn admit(mut self, schemas: &SchemaRegistry) -> Self {
        if let ResumeFailureKind::IdentifierReported { identified, error } = self.0.as_mut()
            && let Err(rejected) = identified.admit(schemas)
        {
            *error = rejected;
        }
        self
    }

    pub(crate) fn with_secrets(self, secrets: &RedactionSet) -> HostResumeFailure {
        HostResumeFailure::from_kind(match self.into_kind() {
            ResumeFailureKind::NoResource(error) => {
                ResumeFailureKind::NoResource(error.with_secrets(secrets))
            }
            ResumeFailureKind::IdentifierReported { identified, error } => {
                ResumeFailureKind::IdentifierReported {
                    identified: identified.with_secrets(secrets),
                    error: error.with_secrets(secrets),
                }
            }
            ResumeFailureKind::InFlight { pending, error } => ResumeFailureKind::InFlight {
                pending,
                error: error.with_secrets(secrets),
            },
        })
    }
}
impl ResumeOutcome<Unbound> {
    pub(crate) fn admit(
        mut self,
        sent: SentAttributes,
        schemas: &SchemaRegistry,
    ) -> Result<Self, ResumeFailure> {
        let result = match &mut self.kind {
            ResumeOutcomeKind::Resent(outcome) => {
                if let Some(diagnostic) = outcome.diagnostic() {
                    let id = match outcome.kind() {
                        CreateOutcomeKind::Success { state }
                        | CreateOutcomeKind::PartialSuccess { state, .. } => &state.id,
                    };
                    diagnostic.validate(id, schemas)
                } else {
                    Ok(())
                }
            }
            ResumeOutcomeKind::Resumed(completed) => completed.0.admit(schemas),
            ResumeOutcomeKind::PartialSuccess(partial) => partial
                .diagnostic
                .validate(partial.completed.0.id(), schemas),
        };
        if let Err(error) = result {
            let identified = match self.kind {
                ResumeOutcomeKind::Resumed(completed) => completed.0,
                ResumeOutcomeKind::PartialSuccess(partial) => partial.completed.0,
                ResumeOutcomeKind::Resent(outcome) => {
                    let state = match outcome.into_kind() {
                        CreateOutcomeKind::Success { state }
                        | CreateOutcomeKind::PartialSuccess { state, .. } => state,
                    };
                    IdentifiedCreate::new(
                        state.id.clone(),
                        state.identifier.clone(),
                        Observation::Read(ReadOutcome::found(state)),
                        SentRecord::Request(Sent::Only(sent.attributes)),
                    )
                }
            };
            return Err(ResumeFailure::identifier_reported(identified, error));
        }
        Ok(self)
    }

    pub(crate) fn with_secrets(self, secrets: &RedactionSet) -> HostResumeOutcome {
        HostResumeOutcome {
            kind: match self.kind {
                ResumeOutcomeKind::Resumed(completed) => ResumeOutcomeKind::Resumed(Box::new(
                    CompletedResume(completed.0.with_secrets(secrets)),
                )),
                ResumeOutcomeKind::PartialSuccess(partial) => {
                    ResumeOutcomeKind::PartialSuccess(Box::new(PartialResume {
                        completed: CompletedResume(partial.completed.0.with_secrets(secrets)),
                        diagnostic: partial.diagnostic.with_secrets(secrets),
                        read_back_end: partial.read_back_end,
                    }))
                }
                ResumeOutcomeKind::Resent(outcome) => {
                    ResumeOutcomeKind::Resent(Box::new(outcome.with_secrets(secrets)))
                }
            },
        }
    }
}

// Fixtures can construct standalone outcomes without dispatching a request.
// These adapters exist only in core's unit-test harness.
#[cfg(test)]
impl ProviderFailure<Unbound> {
    #[doc(hidden)]
    pub fn into_host_for_test(self) -> HostError {
        self.with_secrets(&RedactionSet::for_test(&[]))
    }
    #[doc(hidden)]
    pub fn with_secrets_for_test(self, secrets: &RedactionSet) -> HostError {
        self.with_secrets(secrets)
    }
}
#[cfg(test)]
impl PartialReadDiagnostic<Unbound> {
    #[doc(hidden)]
    pub fn into_host_for_test(self) -> HostPartialReadDiagnostic {
        self.with_secrets(&RedactionSet::for_test(&[]))
    }
    #[doc(hidden)]
    pub fn with_secrets_for_test(self, secrets: &RedactionSet) -> HostPartialReadDiagnostic {
        self.with_secrets(secrets)
    }
}
#[cfg(test)]
impl IdentifiedCreate<Unbound> {
    #[doc(hidden)]
    pub fn into_host_for_test(self) -> HostIdentifiedCreate {
        self.with_secrets(&RedactionSet::for_test(&[]))
    }
    #[doc(hidden)]
    pub fn with_secrets_for_test(self, secrets: &RedactionSet) -> HostIdentifiedCreate {
        self.with_secrets(secrets)
    }
}
#[cfg(test)]
impl CreateOutcome<Unbound> {
    #[doc(hidden)]
    pub fn into_host_for_test(self) -> HostCreateOutcome {
        self.with_secrets(&RedactionSet::for_test(&[]))
    }
    #[doc(hidden)]
    pub fn with_secrets_for_test(self, secrets: &RedactionSet) -> HostCreateOutcome {
        self.with_secrets(secrets)
    }
}
#[cfg(test)]
impl UpdateOutcome<Unbound> {
    #[doc(hidden)]
    pub fn into_host_for_test(self) -> HostUpdateOutcome {
        self.with_secrets(&RedactionSet::for_test(&[]))
    }
    #[doc(hidden)]
    pub fn with_secrets_for_test(self, secrets: &RedactionSet) -> HostUpdateOutcome {
        self.with_secrets(secrets)
    }
}
#[cfg(test)]
impl CreateFailure<Unbound> {
    #[doc(hidden)]
    pub fn into_host_for_test(self) -> HostCreateFailure {
        self.with_secrets(&RedactionSet::for_test(&[]))
    }
    #[doc(hidden)]
    pub fn with_secrets_for_test(self, secrets: &RedactionSet) -> HostCreateFailure {
        self.with_secrets(secrets)
    }
}
#[cfg(test)]
impl ResumeFailure<Unbound> {
    #[doc(hidden)]
    pub fn into_host_for_test(self) -> HostResumeFailure {
        self.with_secrets(&RedactionSet::for_test(&[]))
    }
    #[doc(hidden)]
    pub fn with_secrets_for_test(self, secrets: &RedactionSet) -> HostResumeFailure {
        self.with_secrets(secrets)
    }
}
#[cfg(test)]
impl ResumeOutcome<Unbound> {
    #[doc(hidden)]
    pub fn into_host_for_test(self) -> HostResumeOutcome {
        self.with_secrets(&RedactionSet::for_test(&[]))
    }
    #[doc(hidden)]
    pub fn with_secrets_for_test(self, secrets: &RedactionSet) -> HostResumeOutcome {
        self.with_secrets(secrets)
    }
}

impl<P: ProviderNormalizer + ?Sized> ProviderNormalizer for &P {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        (**self).normalize_desired(resources)
    }
    fn normalize_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        (**self).normalize_state(states)
    }
    fn hydrate_read_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
        saved: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        (**self).hydrate_read_state(states, saved)
    }
}

impl<P: ProviderNormalizer + ?Sized> ProviderNormalizer for Box<P> {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        (**self).normalize_desired(resources)
    }
    fn normalize_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        (**self).normalize_state(states)
    }
    fn hydrate_read_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
        saved: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        (**self).hydrate_read_state(states, saved)
    }
}
impl<P: ProviderNormalizer + ?Sized> ProviderNormalizer for Arc<P> {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        (**self).normalize_desired(resources)
    }
    fn normalize_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        (**self).normalize_state(states)
    }
    fn hydrate_read_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
        saved: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        (**self).hydrate_read_state(states, saved)
    }
}

/// Runtime CRUD operations for a provider.
///
/// Each infrastructure provider (AWS, GCP, etc.) implements this trait
/// to perform actual API calls against its infrastructure.
///
/// Every CRUD op (except [`Provider::read_data_source`]) takes a
/// per-operation `*Request` record. Request records mirror the
/// records of the same name in `wit/types.wit`; future per-op
/// parameters become non-breaking record-field additions instead of
/// trait-signature churn.
pub trait Provider<S: TextState = Unbound>: Send + Sync {
    /// Name of this Provider (e.g., "aws")
    fn name(&self) -> &str;

    /// Resolve comparison keys in this initialized instance. An unsupported
    /// identity is an error for each request, never a successful raw echo.
    fn resolve_references<'a>(
        &'a self,
        requests: &'a [crate::references::ReferenceRequest],
    ) -> BoxFuture<'a, crate::references::ReferenceResults<S>> {
        Box::pin(async move {
            Ok(requests
                .iter()
                .map(|request| {
                    Err(crate::references::ReferenceError::InvalidReference(
                        ProviderFailure::<S>::invalid_input(format!(
                            "unsupported reference identity {}",
                            request.ty
                        ))
                        .into(),
                    ))
                })
                .collect())
        })
    }

    /// Read the current provider-side state of an existing resource.
    ///
    /// `identifier` is the validated, non-empty cloud-side identifier (e.g.
    /// `vpc-0abc...`). Resources without saved state do not cross this
    /// boundary. The result distinguishes absence from a present observation,
    /// which itself also carries a validated identifier.
    ///
    /// `request` carries no operationally meaningful fields today; it
    /// exists so future fields (e.g. a freshness hint or an attribute
    /// projection) can be added without breaking the signature.
    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome, S>>;

    /// Read a data source resource.
    ///
    /// Unlike [`Provider::read`], this receives the full checked data source so
    /// the provider can see the user-supplied input attributes (e.g. the
    /// `identity_store_id` + `user_name` that `aws.identitystore.user`
    /// needs to resolve itself via the AWS SDK).
    ///
    /// Each provider must implement this explicitly, including zero-input data
    /// sources (e.g. `aws.sts.caller_identity`). Data sources have no managed
    /// provider identifier, so they must not be routed through
    /// [`Provider::read`]. Their generic [`State`] may therefore omit an
    /// identifier without weakening the managed-resource read boundary.
    ///
    /// # `State.exists` contract
    ///
    /// A successful read MUST return a `State` with `exists: true`,
    /// even when the query yielded no rows — the data source itself
    /// "exists" (the lookup succeeded); the result set being empty is
    /// just an empty-list attribute on the returned state. Reserve
    /// `exists: false` for *failure to resolve the data source itself*
    /// (the query returned no resource at all, e.g. an
    /// `identity_store_id` whose store does not exist).
    ///
    /// This matters for the binding view (`ResolvedBindings::pre_apply`
    /// / `layer_data_source_bindings`): the merge that surfaces the
    /// read result to downstream `ResourceRef`s is gated on
    /// `state.exists`, so `exists: false` causes the binding to drop
    /// the read state entirely and downstream `ResourceRef`s fail with
    /// the "has not been published yet" diagnostic (carina#3252).
    ///
    /// A raw data source cannot cross this boundary:
    ///
    /// ```compile_fail
    /// use carina_core::provider::Provider;
    /// use carina_core::resource::DataSource;
    ///
    /// fn dispatch(provider: &dyn Provider, resource: &DataSource) {
    ///     let _ = provider.read_data_source(resource);
    /// }
    /// ```
    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State, S>>;

    /// Create the resource described by `request.resource()` and return
    /// the resulting state (with `identifier` set to the cloud-side
    /// internal ID, e.g. `vpc-xxx`).
    ///
    /// ```compile_fail,E0061
    /// use carina_core::provider::{Provider, CreateRequest};
    /// use carina_core::resource::ResourceId;
    /// fn bypass(provider: &dyn Provider, id: &ResourceId, request: CreateRequest) {
    ///     let _ = provider.create(id, request);
    /// }
    /// ```
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome<S>, CreateFailure<S>>>;

    /// Resume a create continuation loaded from state.
    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome<S>, ResumeFailure<S>>>;

    /// Update an existing resource by applying `request.patch()`.
    ///
    /// Each [`PatchOp`] corresponds to a key the user explicitly specified or
    /// removed in the desired state, or to a desired write-only key that must
    /// be reasserted alongside another change. Fields the user has never
    /// specified do not appear in the patch.
    ///
    /// **Providers MUST NOT modify any attribute that is not
    /// represented in `request.patch().ops`.** The patch is the sole
    /// source of truth for the update payload.
    ///
    /// `request.from()` is the current provider-side state and may be
    /// used for read-modify-write paths or for resolving
    /// server-assigned identifiers; it MUST NOT be used to derive
    /// additional fields to write back.
    ///
    /// A raw request cannot be dispatched without the core-minted witness:
    ///
    /// ```compile_fail,E0061
    /// use carina_core::provider::{AppliedIdentifier, Provider, UpdateRequest};
    /// use carina_core::resource::ResourceId;
    /// fn bypass(provider: &dyn Provider, id: &ResourceId,
    ///           identifier: &AppliedIdentifier, request: UpdateRequest) {
    ///     let _ = provider.update(id, identifier, request);
    /// }
    /// ```
    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        attempt: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome<S>, S>>;

    /// Delete the exact resource released by the scheduler. The attempt owns
    /// its id, cloud-side identifier, and schema-resolved request directives.
    ///
    /// Raw requests cannot authorize deletion, including the public chain
    /// that previously bypassed scheduler readiness (carina#4306):
    ///
    /// ```compile_fail,E0061
    /// use carina_core::provider::{AppliedIdentifier, DeleteRequest, Provider, ProviderDirectives};
    /// use carina_core::resource::ResourceId;
    /// async fn bypass(provider: &dyn Provider, id: ResourceId, resolved: ProviderDirectives)
    ///     -> Result<(), Box<dyn std::error::Error>> {
    ///     provider.delete(&id, &AppliedIdentifier::new("x")?, DeleteRequest::from_resolved(resolved)).await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// Resolving a request against a schema grants no delete authority either:
    ///
    /// ```compile_fail,E0061
    /// use carina_core::provider::{AppliedIdentifier, DeleteRequest, Provider};
    /// use carina_core::resource::{Directives, ProviderDirectiveSchema, ResourceId};
    /// fn bypass(provider: &dyn Provider, id: &ResourceId, identifier: &AppliedIdentifier,
    ///           directives: &Directives, schema: ProviderDirectiveSchema<'_>) {
    ///     let _ = provider.delete(id, identifier, DeleteRequest::resolve(directives, schema));
    /// }
    /// ```
    fn delete(&self, attempt: DeleteAttempt) -> BoxFuture<'_, ProviderResult<(), S>>;

    /// Permissions this provider needs to perform `op` on `id`.
    /// Empty vec means the provider declares no permissions for this resource/op pair.
    fn required_permissions(
        &self,
        id: &ResourceId,
        op: PlanOp,
    ) -> ProviderResult<Vec<S::Permission>, S>;

    /// Binding-name patterns for resources that can satisfy a wait on `target_id.attr_path`.
    /// Empty vec means the provider declares no satisfier hint for this target attribute.
    fn satisfier_hint(
        &self,
        _target_id: &ResourceId,
        _attr_path: &AttrPath,
    ) -> ProviderResult<Vec<BindingPattern>, S> {
        Ok(Vec::new())
    }
}

mod host_provider_sealed {
    pub trait Sealed {}
}
/// Host CRUD interface. Every returned diagnostic has crossed its call boundary.
pub trait HostProvider: Provider<Bound> + host_provider_sealed::Sealed {}
impl HostProvider for ProviderBoundary<'_> {}
impl HostProvider for ProviderRouter {}
impl host_provider_sealed::Sealed for ProviderBoundary<'_> {}
impl host_provider_sealed::Sealed for ProviderRouter {}
mod host_normalizer_sealed {
    pub trait Sealed {}
}
/// Host normalizer results have crossed core's fixed-session boundary.
pub trait HostNormalizer: ProviderNormalizer<Bound> + host_normalizer_sealed::Sealed {
    /// Core-owned route admission for desired normalization.
    fn owns_desired(&self, id: &ResourceId) -> bool;
}
impl HostNormalizer for ProviderBoundary<'_> {
    fn owns_desired(&self, id: &ResourceId) -> bool {
        self.owns_desired(id)
    }
}
impl host_normalizer_sealed::Sealed for ProviderBoundary<'_> {}
impl HostNormalizer for ProviderRouter {
    fn owns_desired(&self, id: &ResourceId) -> bool {
        self.normalizers.owns_desired(id)
    }
}
impl host_normalizer_sealed::Sealed for ProviderRouter {}
impl HostNormalizer for NormalizerRouter {
    fn owns_desired(&self, id: &ResourceId) -> bool {
        self.normalizers
            .iter()
            .any(|normalizer| normalizer.owns_desired(id))
    }
}
impl host_normalizer_sealed::Sealed for NormalizerRouter {}
impl<N: ProviderNormalizer> HostNormalizer for ConfiguredNormalizer<N> {
    fn owns_desired(&self, id: &ResourceId) -> bool {
        self.owns(id)
    }
}
impl<N: ProviderNormalizer> host_normalizer_sealed::Sealed for ConfiguredNormalizer<N> {}
impl HostNormalizer for NoopNormalizer {
    fn owns_desired(&self, _: &ResourceId) -> bool {
        true
    }
}
impl host_normalizer_sealed::Sealed for NoopNormalizer {}

/// Convenience for a `ProviderNormalizer` method that does nothing.
///
/// Returns an immediately-ready future. A `BoxFuture`-returning trait
/// method cannot have an empty `{}` default body, so every "I don't
/// normalize this" implementation returns this explicitly — keeping the
/// no-op a deliberate, visible choice rather than a silent default
/// (the hazard that caused carina-rs/carina-provider-awscc#192).
pub fn ready_noop<'a, S: TextState>() -> BoxFuture<'a, ProviderResult<(), S>> {
    Box::pin(async { Ok(()) })
}

/// Provider-transformable states with a fixed set of resource keys.
///
/// Normalizers can mutate every state value, but the API deliberately has no
/// insertion, removal, or mutable-key operation. This makes preservation of
/// the host-owned route set a type guarantee rather than a post-call assert.
///
/// ```compile_fail
/// use carina_core::provider::StateTransformMap;
/// use carina_core::resource::{ResourceId, State};
///
/// fn add_route(states: &mut StateTransformMap, id: ResourceId) {
///     states.insert(id.clone(), State::not_found(id));
/// }
/// ```
#[derive(Debug, PartialEq)]
pub struct StateTransformMap(HashMap<ResourceId, SnapshotState>);

impl StateTransformMap {
    pub(crate) fn from_states(states: HashMap<ResourceId, SnapshotState>) -> Self {
        Self(states)
    }

    pub(crate) fn into_states(self) -> HashMap<ResourceId, SnapshotState> {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ResourceId, &SnapshotState)> {
        self.0.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&ResourceId, &mut SnapshotState)> {
        self.0.iter_mut()
    }

    pub fn values(&self) -> impl Iterator<Item = &SnapshotState> {
        self.0.values()
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut SnapshotState> {
        self.0.values_mut()
    }

    pub fn get(&self, id: &ResourceId) -> Option<&SnapshotState> {
        self.0.get(id)
    }

    pub fn get_mut(&mut self, id: &ResourceId) -> Option<&mut SnapshotState> {
        self.0.get_mut(id)
    }
}

impl std::ops::Deref for StateTransformMap {
    type Target = HashMap<ResourceId, SnapshotState>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> IntoIterator for &'a StateTransformMap {
    type Item = (&'a ResourceId, &'a SnapshotState);
    type IntoIter = std::collections::hash_map::Iter<'a, ResourceId, SnapshotState>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a mut StateTransformMap {
    type Item = (&'a ResourceId, &'a mut SnapshotState);
    type IntoIter = std::collections::hash_map::IterMut<'a, ResourceId, SnapshotState>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut()
    }
}

/// Plan-time normalizer for a provider.
///
/// Normalizes desired state and read state so that diffs produce correct
/// plans. Uses provider-specific schema knowledge. Separated from `Provider`
/// because these are normalization concerns rather than runtime CRUD.
///
/// The methods are **async** (returning [`BoxFuture`], mirroring the
/// [`Provider`] trait) so a host implementation that drives an async
/// backend — e.g. `WasmProviderNormalizer` `.await`ing the WASM guest's
/// store lock — does so directly, without a synchronous method bridging
/// to async via a nested `block_on` (the self-deadlock fixed by
/// carina#3112). Each method mutates its arguments in place and returns a
/// [`ProviderResult`] so boundary and provider failures cannot be silently
/// converted into unnormalized values. The returned future borrows the arguments for `'a`, so
/// callers must `.await` it before the borrow ends (they always do —
/// the futures are never run concurrently).
pub trait ProviderNormalizer<S: TextState = Unbound>: Send + Sync {
    /// Normalize desired resource state before diffing.
    ///
    /// For example, resolves bare enum identifiers like `advanced` or
    /// `Tier.advanced` into fully-qualified DSL format like
    /// `awscc.ec2_ipam.Tier.advanced` based on schema definitions.
    /// Providers without enum types return [`ready_noop`].
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<(), S>>;

    /// Normalize resolved dispatch inputs. Host boundaries collect annotated
    /// values first, then give raw providers the established plaintext shape.
    fn normalize_dispatch_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<(), S>> {
        crate::executor::normalized::unwrap_dispatch_desired(resources);
        self.normalize_desired(resources)
    }

    /// Normalize current state values before diffing.
    ///
    /// Converts raw values in current state (e.g., `"ap-northeast-1a"`) to
    /// the same DSL enum format that `normalize_desired` produces
    /// (e.g., `"awscc.AvailabilityZone.ZoneName.ap_northeast_1a"`).
    /// This prevents false diffs when state stores raw AWS values but
    /// desired state has been normalized.
    /// Providers without enum types return [`ready_noop`].
    fn normalize_state<'a>(
        &'a self,
        current_states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<(), S>>;

    /// Hydrate read state with saved attributes that APIs don't return.
    ///
    /// Some APIs (e.g., CloudControl) don't return certain properties in read
    /// responses (create-only properties, or normal properties like `description`
    /// on some resources). This method carries them forward from previously
    /// saved attribute values.
    /// Providers that don't hydrate return [`ready_noop`].
    fn hydrate_read_state<'a>(
        &'a self,
        current_states: &'a mut StateTransformMap,
        saved_attrs: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<(), S>>;
}

/// Normalize state values while keeping the caller-owned route set fixed.
///
/// The provider receives only the non-constructible [`StateTransformMap`]
/// view. Changes commit atomically after a successful call; on error the
/// caller's map is unchanged.
pub async fn normalize_provider_states<N, T: TextState>(
    normalizer: &N,
    states: &mut HashMap<ResourceId, SnapshotState>,
) -> ProviderResult<(), T>
where
    N: ProviderNormalizer<T> + ?Sized,
{
    let mut transformed = StateTransformMap::from_states(states.clone());
    normalizer.normalize_state(&mut transformed).await?;
    *states = transformed.into_states();
    Ok(())
}

/// Hydrate state values while keeping the caller-owned route set fixed.
///
/// Like [`normalize_provider_states`], this commits only a successful
/// transformation and leaves the input unchanged after an error.
pub async fn hydrate_provider_states<N, T: TextState>(
    normalizer: &N,
    states: &mut HashMap<ResourceId, SnapshotState>,
    saved_attrs: &SavedAttrs,
) -> ProviderResult<(), T>
where
    N: ProviderNormalizer<T> + ?Sized,
{
    let mut transformed = StateTransformMap::from_states(states.clone());
    normalizer
        .hydrate_read_state(&mut transformed, saved_attrs)
        .await?;
    *states = transformed.into_states();
    Ok(())
}

/// Host ingestion of state normalization requires an already-bound normalizer.
pub async fn normalize_host_provider_states<N: HostNormalizer + ?Sized>(
    normalizer: &N,
    states: &mut HashMap<ResourceId, SnapshotState>,
) -> HostResult<()> {
    normalize_provider_states(normalizer, states).await
}
/// Host ingestion of hydration requires an already-bound normalizer.
pub async fn hydrate_host_provider_states<N: HostNormalizer + ?Sized>(
    normalizer: &N,
    states: &mut HashMap<ResourceId, SnapshotState>,
    saved_attrs: &SavedAttrs,
) -> HostResult<()> {
    hydrate_provider_states(normalizer, states, saved_attrs).await
}

/// A no-op normalizer for providers that don't need plan-time normalization.
#[derive(Debug, Clone, Copy)]
pub struct NoopNormalizer;
impl<S: TextState> ProviderNormalizer<S> for NoopNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        _resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<(), S>> {
        ready_noop()
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<(), S>> {
        ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut StateTransformMap,
        _saved_attrs: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<(), S>> {
        ready_noop()
    }
}

/// The exact initialized provider instance selected for a resource operation.
///
/// The provider kind alone is insufficient: named instances of one provider
/// kind may initialize in different regions or accounts and therefore carry
/// different reference-comparison scopes.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ProviderInstanceKey {
    provider: String,
    provider_instance: Option<String>,
}

impl ProviderInstanceKey {
    pub fn new(provider: impl Into<String>, provider_instance: Option<String>) -> Self {
        Self {
            provider: provider.into(),
            provider_instance,
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn provider_instance(&self) -> Option<&str> {
        self.provider_instance.as_deref()
    }

    fn matches(&self, id: &ResourceId) -> bool {
        self.provider == id.provider && self.provider_instance == id.provider_instance
    }
}

impl<P: crate::resource::PlaceholderKind, B: crate::resource::BindingAddress>
    From<&ProviderConfig<P, B>> for ProviderInstanceKey
{
    fn from(config: &ProviderConfig<P, B>) -> Self {
        Self::new(
            config.name.clone(),
            config.binding.as_ref().map(ToString::to_string),
        )
    }
}

impl From<&ResourceId> for ProviderInstanceKey {
    fn from(id: &ResourceId) -> Self {
        Self::new(id.provider.clone(), id.provider_instance.clone())
    }
}

/// Check a default-tag value before host-owned merging. Unresolved values are
/// skippable; redacted secrets are fatal.
fn assert_default_tag_value_encodable(
    value: &crate::resource::knowledge::Classification<'_>,
) -> Result<(), SerializationError> {
    use crate::resource::knowledge::{Classification, KnownValue, KnownView, ShapeView};
    if crate::resource::knowledge::ClassifiedRef::from(value)
        .secret()
        .is_some()
    {
        return Ok(());
    }
    fn known(value: &KnownValue<'_>) -> Result<(), SerializationError> {
        value.visit(
            |value| match value {
                KnownView::EnumIdentifier(value) => {
                    Err(SerializationError::UnresolvedEnumIdentifier {
                        value: value.to_string(),
                        context: crate::value::SerializationContext::WasmBoundary,
                    })
                }
                KnownView::List(items) => {
                    for value in items.iter() {
                        known(value)?;
                    }
                    Ok(())
                }
                KnownView::Map(items) => {
                    for (_, value) in items.iter() {
                        known(value)?;
                    }
                    Ok(())
                }
                KnownView::String(_)
                | KnownView::CanonicalEnum(_)
                | KnownView::Int(_)
                | KnownView::Float(_)
                | KnownView::Bool(_)
                | KnownView::Duration(_)
                | KnownView::StringList(_) => Ok(()),
            },
            |_| Ok(()),
        )
    }
    match value {
        Classification::Known(value) => known(value),
        Classification::Failed(value) => Err(crate::resource::knowledge_serialization_error(
            value.error(),
        )),
        Classification::ShapeKnown(value) => value.visit(
            |view| match view {
                ShapeView::List(items) => {
                    for value in items {
                        assert_default_tag_value_encodable(value)?;
                    }
                    Ok(())
                }
                ShapeView::Map(items) => {
                    for value in items.values() {
                        assert_default_tag_value_encodable(value)?;
                    }
                    Ok(())
                }
            },
            |_| Ok(()),
        ),
        Classification::Pending(value) => {
            crate::resource::require_known_value(Classification::Pending(value.clone())).map(|_| ())
        }
    }
}

#[derive(Debug)]
struct DefaultTagEncodingError {
    key: String,
    provider_name: String,
    source: SerializationError,
}

impl std::fmt::Display for DefaultTagEncodingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "key '{}' of provider '{}': {}",
            DiagnosticText::new(&self.key),
            DiagnosticText::new(&self.provider_name),
            self.source
        )
    }
}

impl std::error::Error for DefaultTagEncodingError {}

/// Merge one provider instance's default tags into resources routed to it.
///
/// For each resource matching the [`ProviderConfig`]'s `(name, binding)`
/// routing key whose schema includes a `tags` attribute:
/// - If the resource has no `tags`, set it to the config's `default_tags`
/// - If the resource has `tags`, merge defaults in (resource-level tags win)
///
/// Records which tag keys came from defaults as host-owned provenance on the
/// resource, separate from provider-facing attributes.
pub fn merge_default_tags<S, D>(
    config: &ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >,
    resources: &mut [Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >],
    registry: &SchemaRegistry,
) -> HostResult<()> {
    if config.default_tags.is_empty() {
        return Ok(());
    }

    // Validate every tag before touching resources so a fatal encoding error
    // cannot leave a partially merged desired state behind.
    let mut default_tags = IndexMap::new();
    for (key, value) in &config.default_tags {
        match value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .map_err(crate::resource::knowledge_serialization_error)
            .and_then(|value| assert_default_tag_value_encodable(&value))
        {
            Ok(()) => {
                default_tags.insert(key.clone(), value.clone());
            }
            Err(
                error @ (SerializationError::UnknownNotAllowed { .. }
                | SerializationError::UnresolvedResourceRef { .. }
                | SerializationError::UnresolvedCoalesce { .. }
                | SerializationError::UnresolvedIntegerArithmetic { .. }
                | SerializationError::UnresolvedInterpolation { .. }
                | SerializationError::UnresolvedFunctionCall { .. }
                | SerializationError::UnresolvedAccess { .. }
                | SerializationError::Knowledge { .. }
                | SerializationError::SecretNotAllowed { .. }
                | SerializationError::SavedPlanFieldSecretNotAllowed { .. }),
            ) => {
                log::error!("Skipping default_tag '{key}' with unresolvable value: {error}");
            }
            Err(
                source @ (SerializationError::NullProviderSecret
                | SerializationError::UnresolvedEnumIdentifier { .. }
                | SerializationError::UnexpandedModuleIf { .. }
                | SerializationError::UnavailableSecret(_)
                | SerializationError::InsideSecret { .. }),
            ) => {
                return Err(HostError::invalid_input_caused_by(
                    "failed to encode default tag",
                    crate::provider_text::HostDiagnostic::capture_host_or_provider(
                        DefaultTagEncodingError {
                            key: key.clone(),
                            provider_name: config.name.clone(),
                            source,
                        },
                    ),
                )
                .for_provider(config.name.clone()));
            }
        }
    }
    if default_tags.is_empty() {
        return Ok(());
    }

    let route = ProviderInstanceKey::from(config);
    for resource in resources.iter_mut() {
        if !route.matches(&resource.id) {
            continue;
        }

        // Only a map(string) attribute can receive provider default tags.
        // Some providers use the same attribute name for list/struct shapes;
        // presence alone is therefore not enough to opt a resource in.
        let has_tags = registry.get_for(resource).is_some_and(|schema| {
            let schema = schema.as_schema();
            let Some(tags) = schema.attributes.get("tags") else {
                return false;
            };
            let Shape::Map {
                key,
                value,
                constraints: _,
            } = schema.shape_of(&tags.attr_type)
            else {
                return false;
            };
            matches!(schema.shape_of(key), Shape::String { .. })
                && matches!(schema.shape_of(value), Shape::String { .. })
        });

        if !has_tags {
            continue;
        }

        let default_tag_keys = &mut resource.default_tag_keys;
        match resource.attributes.get_mut("tags") {
            Some(Value::Concrete(ConcreteValue::Map(existing_tags))) => {
                for (key, value) in &default_tags {
                    if !existing_tags.contains_key(key) {
                        existing_tags.insert(key.clone(), value.clone());
                        default_tag_keys.insert(key.clone());
                    }
                }
            }
            None => {
                for key in default_tags.keys() {
                    default_tag_keys.insert(key.clone());
                }
                resource.attributes.insert(
                    "tags".to_string(),
                    Value::Concrete(ConcreteValue::Map(default_tags.clone())),
                );
            }
            _ => {
                continue;
            }
        }
    }

    Ok(())
}

/// The opaque result of initializing one provider instance.
///
/// Native providers can state honestly that no reference scope is available.
/// An available scope requires the one-shot mint that core passes into the
/// factory initialization step.
pub struct InitializeOutcome {
    pub(crate) reference_scope: ReferenceScope,
}

/// Raw provider plus the opaque outcome of its initialization transition.
pub struct ProviderInitialization {
    provider: Box<dyn Provider>,
    outcome: InitializeOutcome,
}

impl ProviderInitialization {
    pub(crate) fn into_parts(self) -> (Box<dyn Provider>, InitializeOutcome) {
        (self.provider, self.outcome)
    }
}

impl InitializeOutcome {
    pub fn unavailable() -> Self {
        Self {
            reference_scope: ReferenceScope::Unavailable,
        }
    }

    pub fn with_provider(self, provider: Box<dyn Provider>) -> ProviderInitialization {
        ProviderInitialization {
            provider,
            outcome: self,
        }
    }
}

/// One-shot capability supplied only while a provider factory initializes an
/// instance. Its private field prevents callers from minting an available
/// initialization outcome outside that transition.
#[doc(hidden)]
pub struct InitializeOutcomeMint {
    _private: (),
}

impl InitializeOutcomeMint {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }

    pub fn available(
        self,
        partition: impl Into<String>,
        region: impl Into<String>,
        account: impl Into<String>,
    ) -> Result<InitializeOutcome, ReferenceScopeError> {
        ReferenceScope::available(partition, region, account)
            .map(|reference_scope| InitializeOutcome { reference_scope })
    }
}

/// A provider implementation paired with the outcome returned by that exact
/// initialized instance.
///
/// The fields are private so a routed provider cannot exist without an
/// explicit scope. Comparison contexts borrow this witness rather than a raw
/// provider, which makes scope selection part of the type-level call path.
///
/// Downstream code cannot pair an arbitrary provider with an available scope:
///
/// ```compile_fail
/// use carina_core::provider::{
///     InitializeOutcomeMint, InitializedProvider, Provider,
/// };
///
/// fn forge(provider: Box<dyn Provider>) -> InitializedProvider {
///     let mint = InitializeOutcomeMint { _private: () };
///     let outcome = mint.available("aws", "us-east-1", "123456789012");
///     InitializedProvider::new(provider, outcome)
/// }
/// ```
pub struct InitializedProvider {
    pub(crate) provider: Arc<ProviderBoundary<'static>>,
    pub(crate) reference_scope: ReferenceScope,
}

impl InitializedProvider {
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn for_test(provider: Box<dyn Provider>, reference_scope: ReferenceScope) -> Self {
        struct Factory {
            provider: std::sync::Mutex<Option<Box<dyn Provider>>>,
            scope: ReferenceScope,
        }
        impl ProviderFactory for Factory {
            fn name(&self) -> &str {
                "fixture"
            }
            fn display_name(&self) -> &str {
                "Fixture"
            }
            fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
                ValidatedConfigSchema::default()
            }
            fn validate_config(
                &self,
                _: &CanonicalProviderConfigAttributes,
                _: &crate::provider::ProviderValidationInputs,
            ) -> ProviderResult<()> {
                Ok(())
            }
            fn schemas(&self) -> Vec<ValidatedResourceSchema> {
                Vec::new()
            }
            fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
                ValidatedProviderSchemaWitness::new(
                    Vec::new(),
                    ValidatedConfigSchema::default(),
                    Default::default(),
                    Vec::new(),
                )
                .unwrap()
            }
            fn create_provider(
                &self,
                _: &crate::provider::ProviderSession,
                _: InitializeOutcomeMint,
            ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
                let provider = self.provider.lock().unwrap().take().unwrap();
                let reference_scope = self.scope.clone();
                Box::pin(async move {
                    Ok(InitializeOutcome { reference_scope }.with_provider(provider))
                })
            }
        }
        futures::executor::block_on(ProviderBoundary::initialize(
            &Factory {
                provider: std::sync::Mutex::new(Some(provider)),
                scope: reference_scope,
            },
            &ProviderPlan::state_only().session("", None, None),
        ))
        .unwrap()
    }
    pub fn into_boundary(self) -> ProviderBoundary<'static> {
        Arc::try_unwrap(self.provider)
            .unwrap_or_else(|_| unreachable!("initialized boundary is uniquely owned"))
    }
    pub fn reference_scope(&self) -> &ReferenceScope {
        &self.reference_scope
    }

    pub fn provider(&self) -> &(dyn HostProvider + 'static) {
        self.provider.as_ref()
    }
}

/// Provider normalizers without CRUD-capable initialized instances.
///
/// Normalizer-only loading returns this type. It deliberately has no API that
/// can mint a [`crate::differ::ComparisonSchema`].
pub struct ConfiguredNormalizer<N = Box<dyn ProviderNormalizer>> {
    pub(crate) diagnostic_inputs: ProviderValidationInputs,
    pub(crate) route: Option<ProviderInstanceKey>,
    pub(crate) provider_name: String,
    pub(crate) ext: N,
    pub(crate) config_secrets: RedactionSet,
    pub(crate) schemas: SchemaRegistry,
}

impl AsRef<dyn HostNormalizer> for ConfiguredNormalizer {
    fn as_ref(&self) -> &(dyn HostNormalizer + 'static) {
        self
    }
}

pub struct NormalizerRouter {
    normalizers: Vec<Arc<dyn HostNormalizer>>,
}
impl Default for NormalizerRouter {
    fn default() -> Self {
        Self::new()
    }
}
impl NormalizerRouter {
    pub fn new() -> Self {
        Self {
            normalizers: Vec::new(),
        }
    }
    pub fn add_initialized_normalizer(&mut self, normalizer: ConfiguredNormalizer) {
        self.normalizers.push(Arc::new(normalizer));
    }
}
#[cfg(test)]
impl<N> ConfiguredNormalizer<N> {
    pub(crate) fn fixture(ext: N) -> Self {
        let session = ProviderPlan::state_only().session("", None, None);
        Self {
            diagnostic_inputs: session.guest_inputs(),
            route: None,
            provider_name: String::new(),
            ext,
            config_secrets: session.redaction_set().clone(),
            schemas: SchemaRegistry::new(),
        }
    }
    pub(crate) fn fixture_with_session(
        ext: N,
        session: &ProviderSession,
        schemas: &SchemaRegistry,
    ) -> Self {
        Self {
            diagnostic_inputs: session.guest_inputs(),
            route: None,
            provider_name: String::new(),
            ext,
            config_secrets: session.redaction_set().clone(),
            schemas: schemas.clone(),
        }
    }
}
#[cfg(test)]
impl NormalizerRouter {
    pub(crate) fn add_normalizer(
        &mut self,
        provider_name: impl Into<String>,
        ext: Box<dyn ProviderNormalizer>,
    ) {
        let provider_name = provider_name.into();
        let session = ProviderPlan::state_only().session(&provider_name, None, None);
        self.add_initialized_normalizer(ConfiguredNormalizer {
            diagnostic_inputs: session.guest_inputs(),
            route: None,
            provider_name,
            ext,
            config_secrets: session.redaction_set().clone(),
            schemas: SchemaRegistry::new(),
        });
    }
}
#[cfg(test)]
impl ProviderRouter {
    pub(crate) fn add_normalizer(
        &mut self,
        provider_name: impl Into<String>,
        ext: Box<dyn ProviderNormalizer>,
    ) {
        self.normalizers.add_normalizer(provider_name, ext);
    }
}
impl<N: ProviderNormalizer> ConfiguredNormalizer<N> {
    fn dispatch<'a, F: std::future::Future + Send + 'a>(
        &self,
        invoke: impl FnOnce() -> F,
    ) -> BoxFuture<'a, F::Output> {
        self.diagnostic_inputs.diagnostics().dispatch(invoke)
    }
    fn owns(&self, id: &ResourceId) -> bool {
        self.route.as_ref().is_none_or(|route| {
            route.provider() == id.provider
                && route.provider_instance() == id.provider_instance.as_deref()
        })
    }
    fn bind_error(&self, error: ProviderError) -> HostError {
        error
            .for_provider(self.provider_name.clone())
            .with_secrets(&self.config_secrets)
    }
    async fn transform_states(
        &self,
        states: &mut StateTransformMap,
        saved: Option<&SavedAttrs>,
    ) -> HostResult<()> {
        let mut selected = StateTransformMap::from_states(
            states
                .iter()
                .filter(|(id, _)| self.owns(id))
                .map(|(id, state)| (id.clone(), state.clone()))
                .collect(),
        );
        if let Some(saved) = saved {
            let saved = SavedAttrs(
                saved
                    .iter()
                    .filter(|(id, _)| self.owns(id))
                    .map(|(id, attrs)| (id.clone(), attrs.clone()))
                    .collect(),
            );
            self.dispatch(|| self.ext.hydrate_read_state(&mut selected, &saved))
                .await
                .map_err(|error| self.bind_error(error))?;
        } else {
            self.dispatch(|| self.ext.normalize_state(&mut selected))
                .await
                .map_err(|error| self.bind_error(error))?;
        }
        states.0.extend(selected.into_states());
        Ok(())
    }
}
impl<N: ProviderNormalizer> ConfiguredNormalizer<N> {
    fn normalize_desired_inner<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
        dispatch: bool,
    ) -> BoxFuture<'a, HostResult<()>> {
        Box::pin(async move {
            let (positions, mut selected): (Vec<_>, Vec<_>) = resources
                .iter()
                .enumerate()
                .filter(|(_, resource)| self.owns(&resource.id))
                .map(|(index, resource)| (index, resource.clone()))
                .unzip();
            let ids = selected
                .iter()
                .map(|resource| resource.id.clone())
                .collect::<Vec<_>>();
            self.config_secrets.extend(&RedactionSet::collect(
                selected
                    .iter()
                    .flat_map(|resource| resource.attributes.values()),
            ));
            if dispatch {
                crate::executor::normalized::unwrap_dispatch_desired(&mut selected);
            }
            self.dispatch(|| self.ext.normalize_desired(&mut selected))
                .await
                .map_err(|error| self.bind_error(error))?;
            if ids
                .iter()
                .zip(&selected)
                .any(|(id, returned)| id != &returned.id)
            {
                return Err(self.bind_error(ProviderError::invalid_input(
                    "provider normalizer changed resource identity",
                )));
            }
            crate::value::canonicalize_normalized_resources_with_schemas(
                &mut selected,
                &self.schemas,
            )
            .map_err(|error| self.bind_error(error.into()))?;
            for (position, resource) in positions.into_iter().zip(selected) {
                resources[position] = resource;
            }
            Ok(())
        })
    }
}

impl<N: ProviderNormalizer> ProviderNormalizer<Bound> for ConfiguredNormalizer<N> {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalize_desired_inner(resources, false)
    }
    fn normalize_dispatch_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalize_desired_inner(resources, true)
    }
    fn normalize_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, HostResult<()>> {
        Box::pin(self.transform_states(states, None))
    }
    fn hydrate_read_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
        saved: &'a SavedAttrs,
    ) -> BoxFuture<'a, HostResult<()>> {
        Box::pin(self.transform_states(states, Some(saved)))
    }
}

impl ProviderNormalizer<Bound> for NormalizerRouter {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, HostResult<()>> {
        Box::pin(async move {
            for normalizer in &self.normalizers {
                normalizer.normalize_desired(resources).await?;
            }
            Ok(())
        })
    }
    fn normalize_dispatch_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, HostResult<()>> {
        Box::pin(async move {
            for normalizer in &self.normalizers {
                normalizer.normalize_dispatch_desired(resources).await?;
            }
            for resource in resources {
                if !self
                    .normalizers
                    .iter()
                    .any(|normalizer| normalizer.owns_desired(&resource.id))
                {
                    crate::executor::normalized::unwrap_dispatch_desired(std::slice::from_mut(
                        resource,
                    ));
                }
            }
            Ok(())
        })
    }
    fn normalize_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, HostResult<()>> {
        Box::pin(async move {
            for normalizer in &self.normalizers {
                normalizer.normalize_state(states).await?;
            }
            Ok(())
        })
    }
    fn hydrate_read_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
        saved: &'a SavedAttrs,
    ) -> BoxFuture<'a, HostResult<()>> {
        Box::pin(async move {
            for normalizer in &self.normalizers {
                normalizer.hydrate_read_state(states, saved).await?;
            }
            Ok(())
        })
    }
}

/// A provider that routes operations to the correct sub-provider
/// based on the resource's `(provider, provider_instance)` pair.
///
/// Each entry is keyed by `(kind, binding)`:
/// - `binding = None` is the kind's default instance, used by resources
///   that omit `directives { provider = ... }`.
/// - `binding = Some(name)` is a named instance declared as
///   `let <name> = provider <kind> { ... }` and selected via
///   `directives { provider = <name> }`.
pub struct ProviderRouter {
    providers: HashMap<ProviderInstanceKey, InitializedProvider>,
    normalizers: NormalizerRouter,
}

/// Why an initialized provider comparison could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComparisonResolutionError {
    MissingResourceSchema { id: ResourceId },
    UnknownProviderInstance { instance: ProviderInstanceKey },
    SchemaOwnerMismatch { id: ResourceId, owner: String },
    ComparisonNotResolvedAtApplyStart { id: ResourceId },
}

impl std::fmt::Display for ComparisonResolutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingResourceSchema { id } => {
                write!(formatter, "missing managed resource schema for `{id}`")
            }
            Self::UnknownProviderInstance { instance } => match instance.provider_instance() {
                Some(binding) => write!(
                    formatter,
                    "unknown provider instance `{binding}` for kind `{}`",
                    instance.provider(),
                ),
                None => write!(
                    formatter,
                    "unknown default provider instance for kind `{}`",
                    instance.provider(),
                ),
            },
            Self::SchemaOwnerMismatch { id, owner } => write!(
                formatter,
                "resource `{id}` routes to provider `{}` but its schema is owned by `{owner}`",
                id.provider,
            ),
            Self::ComparisonNotResolvedAtApplyStart { id } => write!(
                formatter,
                "comparison for `{id}` was not resolved before provider mutation began"
            ),
        }
    }
}

impl std::error::Error for ComparisonResolutionError {}

impl From<ComparisonResolutionError> for HostError {
    fn from(error: ComparisonResolutionError) -> Self {
        HostError::internal_caused_by(
            "Failed to resolve resource comparison",
            crate::provider_text::HostDiagnostic::capture_host_or_provider(error),
        )
    }
}

/// One owner schema paired with the initialized provider context that may
/// compare values under that schema.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedComparison<'a> {
    owner_schema: crate::schema::OwnerBoundResourceSchema<'a>,
    context: crate::differ::ComparisonSchema<'a>,
}

impl<'a> ResolvedComparison<'a> {
    pub fn owner_schema(self) -> crate::schema::OwnerBoundResourceSchema<'a> {
        self.owner_schema
    }

    pub fn context(&self) -> &crate::differ::ComparisonSchema<'a> {
        &self.context
    }
}

/// Resolve the schema and comparison context for a resource without exposing
/// provider CRUD. Runtime planning implements this with initialized providers;
/// metadata-only workflows use [`MetadataComparisonResolver`] so identity
/// reconciliation cannot accidentally initialize or dispatch a provider.
pub trait ResourceComparisonResolver {
    fn comparison_for<'a>(
        &'a self,
        registry: &'a SchemaRegistry,
        id: &ResourceId,
    ) -> Result<ResolvedComparison<'a>, ComparisonResolutionError>;
}

/// Comparison resolver for schema-only, provider-I/O-free workflows.
///
/// Provider routes are admitted explicitly from the finalized resource set.
/// Reference scope remains unavailable because obtaining account/region scope
/// may require provider initialization; ordinary typed and transformed values
/// still use the same production comparison implementation.
pub struct MetadataComparisonResolver {
    routes: HashSet<ProviderInstanceKey>,
    reference_scope: ReferenceScope,
}

impl MetadataComparisonResolver {
    pub fn new<'a>(ids: impl IntoIterator<Item = &'a ResourceId>) -> Self {
        Self {
            routes: ids.into_iter().map(ProviderInstanceKey::from).collect(),
            reference_scope: ReferenceScope::Unavailable,
        }
    }
}

impl ResourceComparisonResolver for MetadataComparisonResolver {
    fn comparison_for<'a>(
        &'a self,
        registry: &'a SchemaRegistry,
        id: &ResourceId,
    ) -> Result<ResolvedComparison<'a>, ComparisonResolutionError> {
        let schema = registry
            .get(
                &id.provider,
                &id.resource_type,
                crate::schema::SchemaKind::Resource,
            )
            .ok_or_else(|| ComparisonResolutionError::MissingResourceSchema { id: id.clone() })?;
        let route = ProviderInstanceKey::from(id);
        if !self.routes.contains(&route) {
            return Err(ComparisonResolutionError::UnknownProviderInstance { instance: route });
        }
        if route.provider() != schema.owner() {
            return Err(ComparisonResolutionError::SchemaOwnerMismatch {
                id: id.clone(),
                owner: schema.owner().to_string(),
            });
        }
        Ok(ResolvedComparison {
            owner_schema: schema,
            context: crate::differ::ComparisonSchema::from_scope_and_forms(
                &self.reference_scope,
                schema.reference_forms(),
            ),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResolvedComparisonKey {
    instance: ProviderInstanceKey,
    resource_type: String,
}

impl From<&ResourceId> for ResolvedComparisonKey {
    fn from(id: &ResourceId) -> Self {
        // A comparison is owned by the initialized provider instance and the
        // provider's schema for this resource type. Resource identity is not
        // part of either context and may still be pending at pre-resolution.
        Self {
            instance: ProviderInstanceKey::from(id),
            resource_type: id.resource_type.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedComparisonData {
    owner: crate::parser::SchemaTypeOwner,
    schema: ValidatedResourceSchema,
    reference_forms: crate::schema::ReferenceForms,
    assignability_edges: Vec<crate::schema::AssignabilityEdge>,
    scope: ReferenceScope,
}

/// Apply-start witness containing every comparison state writeback may need.
///
/// Its constructor resolves provider routing and schema ownership before any
/// provider mutation begins. Writeback can therefore borrow comparisons from
/// this table without performing another fallible route or schema lookup.
#[derive(Debug, Clone)]
pub struct ResolvedComparisonTable {
    entries: HashMap<ResolvedComparisonKey, ResolvedComparisonData>,
}

impl ResolvedComparisonTable {
    pub fn comparison_for(
        &self,
        id: &ResourceId,
    ) -> Result<ResolvedComparison<'_>, ComparisonResolutionError> {
        let key = ResolvedComparisonKey::from(id);
        let data = self.entries.get(&key).ok_or_else(|| {
            ComparisonResolutionError::ComparisonNotResolvedAtApplyStart { id: id.clone() }
        })?;
        let owner_schema = crate::schema::OwnerBoundResourceSchema::new(
            &data.owner,
            &data.schema,
            &data.reference_forms,
            &data.assignability_edges,
        );
        Ok(ResolvedComparison {
            owner_schema,
            context: crate::differ::ComparisonSchema::from_scope_and_forms(
                &data.scope,
                &data.reference_forms,
            ),
        })
    }
}

/// Build the complete schema/router pair used by comparison-focused tests.
///
/// Every requested resource type receives an empty schema when the fixture did
/// not declare one, and every provider route receives an unavailable-scope
/// initialized provider. Tests therefore exercise the production comparison
/// resolver without duplicating schema admission or route setup.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn comparison_fixture_for_test<'a>(
    registry: &SchemaRegistry,
    ids: impl IntoIterator<Item = &'a ResourceId>,
) -> (SchemaRegistry, ProviderRouter) {
    comparison_fixture_with_scope_for_test(registry, ids, ReferenceScope::Unavailable)
}

/// Build a comparison fixture with one explicit scope for every requested
/// route. Reference-form tests use this to exercise the production resolver
/// without constructing a dispatch-capable provider.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn comparison_fixture_with_scope_for_test<'a>(
    registry: &SchemaRegistry,
    ids: impl IntoIterator<Item = &'a ResourceId>,
    reference_scope: ReferenceScope,
) -> (SchemaRegistry, ProviderRouter) {
    let ids = ids.into_iter().cloned().collect::<Vec<_>>();
    let mut registry = registry.clone();
    let missing_schemas = ids
        .iter()
        .filter(|id| {
            registry
                .get(
                    &id.provider,
                    &id.resource_type,
                    crate::schema::SchemaKind::Resource,
                )
                .is_none()
        })
        .map(|id| (id.provider.clone(), id.resource_type.clone()))
        .collect::<BTreeSet<_>>();
    for (owner, resource_type) in missing_schemas {
        registry.insert(
            owner,
            PatternResourceSchema::new(resource_type)
                .validate()
                .expect("empty comparison fixture schema must validate"),
        );
    }

    let routes = ids
        .iter()
        .map(ProviderInstanceKey::from)
        .collect::<BTreeSet<_>>();
    let mut router = ProviderRouter::new();
    for route in routes {
        router.add_provider_instance(
            route,
            InitializedProvider::for_test(
                Box::new(ComparisonFixtureProvider),
                reference_scope.clone(),
            ),
        );
    }
    (registry, router)
}

#[cfg(any(test, feature = "test-utils"))]
struct ComparisonFixtureProvider;

#[cfg(any(test, feature = "test-utils"))]
impl Provider for ComparisonFixtureProvider {
    fn name(&self) -> &str {
        "comparison-fixture"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { panic!("comparison fixture must not dispatch read") })
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { panic!("comparison fixture must not dispatch data-source read") })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        _attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        Box::pin(async { panic!("comparison fixture must not dispatch create") })
    }

    fn resume_create(
        &self,
        _request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        Box::pin(async { panic!("comparison fixture must not dispatch resume-create") })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: UpdateRequest,
        _attempt: crate::provider::UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        Box::pin(async { panic!("comparison fixture must not dispatch update") })
    }

    fn delete(
        &self,
        _attempt: crate::provider::DeleteAttempt,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { panic!("comparison fixture must not dispatch delete") })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: PlanOp) -> ProviderResult<Vec<String>> {
        panic!("comparison fixture must not dispatch permission lookup")
    }
}

impl Default for ProviderRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRouter {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            normalizers: NormalizerRouter::new(),
        }
    }

    pub async fn resolve_references_for(
        &self,
        resource: &ResourceId,
        requests: &[crate::references::ReferenceRequest],
    ) -> HostResult<
        Vec<
            Result<
                crate::references::ReferenceOutcome<Bound>,
                crate::references::ReferenceError<Bound>,
            >,
        >,
    > {
        self.get_provider_or_error(resource)?
            .resolve_references(requests)
            .await
    }

    /// Register the kind's default instance (resources with
    /// `provider_instance = None` route here).
    pub fn add_provider(&mut self, kind: String, provider: InitializedProvider) {
        self.normalizers.normalizers.push(provider.provider.clone());
        self.providers
            .insert(ProviderInstanceKey::new(kind, None), provider);
    }

    /// Register a provider under the same route derived from its config by
    /// host-owned normalization.
    pub fn add_provider_config<
        P: crate::resource::PlaceholderKind,
        B: crate::resource::BindingAddress,
    >(
        &mut self,
        config: &ProviderConfig<P, B>,
        provider: InitializedProvider,
    ) {
        self.normalizers.normalizers.push(provider.provider.clone());
        self.providers
            .insert(ProviderInstanceKey::from(config), provider);
    }

    /// Register an already-initialized provider under an explicit instance
    /// route. Native hosts and test harnesses use this when no parsed provider
    /// block exists to carry the route.
    pub fn add_provider_instance(
        &mut self,
        key: ProviderInstanceKey,
        provider: InitializedProvider,
    ) {
        self.normalizers.normalizers.push(provider.provider.clone());
        self.providers.insert(key, provider);
    }

    pub fn add_initialized_normalizer(&mut self, normalizer: ConfiguredNormalizer) {
        self.normalizers.add_initialized_normalizer(normalizer);
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    fn get_initialized_provider_or_error(
        &self,
        id: &ResourceId,
    ) -> HostResult<&InitializedProvider> {
        let key = ProviderInstanceKey::from(id);
        self.providers.get(&key).ok_or_else(|| {
            HostError::internal(match &id.provider_instance {
                Some(binding) => format!(
                    "Unknown provider instance: {} (kind={})",
                    binding, id.provider
                ),
                None => format!("Unknown provider: {}", id.provider),
            })
        })
    }

    fn get_provider_or_error(&self, id: &ResourceId) -> HostResult<&dyn HostProvider> {
        self.get_initialized_provider_or_error(id)
            .map(InitializedProvider::provider)
    }

    /// Return the exact scope of every initialized provider instance.
    pub fn provider_scopes(&self) -> BTreeMap<ProviderInstanceKey, ReferenceScope> {
        self.providers
            .iter()
            .map(|(key, provider)| (key.clone(), provider.reference_scope().clone()))
            .collect()
    }

    /// Mint the comparison context for one resource and its owner-bound
    /// schema. Both the instance route and schema owner must agree.
    pub fn comparison_context_for<'a>(
        &'a self,
        id: &ResourceId,
        schema: crate::schema::OwnerBoundResourceSchema<'a>,
    ) -> HostResult<crate::differ::ComparisonSchema<'a>> {
        self.resolve_comparison(id, schema)
            .map(|resolved| resolved.context)
            .map_err(HostError::from)
    }

    /// Resolve the owner schema and initialized provider comparison context in
    /// one typed step.
    pub fn comparison_for<'a>(
        &'a self,
        registry: &'a SchemaRegistry,
        id: &ResourceId,
    ) -> Result<ResolvedComparison<'a>, ComparisonResolutionError> {
        let schema = registry
            .get(
                &id.provider,
                &id.resource_type,
                crate::schema::SchemaKind::Resource,
            )
            .ok_or_else(|| ComparisonResolutionError::MissingResourceSchema { id: id.clone() })?;
        self.resolve_comparison(id, schema)
    }

    /// Resolve every resource comparison required by one apply before the
    /// executor can mutate provider state.
    pub fn resolve_comparisons<'i>(
        &self,
        registry: &SchemaRegistry,
        ids: impl IntoIterator<Item = &'i ResourceId>,
    ) -> Result<ResolvedComparisonTable, ComparisonResolutionError> {
        let mut entries = HashMap::new();
        for id in ids {
            let resolved = self.comparison_for(registry, id)?;
            let owner_schema = resolved.owner_schema();
            let key = ResolvedComparisonKey::from(id);
            let provider = self.providers.get(&key.instance).ok_or_else(|| {
                ComparisonResolutionError::UnknownProviderInstance {
                    instance: key.instance.clone(),
                }
            })?;
            entries.insert(
                key,
                ResolvedComparisonData {
                    owner: owner_schema.schema_type_owner().clone(),
                    schema: owner_schema.schema().clone(),
                    reference_forms: owner_schema.reference_forms().clone(),
                    assignability_edges: owner_schema.assignability_edges().to_vec(),
                    scope: provider.reference_scope().clone(),
                },
            );
        }
        Ok(ResolvedComparisonTable { entries })
    }

    fn resolve_comparison<'a>(
        &'a self,
        id: &ResourceId,
        schema: crate::schema::OwnerBoundResourceSchema<'a>,
    ) -> Result<ResolvedComparison<'a>, ComparisonResolutionError> {
        let key = ProviderInstanceKey::from(id);
        if key.provider() != schema.owner() {
            return Err(ComparisonResolutionError::SchemaOwnerMismatch {
                id: id.clone(),
                owner: schema.owner().to_string(),
            });
        }
        let provider = self.providers.get(&key).ok_or_else(|| {
            ComparisonResolutionError::UnknownProviderInstance {
                instance: key.clone(),
            }
        })?;
        Ok(ResolvedComparison {
            owner_schema: schema,
            context: crate::differ::ComparisonSchema::from_initialized_provider(provider, schema),
        })
    }
}

impl ResourceComparisonResolver for ProviderRouter {
    fn comparison_for<'a>(
        &'a self,
        registry: &'a SchemaRegistry,
        id: &ResourceId,
    ) -> Result<ResolvedComparison<'a>, ComparisonResolutionError> {
        ProviderRouter::comparison_for(self, registry, id)
    }
}

impl Provider<Bound> for ProviderRouter {
    fn name(&self) -> &str {
        "router"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, HostResult<ReadOutcome>> {
        match self.get_provider_or_error(id) {
            Ok(provider) => provider.read(id, identifier, request),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, HostResult<State>> {
        match self.get_provider_or_error(&resource.id) {
            Ok(provider) => provider.read_data_source(resource),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<HostCreateOutcome, HostCreateFailure>> {
        match self.get_provider_or_error(id) {
            Ok(provider) => provider.create(id, request, attempt),
            Err(error) => {
                Box::pin(async move { Err(HostCreateFailure::not_created_at_dispatch(error)) })
            }
        }
    }

    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<HostResumeOutcome, HostResumeFailure>> {
        match self.get_provider_or_error(request.id()) {
            Ok(provider) => provider.resume_create(request),
            Err(error) => {
                let pending = request.into_in_flight();
                Box::pin(async move { Err(HostResumeFailure::in_flight(pending, error)) })
            }
        }
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        attempt: UpdateAttempt,
    ) -> BoxFuture<'_, HostResult<HostUpdateOutcome>> {
        match self.get_provider_or_error(id) {
            Ok(provider) => provider.update(id, identifier, request, attempt),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn delete(&self, attempt: crate::provider::DeleteAttempt) -> BoxFuture<'_, HostResult<()>> {
        match self.get_provider_or_error(attempt.id()) {
            Ok(provider) => provider.delete(attempt),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn required_permissions(
        &self,
        id: &ResourceId,
        op: PlanOp,
    ) -> HostResult<Vec<crate::provider_text::RequiredPermission>> {
        self.get_provider_or_error(id)?
            .required_permissions(id, op)
            .map_err(|error| error.for_provider(id.provider.clone()))
    }

    fn satisfier_hint(
        &self,
        target_id: &ResourceId,
        attr_path: &AttrPath,
    ) -> HostResult<Vec<BindingPattern>> {
        self.get_provider_or_error(target_id)?
            .satisfier_hint(target_id, attr_path)
            .map_err(|error| error.for_provider(target_id.provider.clone()))
    }
}

impl ProviderNormalizer<Bound> for ProviderRouter {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalizers.normalize_desired(resources)
    }

    fn normalize_dispatch_desired<'a>(
        &'a self,
        resources: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalizers.normalize_dispatch_desired(resources)
    }

    fn normalize_state<'a>(
        &'a self,
        current_states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalizers.normalize_state(current_states)
    }

    fn hydrate_read_state<'a>(
        &'a self,
        current_states: &'a mut StateTransformMap,
        saved_attrs: &'a SavedAttrs,
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalizers
            .hydrate_read_state(current_states, saved_attrs)
    }
}

/// Provider-facing text whose DSL spelling has already been resolved by the
/// host.
///
/// The inner string is private and there is no public constructor, so an
/// implementation of [`ProviderFactory`] can inspect API spelling but a caller
/// outside `carina-core` cannot manufacture this capability from raw DSL text.
///
/// External callers cannot bypass local schema validation through the private
/// canonicalization function (carina#4276). This checks the downstream privacy
/// boundary, where importing `canonicalize_custom_type_value` is an E0603 error.
/// The example references only that function: making it public would make the
/// example compile, without another private import masking the regression.
///
/// ```compile_fail,E0603
/// use carina_core::provider::canonicalize_custom_type_value;
///
/// let _ = canonicalize_custom_type_value;
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalCustomTypeValue(String);

impl CanonicalCustomTypeValue {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Produce provider-facing text from a literal at a non-enum custom-type
    /// position. Enum positions must resolve through [`Self::from_canonical_enum`].
    pub(crate) fn from_literal_string(value: &ValueString) -> Self {
        Self(value.expose_plaintext().to_string())
    }

    /// Produce provider-facing API spelling from a schema-resolved enum.
    pub(crate) fn from_canonical_enum(value: &CanonicalEnumValue) -> Self {
        Self(value.api_value().to_string())
    }
}

/// Typed failure while projecting one identified scalar value to the guest
/// validator's canonical text representation.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CustomTypeCanonicalizationError {
    #[error("provider custom types are not loaded")]
    ProvidersNotLoaded,
    #[error(
        "custom-type canonicalization requires an identified scalar declaration, got {type_name}"
    )]
    UnidentifiedDeclaration { type_name: String },
    #[error("custom type `{identity}` has no scalar bridge kind ({type_name})")]
    MissingScalarKind {
        identity: TypeIdentity,
        type_name: String,
    },
    #[error("integer {value} cannot be represented exactly as Float for custom type `{identity}`")]
    InexactFloatWidening { identity: TypeIdentity, value: i64 },
    #[error("could not canonicalize enum `{identity}`: {error}")]
    EnumResolution {
        identity: TypeIdentity,
        error: Box<crate::schema::TypeError>,
    },
    #[error("expected enum `{expected}`, got `{actual}`")]
    EnumIdentityMismatch {
        expected: Box<TypeIdentity>,
        actual: Box<TypeIdentity>,
    },
    #[error("custom type `{identity}` expects {expected}, got {actual}")]
    ConcreteKindMismatch {
        identity: TypeIdentity,
        expected: RefinedScalarKind,
        actual: &'static str,
    },
}

impl CustomTypeCanonicalizationError {
    pub(crate) fn into_type_error(self) -> crate::schema::TypeError {
        match self {
            Self::ProvidersNotLoaded => {
                crate::parser::CustomTypeValidationError::ProvidersNotLoaded.into()
            }
            Self::EnumResolution { error, .. } => *error,
            Self::ConcreteKindMismatch {
                identity,
                expected: _expected,
                actual,
            } => crate::schema::TypeError::value_type_mismatch(
                identity.to_string(),
                actual.to_string(),
            ),
            error @ (Self::UnidentifiedDeclaration { .. }
            | Self::MissingScalarKind { .. }
            | Self::InexactFloatWidening { .. }
            | Self::EnumIdentityMismatch { .. }) => crate::schema::TypeError::ValidationFailed {
                message: error.to_string().into(),
            },
        }
    }
}

/// Return the identity carried by an identified scalar or enum declaration.
pub fn identified_custom_type_identity(attribute_type: &AttributeType) -> Option<&TypeIdentity> {
    match attribute_type.raw_shape() {
        RawShape::UnloadedSchemaType(unloaded) => Some(unloaded.identity()),
        RawShape::Reference { .. } => None,
        RawShape::String {
            identity_layer: crate::schema::StringIdentityParts { identity, .. },
            ..
        }
        | RawShape::Int {
            identity_layer: crate::schema::IntIdentityParts { identity, .. },
            ..
        }
        | RawShape::Float { identity, .. } => identity,
        RawShape::Enum { identity, .. } => Some(identity),
        RawShape::Bool
        | RawShape::Duration
        | RawShape::List { .. }
        | RawShape::Map { .. }
        | RawShape::Struct { .. }
        | RawShape::Union(_)
        | RawShape::Ref(_) => None,
    }
}

/// Return the primitive bridge kind carried by an identified declaration.
/// Enums use their preserved protocol base rather than being treated as
/// strings by convention.
pub fn identified_custom_type_scalar_kind(
    attribute_type: &AttributeType,
) -> Result<Option<RefinedScalarKind>, crate::parser::UnloadedSchemaType> {
    Ok(match attribute_type.raw_shape() {
        RawShape::UnloadedSchemaType(unloaded) => return Err(unloaded.clone()),
        RawShape::Reference {
            identity: _identity,
            pattern: _pattern,
            length: _length,
        } => Some(RefinedScalarKind::String),
        RawShape::String {
            identity_layer:
                crate::schema::StringIdentityParts {
                    identity: Some(_), ..
                },
            ..
        } => Some(RefinedScalarKind::String),
        RawShape::Int {
            identity_layer:
                crate::schema::IntIdentityParts {
                    identity: Some(_), ..
                },
            ..
        } => Some(RefinedScalarKind::Int),
        RawShape::Float {
            identity: Some(_), ..
        } => Some(RefinedScalarKind::Float),
        RawShape::Enum { base, .. } => scalar_kind_from_base(base),
        RawShape::String {
            identity_layer: crate::schema::StringIdentityParts { identity: None, .. },
            ..
        }
        | RawShape::Int {
            identity_layer: crate::schema::IntIdentityParts { identity: None, .. },
            ..
        }
        | RawShape::Float { identity: None, .. }
        | RawShape::Bool
        | RawShape::Duration
        | RawShape::List { .. }
        | RawShape::Map { .. }
        | RawShape::Struct { .. }
        | RawShape::Union(_)
        | RawShape::Ref(_) => None,
    })
}

fn scalar_kind_from_base(attribute_type: &AttributeType) -> Option<RefinedScalarKind> {
    match attribute_type.raw_shape() {
        RawShape::UnloadedSchemaType(_) => None,
        RawShape::Reference {
            identity: _identity,
            pattern: _pattern,
            length: _length,
        } => Some(RefinedScalarKind::String),
        RawShape::String { .. } => Some(RefinedScalarKind::String),
        RawShape::Int { .. } => Some(RefinedScalarKind::Int),
        RawShape::Float { .. } => Some(RefinedScalarKind::Float),
        RawShape::Enum { base, .. } => scalar_kind_from_base(base),
        RawShape::Bool
        | RawShape::Duration
        | RawShape::List { .. }
        | RawShape::Map { .. }
        | RawShape::Struct { .. }
        | RawShape::Union(_)
        | RawShape::Ref(_) => None,
    }
}

/// Canonicalize a value using its identified scalar/enum declaration.
///
/// Requiring the declaration makes unresolved enum identifiers impossible to
/// project by convention: enum spelling always resolves through the exact
/// owning schema type, while refined strings may still accept their raw
/// identifier-shaped DSL literal.
pub(crate) fn canonicalize_custom_type_value(
    declared_type: &AttributeType,
    value: &Value,
) -> Result<Option<CanonicalCustomTypeValue>, CustomTypeCanonicalizationError> {
    match declared_type.raw_shape() {
        RawShape::UnloadedSchemaType(_) => {
            return Err(CustomTypeCanonicalizationError::ProvidersNotLoaded);
        }
        RawShape::Reference { .. }
        | RawShape::String { .. }
        | RawShape::Int { .. }
        | RawShape::Float { .. }
        | RawShape::Bool
        | RawShape::Duration
        | RawShape::Enum { .. }
        | RawShape::List { .. }
        | RawShape::Map { .. }
        | RawShape::Struct { .. }
        | RawShape::Union(_)
        | RawShape::Ref(_) => {}
    }
    let identity = identified_custom_type_identity(declared_type).ok_or_else(|| {
        CustomTypeCanonicalizationError::UnidentifiedDeclaration {
            type_name: declared_type.type_name(),
        }
    })?;
    let scalar_kind = identified_custom_type_scalar_kind(declared_type)
        .map_err(|_| CustomTypeCanonicalizationError::ProvidersNotLoaded)?
        .ok_or_else(|| CustomTypeCanonicalizationError::MissingScalarKind {
            identity: identity.clone(),
            type_name: declared_type.type_name(),
        })?;
    match (declared_type.raw_shape(), value) {
        (RawShape::UnloadedSchemaType(_), _) => {
            Err(CustomTypeCanonicalizationError::ProvidersNotLoaded)
        }
        (
            RawShape::String {
                identity_layer:
                    crate::schema::StringIdentityParts {
                        identity: _,
                        pattern: _,
                        length: _,
                        validate: _,
                        to_dsl: _,
                    },
                member_layer: _,
            },
            Value::Concrete(ConcreteValue::String(value)),
        ) => Ok(Some(CanonicalCustomTypeValue::from_literal_string(value))),
        (
            RawShape::String {
                identity_layer:
                    crate::schema::StringIdentityParts {
                        identity: _,
                        pattern: _,
                        length: _,
                        validate: _,
                        to_dsl: _,
                    },
                member_layer: _,
            },
            Value::Concrete(ConcreteValue::EnumIdentifier(raw)),
        ) => Ok(Some(CanonicalCustomTypeValue(raw.as_str().to_string()))),
        (
            RawShape::Int {
                identity_layer:
                    crate::schema::IntIdentityParts {
                        identity: _,
                        range: _,
                        validate: _,
                    },
                member_layer: _,
            },
            Value::Concrete(ConcreteValue::Int(value)),
        ) => Ok(Some(CanonicalCustomTypeValue(value.to_string()))),
        (
            RawShape::Float {
                identity: _,
                range: _,
                validate: _,
            },
            Value::Concrete(ConcreteValue::Int(value)),
        ) => {
            let float = crate::numeric::int_to_exact_f64(*value).ok_or_else(|| {
                CustomTypeCanonicalizationError::InexactFloatWidening {
                    identity: identity.clone(),
                    value: *value,
                }
            })?;
            canonicalize_custom_type_value(
                declared_type,
                &Value::Concrete(ConcreteValue::Float(float)),
            )
        }
        (
            RawShape::Float {
                identity: _,
                range: _,
                validate: _,
            },
            Value::Concrete(ConcreteValue::Float(value)),
        ) => Ok(Some(CanonicalCustomTypeValue(value.to_string()))),
        (
            RawShape::Enum {
                identity: _,
                base: _,
                values: _,
                dsl_aliases: _,
                validate: _,
                to_dsl: _,
            },
            Value::Concrete(ConcreteValue::String(value)),
        ) => {
            let canonical = crate::resource::EnumValueResolver::new(declared_type)
                .resolve_state_text(value.expose_plaintext())
                .map_err(|error| CustomTypeCanonicalizationError::EnumResolution {
                    identity: identity.clone(),
                    error: Box::new(error),
                })?;
            if !canonical.identity().same_type(identity) {
                return Err(CustomTypeCanonicalizationError::EnumIdentityMismatch {
                    expected: Box::new(identity.clone()),
                    actual: Box::new(canonical.identity().clone()),
                });
            }
            Ok(Some(CanonicalCustomTypeValue::from_canonical_enum(
                &canonical,
            )))
        }
        (
            RawShape::Enum {
                identity: _,
                base: _,
                values: _,
                dsl_aliases: _,
                validate: _,
                to_dsl: _,
            },
            Value::Concrete(ConcreteValue::CanonicalEnum(value)),
        ) => {
            if !value.identity().same_type(identity) {
                return Err(CustomTypeCanonicalizationError::EnumIdentityMismatch {
                    expected: Box::new(identity.clone()),
                    actual: Box::new(value.identity().clone()),
                });
            }
            Ok(Some(CanonicalCustomTypeValue::from_canonical_enum(value)))
        }
        (
            RawShape::Enum {
                identity: _,
                base: _,
                values: _,
                dsl_aliases: _,
                validate: _,
                to_dsl: _,
            },
            Value::Concrete(ConcreteValue::EnumIdentifier(raw)),
        ) => {
            let canonical = crate::resource::EnumValueResolver::new(declared_type)
                .resolve_raw(raw)
                .map_err(|error| CustomTypeCanonicalizationError::EnumResolution {
                    identity: identity.clone(),
                    error: Box::new(error),
                })?;
            if !canonical.identity().same_type(identity) {
                return Err(CustomTypeCanonicalizationError::EnumIdentityMismatch {
                    expected: Box::new(identity.clone()),
                    actual: Box::new(canonical.identity().clone()),
                });
            }
            Ok(Some(CanonicalCustomTypeValue::from_canonical_enum(
                &canonical,
            )))
        }
        (
            RawShape::Reference { .. }
            | RawShape::String { .. }
            | RawShape::Int { .. }
            | RawShape::Float { .. }
            | RawShape::Enum { .. },
            Value::Deferred(_),
        ) => Ok(None),
        (
            RawShape::Reference { .. }
            | RawShape::String { .. }
            | RawShape::Int { .. }
            | RawShape::Float { .. }
            | RawShape::Enum { .. },
            Value::Concrete(value),
        ) => Err(CustomTypeCanonicalizationError::ConcreteKindMismatch {
            identity: identity.clone(),
            expected: scalar_kind,
            actual: custom_type_concrete_value_kind(value),
        }),
        (
            RawShape::Bool
            | RawShape::Duration
            | RawShape::List { .. }
            | RawShape::Map { .. }
            | RawShape::Struct { .. }
            | RawShape::Union(_)
            | RawShape::Ref(_),
            _,
        ) => unreachable!("identified scalar guard excludes non-scalar declarations"),
    }
}

fn custom_type_concrete_value_kind(value: &ConcreteValue) -> &'static str {
    match value {
        ConcreteValue::String(_) => "string",
        ConcreteValue::EnumIdentifier(_) => "enum identifier",
        ConcreteValue::CanonicalEnum(_) => "canonical enum",
        ConcreteValue::Int(_) => "integer",
        ConcreteValue::Float(_) => "float",
        ConcreteValue::Bool(_) => "boolean",
        ConcreteValue::Duration(_) => "duration",
        ConcreteValue::List(_) | ConcreteValue::StringList(_) => "list",
        ConcreteValue::Map(_) => "map",
    }
}

impl std::ops::Deref for CanonicalCustomTypeValue {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for CanonicalCustomTypeValue {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for CanonicalCustomTypeValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Factory for creating and configuring a Provider.
///
/// Each provider crate implements this trait to encapsulate provider-specific
/// logic (configuration validation, provider instantiation, schemas).
/// The CLI uses factories instead of hardcoded provider name matching.
pub trait ProviderFactory: Send + Sync {
    /// Provider name (e.g., "aws", "awscc")
    fn name(&self) -> &str;

    /// Display name for user-facing messages (e.g., "AWS provider", "AWS Cloud Control provider")
    fn display_name(&self) -> &str;

    /// Return the validated schema witness for the provider block's
    /// configuration attributes (e.g., `region`).
    ///
    /// These are used by the host to validate provider config attributes
    /// against their effective path types *before* calling `validate_config`.
    /// Keeping format validation (namespace structure, enum membership) on
    /// the host side means fixes to generic validation logic in
    /// `carina-core` take effect without rebuilding provider binaries.
    fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema;

    /// Validate provider-specific configuration semantics that cannot be
    /// expressed in the attribute type schema (e.g., cross-attribute
    /// consistency checks). Type-level validation is handled by the host
    /// using [`provider_config_attribute_types`] before this is called.
    ///
    /// A raw attribute map cannot be passed to this boundary:
    ///
    /// ```compile_fail
    /// use carina_core::provider::ProviderFactory;
    /// use carina_core::resource::Value;
    /// use indexmap::IndexMap;
    ///
    /// fn bypass(factory: &dyn ProviderFactory, raw: &IndexMap<String, Value>) {
    ///     factory.validate_config(raw).unwrap();
    /// }
    /// ```
    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        _session: &crate::provider::ProviderValidationInputs,
    ) -> ProviderResult<()>;

    /// Validate a value against a provider-defined custom type after local
    /// schema constraints have run. The native default adds no extra
    /// factory-side constraint; host-backed factories override this and the
    /// SDK guest rejects identities absent from its schema-derived table.
    /// Failures retain the provider's structured [`ProviderError`].
    ///
    /// `identity` is the structured [`TypeIdentity`] of the type, so a
    /// provider resolves the exact provider-scoped type instead of
    /// splitting a flat name string. Raw DSL text cannot be passed to this
    /// boundary:
    ///
    /// ```compile_fail
    /// use carina_core::provider::ProviderFactory;
    /// use carina_core::schema::TypeIdentity;
    ///
    /// fn bypass(factory: &dyn ProviderFactory) {
    ///     factory
    ///         .validate_custom_type(
    ///             &TypeIdentity::bare("Mode"),
    ///             "raw.dsl.text",
    ///         )
    ///         .unwrap();
    /// }
    /// ```
    fn validate_custom_type(
        &self,
        _identity: &TypeIdentity,
        _value: &CanonicalCustomTypeValue,
        _session: &crate::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }

    /// Create a provider instance from configuration attributes.
    ///
    /// `binding` is the `let <name> = provider <kind> { ... }` binding
    /// name when this call instantiates a named instance, or `None`
    /// when it instantiates the kind's default instance. The host
    /// uses it as a cache key so multiple named instances of the same
    /// kind do not collapse onto a single shared WASM instance.
    /// Provider implementations are free to ignore it.
    ///
    /// Returns `Err(ProviderError)` when the provider rejects the
    /// supplied configuration (e.g., an `allowed_account_ids` mismatch
    /// detected during `init`). Callers MUST surface the inner message
    /// through the boundary, which redacts provider-derived text. The sealed
    /// [`ProviderReadyConfig`] proves that resolution, host-side schema
    /// validation, and [`ProviderFactory::validate_config`] already ran.
    fn create_provider(
        &self,
        config: &crate::provider::ProviderSession,
        initialize_outcome: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>>;

    /// Create a normalizer instance from configuration attributes.
    ///
    /// `binding` semantics match [`create_provider`]: `Some(name)` for
    /// a named instance, `None` for the kind's default. Returns an explicit
    /// [`NoopNormalizer`] by default. Provider and boundary failures must be
    /// returned rather than replaced with a no-op implementation. The same
    /// checked configuration witness is required as for provider creation.
    fn create_normalizer(
        &self,
        _config: &crate::provider::ProviderSession,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }

    /// Get all resource schemas for this provider.
    fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema>;

    /// Return the provider-level aggregate schema witness.
    ///
    /// Registry ownership, custom-type recognition, and reference forms must
    /// all be derived from this one snapshot rather than reloading the three
    /// declaration sources independently.
    fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness;

    /// Attribute names (beyond schema create-only properties) that contribute
    /// to anonymous resource identity. For example, AWS providers return
    /// `["region"]` because the same resource type in different regions must
    /// produce different identifiers.
    fn identity_attributes(&self) -> Vec<&str> {
        vec![]
    }

    /// Config attribute completions for this provider.
    /// Returns a map of attribute name → completion candidates.
    /// For example, an AWS provider returns `{"region": [CompletionValue { value: "aws.Region.ap_northeast_1", ... }]}`.
    fn config_completions(
        &self,
    ) -> std::collections::HashMap<String, Vec<crate::schema::CompletionValue>> {
        std::collections::HashMap::new()
    }
}

/// Find a factory by provider name.
pub fn find_factory<'a>(
    factories: &'a [Box<dyn ProviderFactory>],
    name: &str,
) -> Option<&'a dyn ProviderFactory> {
    factories
        .iter()
        .find(|f| f.name() == name)
        .map(|f| f.as_ref())
}

/// A schema registry constructed from a completed provider-factory load.
///
/// The private field and absence of `Default`/raw-registry conversions prevent
/// bootstrap registries from being relabeled as provider-loaded. Supplemental
/// schemas (for example, the built-in mock provider) are admitted only while
/// this proof is minted from the loaded factory collection.
#[derive(Debug, Clone)]
pub struct ProviderLoadedSchemaRegistry {
    schemas: std::sync::Arc<SchemaRegistry>,
}

impl ProviderLoadedSchemaRegistry {
    fn from_registry(schemas: SchemaRegistry) -> Self {
        Self {
            schemas: std::sync::Arc::new(schemas),
        }
    }

    /// Borrow the provider-loaded registry.
    pub fn schemas(&self) -> &SchemaRegistry {
        &self.schemas
    }

    /// Share the exact registry carried by this proof with read-only consumers.
    pub fn schemas_arc(&self) -> std::sync::Arc<SchemaRegistry> {
        std::sync::Arc::clone(&self.schemas)
    }

    /// Erase the provider-loaded proof for consumers that need only a registry.
    pub fn into_registry(self) -> SchemaRegistry {
        std::sync::Arc::try_unwrap(self.schemas).unwrap_or_else(|shared| shared.as_ref().clone())
    }

    /// Construct provider-loaded schema evidence for cross-crate fixtures.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(schemas: SchemaRegistry) -> Self {
        Self::from_registry(schemas)
    }
}

/// Provider factories and both proofs minted from one captured load.
///
/// Production consumers retain this bundle so schema evidence and the loaded
/// custom-type context cannot be supplied from separate construction paths.
pub struct LoadedProviderSnapshot {
    factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>>,
    schemas: ProviderLoadedSchemaRegistry,
    provider_context: crate::parser::LoadedProviderContext,
}

impl LoadedProviderSnapshot {
    pub fn factories(&self) -> &[Box<dyn ProviderFactory>] {
        &self.factories
    }

    pub fn factories_arc(&self) -> std::sync::Arc<Vec<Box<dyn ProviderFactory>>> {
        std::sync::Arc::clone(&self.factories)
    }

    pub fn schemas(&self) -> &ProviderLoadedSchemaRegistry {
        &self.schemas
    }

    pub fn schema_registry(&self) -> &SchemaRegistry {
        self.schemas.schemas()
    }

    pub fn provider_context(&self) -> &crate::parser::LoadedProviderContext {
        &self.provider_context
    }

    /// Build a deliberately synthetic loaded snapshot for cross-crate tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(
        schemas: SchemaRegistry,
        factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>>,
    ) -> Self {
        let provider_context =
            crate::parser::LoadedProviderContext::new(crate::parser::ProviderContext {
                environment: crate::EnvironmentSnapshot::empty(),
                decryptor: None,
                custom_types: provider_custom_types_from_factories(std::sync::Arc::clone(
                    &factories,
                )),
                resource_types: crate::parser::ProviderContext::resource_types_from_schema_registry(
                    &schemas,
                ),
            });
        Self {
            factories,
            schemas: ProviderLoadedSchemaRegistry::from_registry(schemas),
            provider_context,
        }
    }
}

/// Collect all resource schemas from the given factories into a [`SchemaRegistry`].
pub fn collect_schemas(factories: &[Box<dyn ProviderFactory>]) -> SchemaRegistry {
    let mut registry = SchemaRegistry::new();
    for factory in factories {
        let witness = factory.schema_witness();
        registry
            .insert_provider_witness(factory.name(), &witness)
            .expect("each schema owner must expose one consistent validated witness");
    }
    registry
}

/// Collect custom type identities from a registry for completion display.
pub fn collect_custom_type_names(registry: &SchemaRegistry) -> Vec<TypeIdentity> {
    let mut names = BTreeSet::new();

    for (_provider, _resource_type, _kind, schema) in registry.iter() {
        for attribute in schema.effective_attributes() {
            collect_type_names_from_type(attribute.field_type(), &mut names);
        }
        // Definitions are visited once; `Ref` edges are not followed, so
        // cyclic generated schemas terminate.
        for def in schema.parts().defs.values() {
            collect_type_names_from_type(def, &mut names);
        }
    }

    names.into_iter().collect()
}

/// Recursively collect Custom type identities from an AttributeType
/// (identities only, no closures).
fn collect_type_names_from_type(
    attr_type: &crate::schema::CompiledAttributeType,
    names: &mut BTreeSet<TypeIdentity>,
) {
    use crate::schema::RawShape;

    match attr_type.raw_shape() {
        RawShape::UnloadedSchemaType(_) => {}
        RawShape::Reference {
            identity,
            pattern: _pattern,
            length: _length,
        } => {
            names.insert(identity.clone());
        }
        RawShape::String {
            identity_layer:
                crate::schema::StringIdentityParts {
                    identity: Some(id), ..
                },
            ..
        }
        | RawShape::Int {
            identity_layer:
                crate::schema::IntIdentityParts {
                    identity: Some(id), ..
                },
            ..
        }
        | RawShape::Float {
            identity: Some(id), ..
        }
        | RawShape::Enum {
            identity: id,
            base: _,
            values: _,
            dsl_aliases: _,
            validate: _,
            to_dsl: _,
        } => {
            names.insert(id.clone());
        }
        RawShape::String {
            identity_layer: crate::schema::StringIdentityParts { identity: None, .. },
            ..
        }
        | RawShape::Int {
            identity_layer: crate::schema::IntIdentityParts { identity: None, .. },
            ..
        }
        | RawShape::Float { identity: None, .. } => {}
        RawShape::List {
            element_type: inner,
            ..
        } => {
            collect_type_names_from_type(inner, names);
        }
        RawShape::Map {
            key,
            value: inner,
            constraints: _,
        } => {
            collect_type_names_from_type(key, names);
            collect_type_names_from_type(inner, names);
        }
        RawShape::Struct { fields, .. } => {
            for field in fields {
                collect_type_names_from_type(&field.field_type, names);
            }
        }
        RawShape::Union(types) => {
            for t in types {
                collect_type_names_from_type(t, names);
            }
        }
        // The caller walks `schema.defs` separately to avoid infinite
        // recursion on cyclic schemas (carina#3340).
        RawShape::Ref(_) => {}
        RawShape::Bool | RawShape::Duration => {}
    }
}

fn local_custom_validator(
    attribute_type: &AttributeType,
) -> Option<&crate::schema::CustomValidator> {
    match attribute_type.raw_shape() {
        RawShape::UnloadedSchemaType(_) => None,
        RawShape::Reference {
            identity: _identity,
            pattern: _pattern,
            length: _length,
        } => None,
        RawShape::String {
            identity_layer: crate::schema::StringIdentityParts { validate, .. },
            ..
        }
        | RawShape::Int {
            identity_layer: crate::schema::IntIdentityParts { validate, .. },
            ..
        }
        | RawShape::Float { validate, .. }
        | RawShape::Enum { validate, .. } => validate,
        RawShape::Bool
        | RawShape::Duration
        | RawShape::List { .. }
        | RawShape::Map { .. }
        | RawShape::Struct { .. }
        | RawShape::Union(_)
        | RawShape::Ref(_) => None,
    }
}

#[derive(Debug)]
pub enum CustomTypeFactoryValidationError {
    InvalidValue(crate::schema::TypeError),
    ProviderRejected(HostError),
}

impl fmt::Display for CustomTypeFactoryValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidValue(error) => error.fmt(formatter),
            Self::ProviderRejected(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CustomTypeFactoryValidationError {}

/// Validate the exact identified declaration supplied by a schema walk.
///
/// This function deliberately does not search the factory's other schemas.
/// Sibling declarations can share an identity while carrying different local
/// constraints; only an authored annotation is conjunctive across declarations.
pub fn validate_custom_type_with_factory(
    factory: &dyn ProviderFactory,
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'_>,
    value: &crate::resource::knowledge::KnownValue<'_>,
    host: &ProviderValidationInputs,
) -> Result<(), CustomTypeFactoryValidationError> {
    crate::schema::validate_custom_type_leaf(declaration, value, |proof| {
        proof.map_or(Ok(()), |proof| {
            dispatch_validated_custom_type(factory, proof, host)
        })
    })
    .map_err(CustomTypeFactoryValidationError::InvalidValue)?
}

fn dispatch_validated_custom_type(
    factory: &dyn ProviderFactory,
    proof: crate::schema::LocallyValidatedCustomType<'_>,
    host: &ProviderValidationInputs,
) -> Result<(), CustomTypeFactoryValidationError> {
    let session = ProviderValidationInputs::collect(factory.name(), [proof.value()])
        .inherit_diagnostic_host(host);
    session
        .diagnostics()
        .dispatch_sync(|| {
            factory.validate_custom_type(
                proof.declaration().identity(),
                proof.canonical(),
                &session,
            )
        })
        .map_err(|error| {
            CustomTypeFactoryValidationError::ProviderRejected(
                error.with_secrets(session.redaction_set()),
            )
        })
}

pub(crate) fn validate_validated_custom_type_with_factory(
    factory: &dyn ProviderFactory,
    proof: crate::schema::LocallyValidatedCustomType<'_>,
    host: &ProviderValidationInputs,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    let identity = proof.declaration().identity().clone();
    dispatch_validated_custom_type(factory, proof, host)
        .map_err(|error| owner_validation_error(factory.name(), &identity, error))
}

fn owner_validation_error(
    owner: &str,
    identity: &TypeIdentity,
    error: CustomTypeFactoryValidationError,
) -> crate::parser::CustomTypeValidationError {
    match error {
        CustomTypeFactoryValidationError::InvalidValue(error) => {
            crate::parser::CustomTypeValidationError::InvalidDeclarationValue {
                owner: owner.to_string(),
                identity: identity.clone(),
                error: Box::new(error),
            }
        }
        CustomTypeFactoryValidationError::ProviderRejected(error) => {
            let error = if error.detail().provider_name.is_some() {
                error
            } else {
                error.for_provider(owner)
            };
            crate::parser::CustomTypeValidationError::ProviderRejected {
                owner: owner.to_string(),
                identity: identity.clone(),
                error: std::sync::Arc::new(error),
                masked: false,
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn validate_custom_type_declaration_with_factory(
    factory: &dyn ProviderFactory,
    declaration: crate::parser::IdentifiedCustomTypeDeclaration<'_>,
    value: &Value,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    let owner = factory.name();
    let known = value
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .and_then(|proof| proof.into_known())
        .map_err(|error| {
            owner_validation_error(
                owner,
                declaration.identity(),
                CustomTypeFactoryValidationError::InvalidValue(error.into()),
            )
        })?;
    validate_custom_type_with_factory(
        factory,
        declaration,
        &known,
        &ProviderValidationInputs::collect(factory.name(), [value]),
    )
    .map_err(|error| owner_validation_error(owner, declaration.identity(), error))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomTypeDispatchTableError {
    MissingScalarKind {
        identity: TypeIdentity,
    },
    ConflictingScalarKinds {
        identity: TypeIdentity,
        first_kind: RefinedScalarKind,
        second_kind: RefinedScalarKind,
    },
}

impl fmt::Display for CustomTypeDispatchTableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingScalarKind { identity } => {
                write!(
                    formatter,
                    "custom type `{identity}` has no scalar bridge kind"
                )
            }
            Self::ConflictingScalarKinds {
                identity,
                first_kind,
                second_kind,
            } => write!(
                formatter,
                "custom type `{identity}` is bound to both {first_kind} and {second_kind} within one provider"
            ),
        }
    }
}

impl std::error::Error for CustomTypeDispatchTableError {}

struct CustomTypeDispatchBinding {
    scalar_kind: RefinedScalarKind,
    callbacks: Vec<crate::schema::CustomValidator>,
}

/// Precomputed SDK guest validator dispatch keyed by semantic identity.
///
/// Each identity owns its scalar kind. Callers supply only the canonical wire
/// text, so they cannot invoke a validator with a mismatched scalar shape.
pub struct CustomTypeDispatchTable {
    bindings: HashMap<TypeIdentity, CustomTypeDispatchBinding>,
}

impl fmt::Debug for CustomTypeDispatchTable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CustomTypeDispatchTable")
            .field("binding_count", &self.bindings.len())
            .finish()
    }
}

impl CustomTypeDispatchTable {
    /// Build guest dispatch from an aggregate provider-schema witness.
    ///
    /// Scalar-kind conflicts and missing bridge kinds cannot reach this seam:
    /// they are rejected before the witness can be constructed.
    pub fn from_provider_schema(witness: &ValidatedProviderSchemaWitness) -> Self {
        let mut bindings = HashMap::<TypeIdentity, CustomTypeDispatchBinding>::new();
        for (identity, declaration) in witness.identified_declarations() {
            let kind = identified_custom_type_scalar_kind(&declaration)
                .expect("validated declarations have loaded scalar kinds")
                .expect("validated provider schema guarantees a scalar bridge kind");
            let callback = local_custom_validator(&declaration).cloned();
            match bindings.entry(identity) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(CustomTypeDispatchBinding {
                        scalar_kind: kind,
                        callbacks: callback.into_iter().collect(),
                    });
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let binding = entry.get_mut();
                    assert_eq!(
                        binding.scalar_kind, kind,
                        "validated provider schema guarantees one scalar kind per identity"
                    );
                    if let Some(callback) = callback
                        && !binding
                            .callbacks
                            .iter()
                            .any(|existing| std::sync::Arc::ptr_eq(existing, &callback))
                    {
                        binding.callbacks.push(callback);
                    }
                }
            }
        }
        Self { bindings }
    }

    pub fn build(
        schemas: &[ValidatedResourceSchema],
        config: &ValidatedConfigSchema,
    ) -> Result<Self, CustomTypeDispatchTableError> {
        let mut bindings = HashMap::<TypeIdentity, CustomTypeDispatchBinding>::new();
        for (identity, declaration) in
            crate::schema::identified_declarations_from_witnesses(schemas, config)
        {
            let Ok(Some(kind)) = identified_custom_type_scalar_kind(&declaration) else {
                return Err(CustomTypeDispatchTableError::MissingScalarKind { identity });
            };
            let callback = local_custom_validator(&declaration).cloned();
            match bindings.entry(identity.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(CustomTypeDispatchBinding {
                        scalar_kind: kind,
                        callbacks: callback.into_iter().collect(),
                    });
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let binding = entry.get_mut();
                    if binding.scalar_kind != kind {
                        return Err(CustomTypeDispatchTableError::ConflictingScalarKinds {
                            identity,
                            first_kind: binding.scalar_kind,
                            second_kind: kind,
                        });
                    }
                    if let Some(callback) = callback
                        && !binding
                            .callbacks
                            .iter()
                            .any(|existing| std::sync::Arc::ptr_eq(existing, &callback))
                    {
                        binding.callbacks.push(callback);
                    }
                }
            }
        }
        Ok(Self { bindings })
    }

    pub fn dispatch(
        &self,
        identity: &TypeIdentity,
        canonical_text: &str,
    ) -> Result<(), CustomTypeDispatchError> {
        let binding = self
            .bindings
            .get(identity)
            .ok_or_else(|| CustomTypeDispatchError::Undeclared(identity.clone()))?;
        let value = bridge_text_to_typed_value(binding.scalar_kind, canonical_text.into())?;
        for callback in &binding.callbacks {
            callback(&value)
                .map_err(|error| CustomTypeDispatchError::Validation(Box::new(error)))?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum CustomTypeBridgeDecodeError {
    Integer(std::num::ParseIntError),
    FloatSyntax(std::num::ParseFloatError),
    Float(crate::NonFiniteFloatError),
}

impl fmt::Display for CustomTypeBridgeDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(_) => f.write_str("invalid integer bridge value"),
            Self::FloatSyntax(_) | Self::Float(_) => f.write_str("invalid float bridge value"),
        }
    }
}

impl std::error::Error for CustomTypeBridgeDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Integer(source) => Some(source),
            Self::FloatSyntax(source) => Some(source),
            Self::Float(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum CustomTypeDispatchError {
    Undeclared(TypeIdentity),
    Decode(CustomTypeBridgeDecodeError),
    Validation(Box<crate::schema::TypeError>),
}

impl fmt::Display for CustomTypeDispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undeclared(identity) => write!(
                f,
                "custom type `{identity}` is not declared by this provider"
            ),
            Self::Decode(_) => f.write_str("failed to decode custom type bridge value"),
            Self::Validation(error) => match error.as_ref() {
                crate::schema::TypeError::ValidationFailed { message } => message.fmt(f),
                error => error.fmt(f),
            },
        }
    }
}

impl std::error::Error for CustomTypeDispatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Undeclared(_) => None,
            Self::Validation(source) => Some(source.as_ref()),
            Self::Decode(source) => Some(source),
        }
    }
}

impl From<CustomTypeBridgeDecodeError> for CustomTypeDispatchError {
    fn from(source: CustomTypeBridgeDecodeError) -> Self {
        Self::Decode(source)
    }
}

pub(crate) fn bridge_text_to_typed_value(
    scalar_kind: RefinedScalarKind,
    canonical_text: ValueString,
) -> Result<Value, CustomTypeBridgeDecodeError> {
    match scalar_kind {
        RefinedScalarKind::String => Ok(Value::Concrete(ConcreteValue::String(canonical_text))),
        RefinedScalarKind::Int => Ok(Value::Concrete(ConcreteValue::Int(
            canonical_text
                .expose_plaintext()
                .parse::<i64>()
                .map_err(CustomTypeBridgeDecodeError::Integer)?,
        ))),
        RefinedScalarKind::Float => {
            let value = canonical_text
                .expose_plaintext()
                .parse::<f64>()
                .map_err(CustomTypeBridgeDecodeError::FloatSyntax)?;
            let value = crate::FiniteF64::new(value).map_err(CustomTypeBridgeDecodeError::Float)?;
            Ok(Value::Concrete(ConcreteValue::Float(value)))
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct EnumDefinitionSignature {
    values: Option<Vec<String>>,
    aliases: Vec<(String, String)>,
}

fn enum_definition_signature(attribute_type: &AttributeType) -> Option<EnumDefinitionSignature> {
    let RawShape::Enum {
        identity: _,
        base: _,
        values,
        dsl_aliases,
        validate: _,
        to_dsl: _,
    } = attribute_type.raw_shape()
    else {
        return None;
    };
    let mut values = values.map(<[String]>::to_vec);
    if let Some(values) = &mut values {
        values.sort();
    }
    let mut aliases = dsl_aliases.to_vec();
    aliases.sort();
    Some(EnumDefinitionSignature { values, aliases })
}

struct OwnerCustomTypeIndex {
    factory_index: usize,
    declarations: BTreeMap<TypeIdentity, Vec<AttributeType>>,
    identity_declarations: BTreeMap<
        TypeIdentity,
        Result<crate::parser::ResolvedSchemaType, crate::parser::CustomTypeValidationError>,
    >,
}

struct ProviderFactorySchemaSnapshot {
    name: String,
    witness: ValidatedProviderSchemaWitness,
}

fn capture_provider_factory_schema_snapshot(
    factories: &[Box<dyn ProviderFactory>],
) -> Vec<ProviderFactorySchemaSnapshot> {
    factories
        .iter()
        .map(|factory| ProviderFactorySchemaSnapshot {
            name: factory.name().to_string(),
            witness: factory.schema_witness(),
        })
        .collect()
}

fn register_provider_factory_schemas(
    schemas: &mut SchemaRegistry,
    snapshot: &[ProviderFactorySchemaSnapshot],
) {
    for factory in snapshot {
        schemas
            .insert_provider_witness(&factory.name, &factory.witness)
            .expect("each schema owner must expose one consistent validated witness");
    }
}

pub(crate) trait ProviderFactorySource: Send + Sync {
    fn as_factories(&self) -> &[Box<dyn ProviderFactory>];
}

impl ProviderFactorySource for std::sync::Arc<Vec<Box<dyn ProviderFactory>>> {
    fn as_factories(&self) -> &[Box<dyn ProviderFactory>] {
        self.as_slice()
    }
}

impl ProviderFactorySource for &[Box<dyn ProviderFactory>] {
    fn as_factories(&self) -> &[Box<dyn ProviderFactory>] {
        self
    }
}

/// One indexed resolver implementation for provider-owned declarations and
/// authored multi-owner annotations. Storage may be shared (`Arc`) or borrowed,
/// but both forms execute the same owner/declaration logic below.
pub(crate) struct FactoryCustomTypeResolver<F> {
    factories: F,
    index: BTreeMap<String, OwnerCustomTypeIndex>,
    diagnostic_inputs: BTreeMap<String, ProviderValidationInputs>,
    assignability_closures:
        BTreeMap<crate::parser::SchemaTypeOwner, crate::schema::AssignabilityClosure>,
}

impl<F> FactoryCustomTypeResolver<F>
where
    F: ProviderFactorySource,
{
    pub(crate) fn new(factories: F) -> Self {
        let snapshot = capture_provider_factory_schema_snapshot(factories.as_factories());
        let index = build_owner_custom_type_index(&snapshot);
        let mut schemas = SchemaRegistry::new();
        register_provider_factory_schemas(&mut schemas, &snapshot);
        Self::with_index(factories, index, &schemas)
    }

    fn with_index(
        factories: F,
        index: BTreeMap<String, OwnerCustomTypeIndex>,
        schemas: &SchemaRegistry,
    ) -> Self {
        let diagnostic_inputs = index
            .keys()
            .map(|owner| {
                let inputs = ProviderValidationInputs::collect(owner, std::iter::empty::<&Value>());
                inputs.diagnostics().set_schemas(schemas.clone());
                (owner.clone(), inputs)
            })
            .collect();
        Self {
            diagnostic_inputs,
            factories,
            index,
            assignability_closures: schemas.assignability_closures().clone(),
        }
    }

    fn known_custom_types(&self) -> BTreeSet<crate::parser::OwnedTypeIdentity> {
        // Keep incompatible kinds separate so annotation resolution can reject
        // a cross-provider conflict before constructing a SchemaType.
        let mut declarations_by_identity =
            BTreeMap::<(TypeIdentity, RefinedScalarKind), crate::parser::OwnedTypeIdentity>::new();
        for (owner, indexed) in &self.index {
            for (identity, declarations) in &indexed.declarations {
                let owned = crate::parser::OwnedTypeIdentity::from_schema_declaration(
                    owner,
                    &declarations[0],
                )
                .expect("admitted identified declaration");
                declarations_by_identity
                    .entry((identity.clone(), owned.scalar_kind()))
                    .and_modify(|existing| {
                        *existing = existing.conjoin(&owned);
                    })
                    .or_insert(owned);
            }
        }
        declarations_by_identity.into_values().collect()
    }
}

impl<F> crate::parser::CustomTypeResolver for FactoryCustomTypeResolver<F>
where
    F: ProviderFactorySource,
{
    fn projection_declaration(
        &self,
        projection: &crate::parser::ResolvedSchemaType,
    ) -> Result<AttributeType, crate::parser::CustomTypeValidationError> {
        use crate::parser::CustomTypeValidationError;
        let owner = projection.provider_names().next().expect("nonempty owners");
        let missing = || CustomTypeValidationError::MissingDeclaration {
            owner: owner.into(),
            identity: projection.identity().clone(),
        };
        let mut candidates = self
            .index
            .get(owner)
            .and_then(|index| index.declarations.get(projection.identity()))
            .ok_or_else(missing)?
            .iter()
            .filter(|declaration| projection.matches_projection_declaration(declaration));
        let selected = candidates.next().ok_or_else(missing)?;
        if candidates.any(|declaration| !same_scalar_callbacks(selected, declaration)) {
            return Err(CustomTypeValidationError::Rejected {
                owner: owner.into(),
                identity: projection.identity().clone(),
                message: "exact schema projection has ambiguous callback provenance".into(),
            });
        }
        Ok(selected.clone())
    }

    fn annotation_declaration(
        &self,
        owned: &crate::parser::OwnedTypeIdentity,
    ) -> Result<crate::parser::ResolvedSchemaType, crate::parser::CustomTypeValidationError> {
        use crate::parser::{CustomTypeValidationError, ResolvedSchemaType, SchemaTypeOwner};
        let mut resolved: Option<ResolvedSchemaType> = None;
        for owner in owned.owners().iter() {
            let SchemaTypeOwner::Provider(owner) = owner;
            let missing = || CustomTypeValidationError::MissingDeclaration {
                owner: owner.clone(),
                identity: owned.identity().clone(),
            };
            let declaration = self
                .index
                .get(owner)
                .and_then(|index| index.identity_declarations.get(owned.identity()))
                .ok_or_else(missing)?
                .as_ref()
                .map_err(Clone::clone)?;
            if declaration.scalar_kind() != owned.scalar_kind() {
                return Err(missing());
            }
            resolved = Some(match resolved {
                Some(existing) => existing.conjoin(declaration),
                None => declaration.clone(),
            });
        }
        Ok(resolved
            .expect("nonempty owners and declarations")
            .into_annotation())
    }

    fn annotation_declarations(
        &self,
        owned: &crate::parser::OwnedTypeIdentity,
    ) -> Result<BTreeMap<String, Vec<AttributeType>>, crate::parser::CustomTypeValidationError>
    {
        owned
            .provider_names()
            .map(|owner| {
                let declarations = self
                    .index
                    .get(owner)
                    .and_then(|index| index.declarations.get(owned.identity()))
                    .filter(|declarations| !declarations.is_empty())
                    .ok_or_else(|| {
                        crate::parser::CustomTypeValidationError::MissingDeclaration {
                            owner: owner.to_string(),
                            identity: owned.identity().clone(),
                        }
                    })?;
                Ok((
                    owner.to_string(),
                    declarations
                        .iter()
                        .map(AttributeType::identity_only_declaration)
                        .collect(),
                ))
            })
            .collect()
    }

    fn validate_validated(
        &self,
        owner: &str,
        proof: crate::schema::LocallyValidatedCustomType<'_>,
    ) -> Result<(), crate::parser::CustomTypeValidationError> {
        let owner_index = self.index.get(owner).ok_or_else(|| {
            crate::parser::CustomTypeValidationError::MissingDeclaration {
                owner: owner.to_string(),
                identity: proof.declaration().identity().clone(),
            }
        })?;
        let factories = self.factories.as_factories();
        validate_validated_custom_type_with_factory(
            factories[owner_index.factory_index].as_ref(),
            proof,
            &self.diagnostic_inputs[owner],
        )
    }

    fn enum_declaration(
        &self,
        owned: &crate::parser::OwnedTypeIdentity,
    ) -> Result<Option<AttributeType>, crate::parser::CustomTypeValidationError> {
        use crate::parser::{CustomTypeValidationError, SchemaTypeOwner};

        let identity = owned.identity();
        let mut selected: Option<(String, EnumDefinitionSignature, AttributeType)> = None;
        for owner in owned.owners().iter() {
            let SchemaTypeOwner::Provider(owner) = owner;
            let owner_index = self.index.get(owner).ok_or_else(|| {
                CustomTypeValidationError::MissingDeclaration {
                    owner: owner.clone(),
                    identity: identity.clone(),
                }
            })?;
            let declarations = owner_index.declarations.get(identity).ok_or_else(|| {
                CustomTypeValidationError::MissingDeclaration {
                    owner: owner.clone(),
                    identity: identity.clone(),
                }
            })?;
            for declaration in declarations {
                let Some(signature) = enum_definition_signature(declaration) else {
                    continue;
                };
                if let Some((first_owner, first_signature, _)) = &selected
                    && first_signature != &signature
                {
                    return Err(CustomTypeValidationError::ConflictingEnumDefinitions {
                        identity: identity.clone(),
                        first_owner: first_owner.clone(),
                        second_owner: owner.clone(),
                    });
                }
                selected.get_or_insert_with(|| (owner.clone(), signature, declaration.clone()));
            }
        }
        Ok(selected.map(|(_, _, declaration)| declaration))
    }
}

// Fingerprints retain callback presence, but executable callbacks have no
// portable identity. Duplicate catalog occurrences are safe only when they
// retain the same host callbacks, including an enum's base declaration.
fn same_scalar_callbacks(left: &AttributeType, right: &AttributeType) -> bool {
    let same_callback = match (local_custom_validator(left), local_custom_validator(right)) {
        (Some(left), Some(right)) => std::sync::Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    };
    same_callback
        && match (left.raw_shape(), right.raw_shape()) {
            (RawShape::Enum { base: left, .. }, RawShape::Enum { base: right, .. }) => {
                same_scalar_callbacks(left, right)
            }
            _ => true,
        }
}

impl<F> crate::parser::ProviderCustomTypeSource for FactoryCustomTypeResolver<F>
where
    F: ProviderFactorySource + 'static,
{
    fn known_custom_types(&self) -> BTreeSet<crate::parser::OwnedTypeIdentity> {
        FactoryCustomTypeResolver::known_custom_types(self)
    }

    fn assignability_closures(
        &self,
    ) -> BTreeMap<crate::parser::SchemaTypeOwner, crate::schema::AssignabilityClosure> {
        self.assignability_closures.clone()
    }
}

fn build_owner_custom_type_index(
    snapshot: &[ProviderFactorySchemaSnapshot],
) -> BTreeMap<String, OwnerCustomTypeIndex> {
    snapshot
        .iter()
        .enumerate()
        .map(|(factory_index, factory)| {
            let mut declarations = BTreeMap::<TypeIdentity, Vec<AttributeType>>::new();
            for (identity, declaration) in factory.witness.identified_declarations() {
                declarations.entry(identity).or_default().push(declaration);
            }
            // Aggregate witness admission already proved agreement within
            // this owner, once. Different owners are conjoined at resolution.
            let identity_declarations = declarations
                .iter()
                .map(|(identity, declarations)| {
                    let mut layers = declarations.iter().map(|declaration| {
                        crate::parser::ResolvedSchemaType::from_annotation_declaration(
                            &factory.name,
                            declaration,
                        )
                        .ok_or_else(|| {
                            crate::parser::CustomTypeValidationError::MissingDeclaration {
                                owner: factory.name.clone(),
                                identity: identity.clone(),
                            }
                        })
                    });
                    // Every grouped identity has a declaration. Enum/Reference
                    // bases contribute constraints too; unsupported bases keep
                    // a typed failure in the index instead of dropping its key.
                    let resolved = layers
                        .next()
                        .expect("nonempty identity declaration group")
                        .and_then(|first| {
                            layers.try_fold(first, |resolved, layer| {
                                layer.map(|layer| resolved.conjoin(&layer))
                            })
                        });
                    (identity.clone(), resolved)
                })
                .collect();
            (
                factory.name.clone(),
                OwnerCustomTypeIndex {
                    factory_index,
                    declarations,
                    identity_declarations,
                },
            )
        })
        .collect()
}

/// Build the opaque loaded custom-type state shared by CLI, LSP, and planning.
/// Recognition keys and resolver declarations come from the same factory
/// schema/config snapshot and are indexed once.
pub fn provider_custom_types_from_factories(
    factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>>,
) -> crate::parser::ProviderCustomTypes {
    let resolver = FactoryCustomTypeResolver::new(factories);
    crate::parser::ProviderCustomTypes::from_source(resolver)
}

/// Capture provider schemas once and mint the loaded context and schema proof
/// shared by validation, planning, apply, state writeback, and the LSP.
///
/// Returning one retained bundle makes it impossible for production callers
/// to pair proofs produced by different provider/schema snapshots.
pub fn loaded_provider_snapshot_from_factories(
    factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>>,
    mut schemas: SchemaRegistry,
    environment: crate::EnvironmentSnapshot,
) -> LoadedProviderSnapshot {
    let snapshot = capture_provider_factory_schema_snapshot(&factories);
    register_provider_factory_schemas(&mut schemas, &snapshot);
    let custom_types =
        crate::parser::ProviderCustomTypes::from_source(FactoryCustomTypeResolver::with_index(
            std::sync::Arc::clone(&factories),
            build_owner_custom_type_index(&snapshot),
            &schemas,
        ));
    let provider_context =
        crate::parser::LoadedProviderContext::new(crate::parser::ProviderContext {
            environment,
            decryptor: None,
            custom_types,
            resource_types: crate::parser::ProviderContext::resource_types_from_schema_registry(
                &schemas,
            ),
        });
    LoadedProviderSnapshot {
        factories,
        schemas: ProviderLoadedSchemaRegistry::from_registry(schemas),
        provider_context,
    }
}

pub(crate) fn validate_custom_type_annotation(
    resolver: &dyn crate::parser::CustomTypeResolver,
    owned: &crate::parser::OwnedTypeIdentity,
    value: &crate::resource::knowledge::KnownValue<'_>,
) -> Result<(), crate::parser::CustomTypeValidationError> {
    use crate::parser::{CustomTypeValidationError, SchemaTypeOwner};

    let declarations_by_owner = resolver.annotation_declarations(owned)?;
    let identity = owned.identity();
    let mut first_enum_definition: Option<(String, EnumDefinitionSignature)> = None;
    for owner in owned.owners().iter() {
        let SchemaTypeOwner::Provider(owner) = owner;
        let Some(declarations) = declarations_by_owner
            .get(owner)
            .filter(|declarations| !declarations.is_empty())
        else {
            return Err(CustomTypeValidationError::MissingDeclaration {
                owner: owner.clone(),
                identity: identity.clone(),
            });
        };
        for declaration in declarations {
            let Some(signature) = enum_definition_signature(declaration) else {
                continue;
            };
            if let Some((first_owner, first_signature)) = &first_enum_definition {
                if first_owner != owner && first_signature != &signature {
                    return Err(CustomTypeValidationError::ConflictingEnumDefinitions {
                        identity: identity.clone(),
                        first_owner: first_owner.clone(),
                        second_owner: owner.clone(),
                    });
                }
            } else {
                first_enum_definition = Some((owner.clone(), signature));
            }
        }
    }

    let mut first_enum_spelling: Option<(String, String)> = None;
    for owner in owned.owners().iter() {
        let SchemaTypeOwner::Provider(owner) = owner;
        let Some(declarations) = declarations_by_owner
            .get(owner)
            .filter(|declarations| !declarations.is_empty())
        else {
            return Err(CustomTypeValidationError::MissingDeclaration {
                owner: owner.clone(),
                identity: identity.clone(),
            });
        };
        let mut bridge: Option<(
            RefinedScalarKind,
            crate::schema::LocallyValidatedCustomType<'_>,
        )> = None;
        for declaration in declarations {
            // Reference identities are indexed for annotations but have no
            // provider validation capability. Preserve their offline checks.
            if matches!(declaration.raw_shape(), RawShape::Reference { .. }) {
                declaration
                    .validate_identified_scalar_value(value)
                    .map_err(|error| CustomTypeValidationError::InvalidDeclarationValue {
                        owner: owner.clone(),
                        identity: identity.clone(),
                        error: Box::new(error),
                    })?;
                continue;
            }
            let identified = crate::parser::IdentifiedCustomTypeDeclaration::new(declaration)
                .ok_or_else(|| CustomTypeValidationError::InvalidDeclarationValue {
                    owner: owner.clone(),
                    identity: identity.clone(),
                    error: Box::new(
                        CustomTypeCanonicalizationError::UnidentifiedDeclaration {
                            type_name: declaration.type_name(),
                        }
                        .into_type_error(),
                    ),
                })?;
            let canonical =
                crate::schema::validate_custom_type_annotation_leaf(identified, value, |proof| {
                    proof
                })
                .map_err(|error| {
                    CustomTypeValidationError::InvalidDeclarationValue {
                        owner: owner.clone(),
                        identity: identity.clone(),
                        error: Box::new(error),
                    }
                })?;

            if enum_definition_signature(declaration).is_some()
                && let Some(proof) = &canonical
            {
                let canonical = proof.canonical();
                if let Some((first_owner, first_spelling)) = &first_enum_spelling {
                    if first_owner != owner && first_spelling != canonical.as_str() {
                        return Err(CustomTypeValidationError::ConflictingEnumSpelling {
                            identity: Box::new(identity.clone()),
                            first_owner: first_owner.clone(),
                            first_spelling: first_spelling.clone(),
                            second_owner: owner.clone(),
                            second_spelling: canonical.as_str().to_string(),
                        });
                    }
                } else {
                    first_enum_spelling = Some((owner.clone(), canonical.as_str().to_string()));
                }
            }

            if local_custom_validator(declaration).is_none()
                && let Some(canonical) = canonical
            {
                let Ok(Some(kind)) = identified_custom_type_scalar_kind(declaration) else {
                    return Err(CustomTypeValidationError::Rejected {
                        owner: owner.clone(),
                        identity: identity.clone(),
                        message: "identified declaration has no scalar bridge kind".into(),
                    });
                };
                if let Some((previous_kind, previous)) = &bridge {
                    if *previous_kind != kind || previous.canonical() != canonical.canonical() {
                        return Err(CustomTypeValidationError::Rejected {
                            owner: owner.clone(),
                            identity: identity.clone(),
                            message:
                                "same-identity annotation declarations canonicalize inconsistently"
                                    .into(),
                        });
                    }
                } else {
                    bridge = Some((kind, canonical));
                }
            }
        }
        if let Some((_kind, proof)) = bridge {
            resolver.validate_validated(owner, proof)?;
        }
    }
    Ok(())
}

/// Provider implementation for Box<dyn Provider>
/// This enables dynamic dispatch for Providers
impl<P: Provider + ?Sized> Provider for Box<P> {
    fn resolve_references<'a>(
        &'a self,
        requests: &'a [crate::references::ReferenceRequest],
    ) -> BoxFuture<
        'a,
        ProviderResult<
            Vec<Result<crate::references::ReferenceOutcome, crate::references::ReferenceError>>,
        >,
    > {
        (**self).resolve_references(requests)
    }

    fn name(&self) -> &str {
        (**self).name()
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        (**self).read(id, identifier, request)
    }

    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).read_data_source(resource)
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        (**self).create(id, request, attempt)
    }

    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        (**self).resume_create(request)
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        attempt: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        (**self).update(id, identifier, request, attempt)
    }

    fn delete(&self, attempt: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        (**self).delete(attempt)
    }

    fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
        (**self).required_permissions(id, op)
    }

    fn satisfier_hint(
        &self,
        target_id: &ResourceId,
        attr_path: &AttrPath,
    ) -> ProviderResult<Vec<BindingPattern>> {
        (**self).satisfier_hint(target_id, attr_path)
    }
}

impl<P: Provider + ?Sized> Provider for Arc<P> {
    fn resolve_references<'a>(
        &'a self,
        requests: &'a [crate::references::ReferenceRequest],
    ) -> BoxFuture<
        'a,
        ProviderResult<
            Vec<Result<crate::references::ReferenceOutcome, crate::references::ReferenceError>>,
        >,
    > {
        (**self).resolve_references(requests)
    }

    fn name(&self) -> &str {
        (**self).name()
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        (**self).read(id, identifier, request)
    }

    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).read_data_source(resource)
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        (**self).create(id, request, attempt)
    }

    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        (**self).resume_create(request)
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        attempt: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        (**self).update(id, identifier, request, attempt)
    }

    fn delete(&self, attempt: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        (**self).delete(attempt)
    }

    fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
        (**self).required_permissions(id, op)
    }

    fn satisfier_hint(
        &self,
        target_id: &ResourceId,
        attr_path: &AttrPath,
    ) -> ProviderResult<Vec<BindingPattern>> {
        (**self).satisfier_hint(target_id, attr_path)
    }
}

impl<P: Provider + ?Sized> Provider for &P {
    fn resolve_references<'a>(
        &'a self,
        requests: &'a [crate::references::ReferenceRequest],
    ) -> BoxFuture<
        'a,
        ProviderResult<
            Vec<Result<crate::references::ReferenceOutcome, crate::references::ReferenceError>>,
        >,
    > {
        (**self).resolve_references(requests)
    }

    fn name(&self) -> &str {
        (**self).name()
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        (**self).read(id, identifier, request)
    }

    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).read_data_source(resource)
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        (**self).create(id, request, attempt)
    }

    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        (**self).resume_create(request)
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        attempt: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        (**self).update(id, identifier, request, attempt)
    }

    fn delete(&self, attempt: crate::provider::DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        (**self).delete(attempt)
    }

    fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
        (**self).required_permissions(id, op)
    }

    fn satisfier_hint(
        &self,
        target_id: &ResourceId,
        attr_path: &AttrPath,
    ) -> ProviderResult<Vec<BindingPattern>> {
        (**self).satisfier_hint(target_id, attr_path)
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::provider_text::captured_session;

    mod assignability_edges_stage_a {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        use crate::parser::SchemaTypeOwner;
        use crate::provider::{
            BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderError,
            ProviderFactory, ProviderInitialization, ProviderResult,
            loaded_provider_snapshot_from_factories,
        };
        use crate::schema::{
            AssignabilityEdge, AttributeSchema, AttributeType, ReferenceForms, ResourceSchema,
            SchemaRegistry, TypeIdentity, TypeInSchema, ValidatedConfigSchema,
            ValidatedProviderSchemaWitness, ValidatedResourceSchema,
        };

        #[test]
        fn round14_provider_registry_retains_member_obligations() {
            use crate::schema::{
                FieldAncestry, LoadedTypeAssignment, StringIdentityLayer, StringMemberConstraints,
            };
            let id = TypeIdentity::from_dotted("aws.Name");
            let source = AttributeType::refined_string(
                Some(id.clone()),
                None,
                Some((Some(1), Some(100))),
                None,
            );
            let target = AttributeType::string_member(
                StringIdentityLayer::new(Some(id), None, Some((Some(1), Some(100))), None),
                StringMemberConstraints::new(None, Some((Some(1), Some(10)))),
            );
            let resource = ResourceSchema::new("Names")
                .attribute(AttributeSchema::optional("source", source).build())
                .attribute(AttributeSchema::optional("target", target).build())
                .validate()
                .unwrap();
            let witness = ValidatedProviderSchemaWitness::new(
                vec![resource],
                Default::default(),
                Default::default(),
                vec![],
            )
            .unwrap();
            let mut registry = SchemaRegistry::new();
            registry.insert_provider_witness("test", &witness).unwrap();
            let loaded = loaded_provider_snapshot_from_factories(
                Arc::default(),
                registry,
                crate::EnvironmentSnapshot::empty(),
            );
            let schema = loaded
                .schema_registry()
                .get("test", "Names", crate::schema::SchemaKind::Resource)
                .unwrap();
            let source = schema.as_schema().attributes()["source"].attr_type();
            let target = schema.as_schema().attributes()["target"].attr_type();
            let assignment = TypeInSchema::schemaless(source)
                .assignment_to(
                    schema.assignment_sink(target, &loaded.provider_context().custom_types),
                )
                .require_loaded()
                .unwrap();
            let LoadedTypeAssignment::NeedsLocal(obligation) = assignment else {
                panic!("publishing a schema cannot erase its member layer")
            };
            for length in [5, 50] {
                let value: crate::resource::Value = crate::resource::Value::Concrete(
                    crate::resource::ConcreteValue::String("A".repeat(length).into()),
                );
                let result = obligation.check_or_defer(&value, FieldAncestry::WRITABLE);
                assert_eq!(result.is_ok(), length == 5, "{result:?}");
                if let Err(error) = result {
                    assert_eq!(
                        error.to_string().matches("outside allowed range").count(),
                        1
                    );
                }
            }
        }

        fn identity(kind: &str) -> TypeIdentity {
            TypeIdentity::new(Some("semantic"), ["Bucket"], kind)
        }

        fn owner(name: &str) -> SchemaTypeOwner {
            SchemaTypeOwner::Provider(name.to_string())
        }

        fn string(kind: &str) -> AttributeType {
            AttributeType::refined_string(Some(identity(kind)), None, None, None)
        }

        struct EdgeFactory {
            owner: &'static str,
            witness: ValidatedProviderSchemaWitness,
            captures: Arc<AtomicUsize>,
        }

        impl ProviderFactory for EdgeFactory {
            fn name(&self) -> &str {
                self.owner
            }
            fn display_name(&self) -> &str {
                self.owner
            }
            fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
                self.captures.fetch_add(1, Ordering::SeqCst);
                self.witness.clone()
            }
            fn schemas(&self) -> Vec<ValidatedResourceSchema> {
                panic!("snapshot must use the aggregate witness")
            }
            fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
                panic!("snapshot must use the aggregate witness")
            }
            fn validate_config(
                &self,
                _: &CanonicalProviderConfigAttributes,
                _session: &crate::provider::ProviderValidationInputs,
            ) -> ProviderResult<()> {
                Ok(())
            }
            fn create_provider(
                &self,
                _: &crate::provider::ProviderSession,
                _: InitializeOutcomeMint,
            ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
                Box::pin(async { Err(ProviderError::internal("schema-only test")) })
            }
        }

        fn factory(
            owner: &'static str,
            edges: &[(&str, &str)],
            captures: Arc<AtomicUsize>,
        ) -> Box<dyn ProviderFactory> {
            let resource = ["A", "B", "C", "Unrelated"]
                .into_iter()
                .fold(ResourceSchema::new("test.Bucket"), |schema, kind| {
                    schema.attribute(AttributeSchema::optional(kind, string(kind)).build())
                })
                .validate()
                .unwrap();
            let witness = ValidatedProviderSchemaWitness::new(
                vec![resource],
                ValidatedConfigSchema::default(),
                ReferenceForms::new(),
                edges
                    .iter()
                    .map(|(source, sink)| {
                        AssignabilityEdge::new(identity(source), identity(sink)).unwrap()
                    })
                    .collect(),
            )
            .unwrap();
            Box::new(EdgeFactory {
                owner,
                witness,
                captures,
            })
        }

        fn snapshot(
            factories: Vec<Box<dyn ProviderFactory>>,
        ) -> crate::provider::LoadedProviderSnapshot {
            loaded_provider_snapshot_from_factories(
                Arc::new(factories),
                SchemaRegistry::new(),
                crate::EnvironmentSnapshot::empty(),
            )
        }

        #[test]
        fn round11_owner_index_is_total_for_scalar_and_enum_declarations() {
            use crate::provider::{ProviderFactorySchemaSnapshot, build_owner_custom_type_index};
            use crate::schema::PatternAttributeType;
            use std::collections::BTreeMap;
            let id = TypeIdentity::from_dotted("test.X");
            let scalar = PatternAttributeType::refined_string(Some(id.clone()), None, None, None);
            let enumeration = PatternAttributeType::enum_with_base(
                id.clone(),
                PatternAttributeType::string(),
                Some(vec!["yes".into()]),
                Vec::new(),
                None,
                None,
            );
            let witness = ValidatedProviderSchemaWitness::new(
                Vec::new(),
                ValidatedConfigSchema::validate(BTreeMap::from([
                    ("scalar".into(), scalar),
                    ("enum".into(), enumeration),
                ]))
                .unwrap(),
                Default::default(),
                Vec::new(),
            )
            .unwrap();
            let snapshots = [ProviderFactorySchemaSnapshot {
                name: "test".into(),
                witness,
            }];
            let index = build_owner_custom_type_index(&snapshots);
            assert!(
                index["test"]
                    .identity_declarations
                    .get(&id)
                    .is_some_and(Result::is_ok),
                "an enum base must not drop the identity's scalar index"
            );
            assert_eq!(index["test"].declarations[&id].len(), 2);
        }

        #[test]
        fn registry_witness_edges_reach_loaded_catalog_without_a_factory() {
            let mut registry = SchemaRegistry::new();
            registry
                .insert_provider_witness(
                    "builtin",
                    &factory("builtin", &[("A", "B")], Arc::default()).schema_witness(),
                )
                .unwrap();
            // Registry merges must preserve the witness's edge authority too.
            let mut merged = SchemaRegistry::new();
            merged.extend(registry).unwrap();
            let loaded = loaded_provider_snapshot_from_factories(
                Arc::default(),
                merged,
                crate::EnvironmentSnapshot::empty(),
            );
            let catalog = &loaded.provider_context().custom_types;
            assert!(catalog.is_loaded());
            assert!(
                loaded
                    .schema_registry()
                    .assignability_closures()
                    .contains_key(&owner("builtin"))
            );
            assert!(catalog.reachable(&owner("builtin"), &identity("A"), &identity("B")));
            let schema = loaded
                .schema_registry()
                .get(
                    "builtin",
                    "test.Bucket",
                    crate::schema::SchemaKind::Resource,
                )
                .unwrap();
            assert!(matches!(
                TypeInSchema::schemaless(schema.as_schema().attributes()["A"].attr_type())
                    .assignment_to(schema.assignment_sink(
                        schema.as_schema().attributes()["B"].attr_type(),
                        catalog
                    )),
                crate::schema::TypeAssignability::Assignable,
            ));
        }

        #[test]
        fn registry_witness_without_edges_proves_mismatch_without_a_factory() {
            let mut registry = SchemaRegistry::new();
            registry
                .insert_provider_witness(
                    "builtin",
                    &factory("builtin", &[], Arc::default()).schema_witness(),
                )
                .unwrap();
            let loaded = loaded_provider_snapshot_from_factories(
                Arc::default(),
                registry,
                crate::EnvironmentSnapshot::empty(),
            );
            let catalog = &loaded.provider_context().custom_types;
            assert!(catalog.is_loaded());
            assert!(
                loaded
                    .schema_registry()
                    .assignability_closures()
                    .contains_key(&owner("builtin"))
            );
            assert!(catalog.assignability_edges_available(&owner("builtin")));
            let schema = loaded
                .schema_registry()
                .get(
                    "builtin",
                    "test.Bucket",
                    crate::schema::SchemaKind::Resource,
                )
                .unwrap();
            assert!(matches!(
                TypeInSchema::schemaless(schema.as_schema().attributes()["A"].attr_type())
                    .assignment_to(schema.assignment_sink(
                        schema.as_schema().attributes()["B"].attr_type(),
                        catalog
                    )),
                crate::schema::TypeAssignability::Mismatch,
            ));
        }

        #[test]
        fn registry_rejects_conflicting_witness_edges_on_insert_and_merge() {
            let with_edges = factory("builtin", &[("A", "B")], Arc::default()).schema_witness();
            let without_edges = factory("builtin", &[], Arc::default()).schema_witness();
            for merge in [false, true] {
                let mut registry = SchemaRegistry::new();
                registry
                    .insert_provider_witness("builtin", &with_edges)
                    .unwrap();
                let result = if merge {
                    let mut incoming = SchemaRegistry::new();
                    incoming
                        .insert_provider_witness("builtin", &without_edges)
                        .unwrap();
                    registry.extend(incoming)
                } else {
                    registry.insert_provider_witness("builtin", &without_edges)
                };
                assert_eq!(
                    result.expect_err("one owner cannot have conflicting witness-backed edges"),
                    crate::schema::SchemaRegistryError::ConflictingAssignabilityEdges {
                        owner: "builtin".into()
                    },
                );
                assert!(
                    registry.assignability_closures()[&owner("builtin")]
                        .reachable(&identity("A"), &identity("B"))
                );
            }
        }

        #[test]
        fn native_factory_edges_reach_loaded_catalog_from_one_witness_capture() {
            let captures = Arc::new(AtomicUsize::new(0));
            let loaded = snapshot(vec![factory(
                "actual-owner",
                &[("A", "B")],
                Arc::clone(&captures),
            )]);
            let catalog = &loaded.provider_context().custom_types;
            assert!(catalog.reachable(&owner("actual-owner"), &identity("A"), &identity("B")));
            assert!(
                !catalog.reachable(&owner("semantic"), &identity("A"), &identity("B")),
                "TypeIdentity::provider is not the declaration owner"
            );
            assert!(!catalog.reachable(&owner("actual-owner"), &identity("B"), &identity("A")));
            assert_eq!(captures.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn reachability_is_transitive_and_directional_within_one_provider() {
            let loaded = snapshot(vec![factory(
                "owner",
                &[("A", "B"), ("B", "C")],
                Arc::default(),
            )]);
            let catalog = &loaded.provider_context().custom_types;
            assert!(catalog.reachable(&owner("owner"), &identity("A"), &identity("B")));
            assert!(catalog.reachable(&owner("owner"), &identity("B"), &identity("C")));
            assert!(catalog.reachable(&owner("owner"), &identity("A"), &identity("C")));
            assert!(!catalog.reachable(&owner("owner"), &identity("C"), &identity("A")));
            assert!(!catalog.reachable(&owner("owner"), &identity("A"), &identity("Unrelated")));
            assert!(!catalog.reachable(&owner("missing-owner"), &identity("A"), &identity("C")));
        }

        #[test]
        fn reachability_never_composes_edges_from_two_providers() {
            // Both owners declare all endpoints. Only the edges differ, so a
            // global union of edge sets would incorrectly manufacture A -> C.
            let loaded = snapshot(vec![
                factory("first", &[("A", "B")], Arc::default()),
                factory("second", &[("B", "C")], Arc::default()),
            ]);
            let catalog = &loaded.provider_context().custom_types;
            assert!(catalog.reachable(&owner("first"), &identity("A"), &identity("B")));
            assert!(catalog.reachable(&owner("second"), &identity("B"), &identity("C")));
            for name in ["first", "second"] {
                assert!(!catalog.reachable(&owner(name), &identity("A"), &identity("C")));
            }
            assert!(!catalog.reachable(&owner("first"), &identity("B"), &identity("C")));
            assert!(!catalog.reachable(&owner("second"), &identity("A"), &identity("B")));
        }

        #[test]
        fn reachability_for_all_rejects_an_edge_missing_from_one_co_owner() {
            let loaded = snapshot(vec![
                factory("first", &[("A", "B")], Arc::default()),
                factory("second", &[], Arc::default()),
            ]);
            let catalog = &loaded.provider_context().custom_types;
            let sink = catalog
                .known()
                .iter()
                .find(|owned| owned.identity() == &identity("B"))
                .unwrap();
            assert_eq!(sink.owners().iter().len(), 2);
            assert!(catalog.reachable(&owner("first"), &identity("A"), sink.identity()));
            assert!(!catalog.reachable(&owner("second"), &identity("A"), sink.identity()));
            assert!(!catalog.reachable_for_all(sink.owners(), &identity("A"), sink.identity()));
        }

        #[test]
        fn reachability_for_all_accepts_an_edge_declared_by_both_co_owners() {
            let loaded = snapshot(vec![
                factory("first", &[("A", "B")], Arc::default()),
                factory("second", &[("A", "B")], Arc::default()),
            ]);
            let catalog = &loaded.provider_context().custom_types;
            let sink = catalog
                .known()
                .iter()
                .find(|owned| owned.identity() == &identity("B"))
                .unwrap();
            assert_eq!(sink.owners().iter().len(), 2);
            assert!(catalog.reachable_for_all(sink.owners(), &identity("A"), sink.identity()));
            assert!(!catalog.reachable_for_all(sink.owners(), sink.identity(), &identity("A")));
            assert!(
                !catalog
                    .with_failed_providers(std::collections::BTreeSet::from(["second".to_string()]))
                    .reachable_for_all(sink.owners(), &identity("A"), sink.identity())
            );
            assert!(
                !crate::parser::ProviderCustomTypes::default().reachable_for_all(
                    sink.owners(),
                    &identity("A"),
                    sink.identity(),
                )
            );
        }

        #[test]
        fn reachability_is_unavailable_for_failed_owners() {
            let loaded = snapshot(vec![
                factory("first", &[("A", "B")], Arc::default()),
                factory("second", &[("B", "C")], Arc::default()),
            ]);
            let catalog = loaded
                .provider_context()
                .custom_types
                .with_failed_providers(std::collections::BTreeSet::from(["first".to_string()]));
            assert!(!catalog.reachable(&owner("first"), &identity("A"), &identity("B")));
            assert!(catalog.reachable(&owner("second"), &identity("B"), &identity("C")));
            assert!(!crate::parser::ProviderCustomTypes::default().reachable(
                &owner("first"),
                &identity("A"),
                &identity("B"),
            ));
        }

        #[test]
        fn stage_b_judgement_consults_the_stored_edges() {
            let sink_owner = owner("owner");
            let loaded = snapshot(vec![factory("owner", &[("A", "B")], Arc::default())]);
            assert!(loaded.provider_context().custom_types.reachable(
                &owner("owner"),
                &identity("A"),
                &identity("B")
            ));
            let source = string("A");
            let sink = string("B");
            assert!(TypeInSchema::schemaless(&source).is_assignable_to(
                (TypeInSchema::schemaless(&sink)).with_authority(
                    &loaded.provider_context().custom_types,
                    crate::schema::SinkAuthority::Owner(&sink_owner)
                )
            ));
            assert!(!TypeInSchema::schemaless(&sink).is_assignable_to(
                (TypeInSchema::schemaless(&source)).with_authority(
                    &loaded.provider_context().custom_types,
                    crate::schema::SinkAuthority::Owner(&sink_owner)
                )
            ));
            assert!(TypeInSchema::schemaless(&source).is_assignable_to(
                (TypeInSchema::schemaless(&source)).with_authority(
                    &loaded.provider_context().custom_types,
                    crate::schema::SinkAuthority::Owner(&sink_owner)
                )
            ));
        }
    }

    #[test]
    fn provider_text_persisted_attribute_error_labels_are_capped_with_safe_sources() {
        let raw = format!("\x1b[2J\u{9b}{}", "x".repeat(20 * 1024));
        let error = super::PersistedWireValueError {
            attribute_key: raw.clone(),
            error: crate::value::SerializationError::UnresolvedEnumIdentifier {
                value: raw,
                context: crate::value::SerializationContext::WasmBoundary,
            },
        };
        assert!(error.to_string().len() <= crate::provider_text::MAX_PROVIDER_TEXT_BYTES);
        assert!(!crate::executor::render_error_chain(&error).contains(['\x1b', '\u{9b}']));
    }

    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };

    use super::*;

    type Value = crate::resource::ExpandedValue;
    type ConcreteValue = crate::resource::ExpandedConcreteValue;
    type DeferredValue = crate::resource::ExpandedDeferredValue;

    fn absolute_binding(binding: &str) -> crate::resource::AbsoluteBindingAddress {
        crate::resource::RootScope::new()
            .resolve(crate::resource::RelativeBindingName::new(binding))
    }

    fn initialized(provider: impl Provider + 'static) -> InitializedProvider {
        InitializedProvider::for_test(Box::new(provider), ReferenceScope::Unavailable)
    }

    #[test]
    fn round13_annotation_resolution_requires_the_total_identity_index() {
        use crate::parser::{CustomTypeResolver, CustomTypeValidationError, OwnedTypeIdentity};
        let identity = TypeIdentity::from_dotted("test.Indexed");
        let declaration = AttributeType::refined_string(Some(identity.clone()), None, None, None);
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(ConfigOnlyCustomTypeFactory)];
        let index = BTreeMap::from([(
            "config-owner".into(),
            OwnerCustomTypeIndex {
                factory_index: 0,
                declarations: BTreeMap::from([(identity.clone(), vec![declaration])]),
                identity_declarations: BTreeMap::new(),
            },
        )]);
        let resolver = FactoryCustomTypeResolver::with_index(
            factories.as_slice(),
            index,
            &SchemaRegistry::new(),
        );
        let owned =
            OwnedTypeIdentity::provider("config-owner", identity, RefinedScalarKind::String);
        assert!(
            matches!(
                resolver.annotation_declaration(&owned),
                Err(CustomTypeValidationError::MissingDeclaration { .. })
            ),
            "raw declarations cannot replace missing indexed evidence"
        );
    }

    #[test]
    fn schema_derived_guest_dispatch_restores_the_typed_scalar() {
        use crate::schema::legacy_validator;

        let identity = TypeIdentity::new(Some("test"), ["number"], "GuardedInt");
        let schema = PatternResourceSchema::new("test.Guarded")
            .attribute(
                PatternAttributeSchema::optional(
                    "value",
                    PatternAttributeType::refined_int_with_validator(
                        identity.clone(),
                        None,
                        legacy_validator(|value| match value {
                            crate::resource::Value::Concrete(
                                crate::resource::ConcreteValue::Int(5),
                            ) => Err(("schema-derived callback rejected five".to_string()).into()),
                            _ => Err(("Expected integer".to_string()).into()),
                        }),
                    ),
                )
                .build(),
            )
            .validate()
            .expect("dispatch fixture schema must validate");

        let table = CustomTypeDispatchTable::build(&[schema], &ValidatedConfigSchema::default())
            .expect("one scalar kind must build a dispatch table");
        let error = table
            .dispatch(&identity, "5")
            .expect_err("the schema callback must be exported without a provider override");
        assert_eq!(error.to_string(), "schema-derived callback rejected five");
    }

    #[test]
    fn raw_float_bridge_rejection_preserves_source_without_invoking_callbacks() {
        use std::error::Error;
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (identity, table) = recording_float_dispatch_table(received.clone());
        for (text, category) in [
            ("NaN", crate::NonFiniteFloatError::NaN),
            ("inf", crate::NonFiniteFloatError::PositiveInfinity),
            ("-inf", crate::NonFiniteFloatError::NegativeInfinity),
            ("1e999", crate::NonFiniteFloatError::PositiveInfinity),
            ("-1e999", crate::NonFiniteFloatError::NegativeInfinity),
        ] {
            let error = table.dispatch(&identity, text).unwrap_err();
            let rendered = crate::executor::render_error_chain(&error);
            assert_eq!(
                rendered.matches("invalid float bridge value").count(),
                1,
                "{rendered}"
            );
            assert_eq!(
                rendered.matches(&category.to_string()).count(),
                1,
                "{rendered}"
            );
            assert_eq!(
                error
                    .source()
                    .unwrap()
                    .source()
                    .unwrap()
                    .downcast_ref::<crate::NonFiniteFloatError>(),
                Some(&category)
            );
            assert!(received.lock().unwrap().is_empty());
        }
        let error = table.dispatch(&identity, "not-a-float").unwrap_err();
        assert!(
            error
                .source()
                .unwrap()
                .source()
                .unwrap()
                .is::<std::num::ParseFloatError>()
        );
        assert!(received.lock().unwrap().is_empty());
    }

    #[test]
    fn schema_derived_guest_dispatch_uses_the_declared_scalar_kind() {
        let identity = TypeIdentity::new(Some("test"), ["number"], "GuardedInt");
        let schema = PatternResourceSchema::new("test.Guarded")
            .attribute(
                PatternAttributeSchema::optional(
                    "value",
                    PatternAttributeType::refined_int(Some(identity.clone()), None),
                )
                .build(),
            )
            .validate()
            .expect("dispatch fixture schema must validate");
        let table = CustomTypeDispatchTable::build(&[schema], &ValidatedConfigSchema::default())
            .expect("one scalar kind must build a dispatch table");

        table
            .dispatch(&identity, "5")
            .expect("the table parses a known integer identity");
        let error = table
            .dispatch(&identity, "not-an-integer")
            .expect_err("the table must parse using the identity's declared scalar kind");
        assert!(
            crate::executor::render_error_chain(&error).contains("invalid integer bridge value"),
            "{error}"
        );
        assert!(!error.to_string().contains("not-an-integer"), "{error}");
    }

    fn recording_float_dispatch_table(
        received: std::sync::Arc<std::sync::Mutex<Vec<f64>>>,
    ) -> (TypeIdentity, CustomTypeDispatchTable) {
        let identity = TypeIdentity::new(Some("test"), ["number"], "GuardedFloat");
        let schema = PatternResourceSchema::new("test.Guarded")
            .attribute(
                PatternAttributeSchema::optional(
                    "value",
                    PatternAttributeType::refined_float_with_validator(
                        identity.clone(),
                        None,
                        crate::schema::legacy_validator(move |value| match value {
                            crate::resource::Value::Concrete(
                                crate::resource::ConcreteValue::Float(value),
                            ) => {
                                received.lock().unwrap().push(value.get());
                                Ok(())
                            }
                            _ => Err(("Expected float".to_string()).into()),
                        }),
                    ),
                )
                .build(),
            )
            .validate()
            .expect("dispatch fixture schema must validate");
        let table = CustomTypeDispatchTable::build(&[schema], &ValidatedConfigSchema::default())
            .expect("one scalar kind must build a dispatch table");
        (identity, table)
    }

    fn assert_schema_derived_guest_float_round_trip(value: f64) {
        use std::sync::{Arc, Mutex};

        let received = Arc::new(Mutex::new(Vec::new()));
        let (identity, table) = recording_float_dispatch_table(Arc::clone(&received));
        let declaration = AttributeType::refined_float(Some(identity.clone()), None);
        let canonical = canonicalize_custom_type_value(
            &declaration,
            &Value::Concrete(ConcreteValue::Float(crate::finite(value))),
        )
        .expect("Float values must canonicalize")
        .expect("Float values must produce bridge text");
        table
            .dispatch(&identity, canonical.as_str())
            .expect("Float values must reach the validator");
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].to_bits(), value.to_bits());
    }

    #[test]
    fn schema_derived_guest_float_dispatch_preserves_large_float() {
        assert_schema_derived_guest_float_round_trip(9_223_372_036_854_774_784.0);
    }

    #[test]
    fn schema_derived_guest_float_dispatch_preserves_negative_zero() {
        assert_schema_derived_guest_float_round_trip(-0.0);
    }

    #[test]
    fn schema_derived_guest_float_dispatch_requires_exact_integer_widening() {
        use std::sync::{Arc, Mutex};

        let received = Arc::new(Mutex::new(Vec::new()));
        let (identity, table) = recording_float_dispatch_table(Arc::clone(&received));
        let declaration = AttributeType::refined_float(Some(identity.clone()), None);

        for (value, expected_text, expected_float) in [
            (Value::Concrete(ConcreteValue::Int(3)), "3", 3.0),
            (
                Value::Concrete(ConcreteValue::Float(crate::finite(1.5))),
                "1.5",
                1.5,
            ),
            (
                Value::Concrete(ConcreteValue::Int(9_223_372_036_854_774_784)),
                "9223372036854775000",
                9_223_372_036_854_774_784.0,
            ),
        ] {
            let canonical = canonicalize_custom_type_value(&declaration, &value)
                .expect("numeric values must canonicalize without rounding")
                .expect("numeric values must produce bridge text");
            assert_eq!(canonical.as_str(), expected_text);
            let result = table.dispatch(&identity, canonical.as_str());
            let received = std::mem::take(&mut *received.lock().unwrap());
            result.expect("exact values must reach the Float validator");
            assert_eq!(received, vec![expected_float]);
        }

        let result = canonicalize_custom_type_value(
            &declaration,
            &Value::Concrete(ConcreteValue::Int(9_007_199_254_740_993)),
        )
        .map(|canonical| {
            let canonical = canonical.expect("numeric values must produce bridge text");
            table
                .dispatch(&identity, canonical.as_str())
                .expect("canonical values must reach the Float validator");
        });
        assert!(
            received.lock().unwrap().is_empty(),
            "inexact integer must not reach the callback"
        );
        let error = result.expect_err("inexact integer must fail during canonicalization");
        assert_eq!(
            error,
            CustomTypeCanonicalizationError::InexactFloatWidening {
                identity,
                value: 9_007_199_254_740_993,
            }
        );
        let expected_message = "integer 9007199254740993 cannot be represented exactly as Float for custom type `test.number.GuardedFloat`";
        assert_eq!(error.to_string(), expected_message);
        assert!(matches!(
            error.into_type_error(),
            crate::schema::TypeError::ValidationFailed { message }
                if message.to_string() == expected_message
        ));
    }

    #[test]
    fn schema_derived_guest_dispatch_rejects_an_unknown_identity() {
        let known_identity = TypeIdentity::new(Some("test"), ["number"], "GuardedInt");
        let schema = PatternResourceSchema::new("test.Guarded")
            .attribute(
                PatternAttributeSchema::optional(
                    "value",
                    PatternAttributeType::refined_int(Some(known_identity), None),
                )
                .build(),
            )
            .validate()
            .expect("dispatch fixture schema must validate");
        let table = CustomTypeDispatchTable::build(&[schema], &ValidatedConfigSchema::default())
            .expect("one scalar kind must build a dispatch table");
        let unknown_identity = TypeIdentity::new(Some("test"), ["number"], "Unknown");

        let error = table
            .dispatch(&unknown_identity, "5")
            .expect_err("an identity absent from the provider schemas must be rejected");
        assert!(
            error
                .to_string()
                .contains("is not declared by this provider"),
            "{error}"
        );
    }

    #[test]
    fn custom_type_names_for_completion_are_in_identity_order() {
        let alpha = TypeIdentity::new(Some("test"), ["value"], "Alpha");
        let zed = TypeIdentity::new(Some("test"), ["value"], "Zed");
        let schema = PatternResourceSchema::new("test.Values")
            .attribute(
                PatternAttributeSchema::optional(
                    "z_declared_first",
                    PatternAttributeType::refined_string(Some(zed.clone()), None, None, None),
                )
                .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "a_declared_second",
                    PatternAttributeType::refined_string(Some(alpha.clone()), None, None, None),
                )
                .build(),
            )
            .validate()
            .expect("completion fixture schema must validate");
        let mut registry = SchemaRegistry::new();
        registry.insert("test", schema);

        assert_eq!(collect_custom_type_names(&registry), [alpha, zed]);
    }

    #[test]
    fn aggregate_dispatch_validation_rejects_cross_schema_scalar_kind_reuse() {
        let identity = TypeIdentity::new(Some("test"), ["value"], "Shared");
        let string_schema = PatternResourceSchema::new("test.StringValue")
            .attribute(
                PatternAttributeSchema::optional(
                    "value",
                    PatternAttributeType::refined_string(Some(identity.clone()), None, None, None),
                )
                .build(),
            )
            .validate()
            .expect("single-kind schema must validate");
        let int_schema = PatternResourceSchema::new("test.IntValue")
            .attribute(
                PatternAttributeSchema::optional(
                    "value",
                    PatternAttributeType::refined_int(Some(identity), None),
                )
                .build(),
            )
            .validate()
            .expect("single-kind schema must validate");

        let error = CustomTypeDispatchTable::build(
            &[string_schema, int_schema],
            &ValidatedConfigSchema::default(),
        )
        .expect_err("one provider cannot reuse an identity across scalar kinds");
        assert!(error.to_string().contains("both String and Int"), "{error}");
    }

    #[test]
    fn aggregate_dispatch_runs_distinct_validators_for_one_identity() {
        use crate::schema::legacy_validator;
        use std::sync::{Arc, Mutex};

        let identity = TypeIdentity::new(Some("test"), ["value"], "Shared");
        for iteration in 0..200 {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let record = |name: &'static str, error: Option<&'static str>| {
                let calls = Arc::clone(&calls);
                legacy_validator(move |_| {
                    calls
                        .lock()
                        .expect("callback log mutex poisoned")
                        .push(name);
                    match error {
                        Some(message) => Err((message.to_string()).into()),
                        None => Ok(()),
                    }
                })
            };

            // Deliberately declare roots in a different order from their
            // canonical names. A raw HashMap walk can run either failing
            // callback first; the ordered declaration walk must always run
            // `a`, then `m`, and short-circuit before `z`.
            let schema = PatternResourceSchema::new("test.Values")
                .attribute(
                    PatternAttributeSchema::optional(
                        "z_declared_first",
                        PatternAttributeType::refined_string_with_validator(
                            identity.clone(),
                            None,
                            None,
                            record("z_declared_first", Some("last declaration rejected")),
                            None,
                        ),
                    )
                    .build(),
                )
                .attribute(
                    PatternAttributeSchema::optional(
                        "a_declared_second",
                        PatternAttributeType::refined_string_with_validator(
                            identity.clone(),
                            None,
                            None,
                            record("a_declared_second", None),
                            None,
                        ),
                    )
                    .build(),
                )
                .attribute(
                    PatternAttributeSchema::optional(
                        "m_declared_third",
                        PatternAttributeType::refined_string_with_validator(
                            identity.clone(),
                            None,
                            None,
                            record("m_declared_third", Some("middle declaration rejected")),
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .expect("same-kind declarations remain a valid resource schema");

            let table =
                CustomTypeDispatchTable::build(&[schema], &ValidatedConfigSchema::default())
                    .expect("v13 dispatch combines every callback attached to one identity");
            let error = table
                .dispatch(&identity, "value")
                .expect_err("the first rejecting callback must stop dispatch");
            assert_eq!(
                error.to_string(),
                "middle declaration rejected",
                "iteration {iteration}"
            );
            assert_eq!(
                *calls.lock().expect("callback log mutex poisoned"),
                ["a_declared_second", "m_declared_third"],
                "iteration {iteration}"
            );
        }
    }

    #[test]
    fn aggregate_dispatch_applies_identity_callback_to_callback_free_declarations() {
        use crate::schema::legacy_validator;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let identity = TypeIdentity::new(Some("test"), ["value"], "Shared");
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        let with_callback = PatternResourceSchema::new("test.WithCallback")
            .attribute(
                PatternAttributeSchema::optional(
                    "value",
                    PatternAttributeType::refined_string_with_validator(
                        identity.clone(),
                        None,
                        None,
                        legacy_validator(move |_value| {
                            callback_calls.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        }),
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .expect("callback schema must validate");
        let without_callback = PatternResourceSchema::new("test.WithoutCallback")
            .attribute(
                PatternAttributeSchema::optional(
                    "value",
                    PatternAttributeType::refined_string(Some(identity.clone()), None, None, None),
                )
                .build(),
            )
            .validate()
            .expect("callback-free schema must validate");

        let table = CustomTypeDispatchTable::build(
            &[without_callback, with_callback],
            &ValidatedConfigSchema::default(),
        )
        .expect("callback presence is a property of the identity, not one declaration");
        table
            .dispatch(&identity, "value-from-callback-free-resource")
            .expect("the identity callback must accept the sibling resource value");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn aggregate_dispatch_accepts_fresh_callbacks_from_repeated_constructor_calls() {
        use crate::schema::legacy_validator;
        use std::sync::atomic::{AtomicUsize, Ordering};

        fn provider_type<R: crate::schema::PatternForm>(
            identity: &TypeIdentity,
            calls: &std::sync::Arc<AtomicUsize>,
        ) -> crate::schema::PatternAttributeType<R> {
            let calls = calls.clone();
            PatternAttributeType::refined_string_with_validator(
                identity.clone(),
                None,
                None,
                legacy_validator(move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                None,
            )
        }

        let identity = TypeIdentity::new(Some("aws"), ["common"], "Region");
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let schemas = ["aws.First", "aws.Second"].map(|resource_type| {
            PatternResourceSchema::new(resource_type)
                .attribute(
                    PatternAttributeSchema::optional("region", provider_type(&identity, &calls))
                        .build(),
                )
                .validate()
                .expect("repeated provider-type constructor schema must validate")
        });

        let table = CustomTypeDispatchTable::build(&schemas, &ValidatedConfigSchema::default())
            .expect("fresh callback allocations for one identity must load");
        table.dispatch(&identity, "ap-northeast-1").unwrap();
        table.dispatch(&identity, "us-east-1").unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "both distinct callback allocations run for both resources' values"
        );
    }

    struct ConfigOnlyCustomTypeFactory;

    impl ProviderFactory for ConfigOnlyCustomTypeFactory {
        fn name(&self) -> &str {
            "config-owner"
        }

        fn display_name(&self) -> &str {
            "config-only custom type fixture"
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<ValidatedResourceSchema> {
            Vec::new()
        }

        fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
            ValidatedConfigSchema::validate(BTreeMap::from([(
                "endpoint".to_string(),
                PatternAttributeType::refined_string(
                    Some(TypeIdentity::new(Some("test"), ["config"], "Endpoint")),
                    None,
                    None,
                    None,
                ),
            )]))
            .expect("config-only custom type must validate")
        }

        fn validate_config(
            &self,
            _attributes: &CanonicalProviderConfigAttributes,
            _session: &crate::provider::ProviderValidationInputs,
        ) -> ProviderResult<()> {
            Ok(())
        }

        fn create_provider(
            &self,
            _config: &crate::provider::ProviderSession,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
            Box::pin(async { Err(ProviderError::internal("not used")) })
        }
    }

    #[test]
    fn native_custom_type_validation_inherits_host_sink() {
        struct DiagnosticFactory;
        impl ProviderFactory for DiagnosticFactory {
            fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
                ConfigOnlyCustomTypeFactory.schema_witness()
            }
            fn name(&self) -> &str {
                "config-owner"
            }
            fn display_name(&self) -> &str {
                "Diagnostic validator"
            }
            fn schemas(&self) -> Vec<ValidatedResourceSchema> {
                Vec::new()
            }
            fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
                ConfigOnlyCustomTypeFactory.provider_config_attribute_types()
            }
            fn validate_config(
                &self,
                _: &CanonicalProviderConfigAttributes,
                _: &ProviderValidationInputs,
            ) -> ProviderResult<()> {
                Ok(())
            }
            fn validate_custom_type(
                &self,
                _: &TypeIdentity,
                _: &CanonicalCustomTypeValue,
                inputs: &ProviderValidationInputs,
            ) -> ProviderResult<()> {
                let diagnostic = ProviderDiagnostic::message("native custom-type warning".into());
                inputs
                    .diagnostics()
                    .emit(crate::provider_text::ProviderEvent::diagnostic(
                        crate::provider_text::EventLevel::Warn,
                        "validation".into(),
                        &diagnostic,
                    ));
                Ok(())
            }
            fn create_provider(
                &self,
                _: &ProviderSession,
                _: InitializeOutcomeMint,
            ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
                Box::pin(async { Err(ProviderError::internal("not used")) })
            }
        }
        let (session, events) = captured_session(ProviderPlan::state_only().session_builder(
            "config-owner",
            None,
            None,
        ));
        let schema = DiagnosticFactory.provider_config_attribute_types();
        let declaration =
            crate::parser::IdentifiedCustomTypeDeclaration::new(&schema.parts()["endpoint"])
                .unwrap();
        let value: Value = Value::Concrete(ConcreteValue::String("native-value".into()));
        let known = value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .unwrap()
            .into_classification()
            .into_known()
            .unwrap();
        validate_custom_type_with_factory(
            &DiagnosticFactory,
            declaration,
            &known,
            &session.guest_inputs(),
        )
        .unwrap();
        let events = events.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "the host-installed sink must observe native validation"
        );
        assert!(
            crate::provider_text::present_event(&events[0])
                .to_string()
                .contains("warn [config-owner] validation: native custom-type warning")
        );
    }

    #[test]
    fn declaration_validation_helper_returns_pending_projection_error() {
        let factory = ConfigOnlyCustomTypeFactory;
        let schema = factory.provider_config_attribute_types();
        let declaration =
            crate::parser::IdentifiedCustomTypeDeclaration::new(&schema.parts()["endpoint"])
                .unwrap();
        let pending = Value::root_resource_ref("producer", "identifier", Vec::new());

        let result = validate_custom_type_declaration_with_factory(&factory, declaration, &pending);

        assert!(matches!(
            result,
            Err(crate::parser::CustomTypeValidationError::InvalidDeclarationValue {
                error,
                ..
            }) if matches!(*error, crate::schema::TypeError::Knowledge { .. })
        ));
    }

    #[test]
    fn issue_4276_annotation_reference_stays_offline() {
        use crate::parser::{CustomTypeValidationError, OwnedTypeIdentity};
        let identity = TypeIdentity::bare("Reference");
        let declaration = AttributeType::reference(identity.clone(), None, Some((Some(3), None)));
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(ConfigOnlyCustomTypeFactory)];
        let index = BTreeMap::from([(
            "config-owner".into(),
            OwnerCustomTypeIndex {
                factory_index: 0,
                identity_declarations: BTreeMap::new(),
                declarations: BTreeMap::from([(identity.clone(), vec![declaration])]),
            },
        )]);
        let owned =
            OwnedTypeIdentity::provider("config-owner", identity, RefinedScalarKind::String);
        let resolver = FactoryCustomTypeResolver::with_index(
            factories.as_slice(),
            index,
            &SchemaRegistry::new(),
        );
        // main validates Reference constraints locally and never creates a provider bridge.
        assert_eq!(
            validate_custom_type_annotation(
                &resolver,
                &owned,
                &(string_value("valid"))
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap()
                    .into_known()
                    .unwrap()
            ),
            Ok(())
        );
        assert!(matches!(
            validate_custom_type_annotation(
                &resolver,
                &owned,
                &(string_value("x"))
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap()
                    .into_known()
                    .unwrap()
            ),
            Err(CustomTypeValidationError::InvalidDeclarationValue { .. })
        ));
    }

    #[test]
    fn issue_4276_annotation_unidentified_index_returns_typed_error() {
        use crate::parser::{CustomTypeValidationError, OwnedTypeIdentity};
        let identity = TypeIdentity::bare("Indexed");
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(ConfigOnlyCustomTypeFactory)];
        let index = BTreeMap::from([(
            "config-owner".into(),
            OwnerCustomTypeIndex {
                factory_index: 0,
                identity_declarations: BTreeMap::new(),
                declarations: BTreeMap::from([(identity.clone(), vec![AttributeType::string()])]),
            },
        )]);
        let owned =
            OwnedTypeIdentity::provider("config-owner", identity, RefinedScalarKind::String);
        let resolver = FactoryCustomTypeResolver::with_index(
            factories.as_slice(),
            index,
            &SchemaRegistry::new(),
        );
        let result = validate_custom_type_annotation(
            &resolver,
            &owned,
            &(string_value("value"))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        );
        // main's local declaration check/canonicalizer returns a typed failure here.
        assert!(
            matches!(
                result,
                Err(CustomTypeValidationError::InvalidDeclarationValue { .. })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn provider_custom_type_index_retains_scalar_kinds_for_resolution() {
        use crate::parser::{
            OwnedTypeIdentity, ProviderContext, ProviderCustomTypes, ResourceTypePath, TypeExpr,
        };
        use crate::static_type::StaticTypeResolutionError;

        let identity = TypeIdentity::from_dotted("shared.counter.Count");
        let snapshots = [
            ("one", RefinedScalarKind::Int),
            ("two", RefinedScalarKind::Int),
            ("three", RefinedScalarKind::Float),
        ]
        .map(|(owner, kind)| ProviderFactorySchemaSnapshot {
            name: owner.into(),
            witness: ValidatedProviderSchemaWitness::new(
                Vec::new(),
                ValidatedConfigSchema::validate(BTreeMap::from([(
                    "count".into(),
                    kind.identified_type(identity.clone()),
                )]))
                .unwrap(),
                Default::default(),
                Vec::new(),
            )
            .unwrap(),
        });
        let factories: &[Box<dyn ProviderFactory>] = &[];
        let resolver = FactoryCustomTypeResolver::with_index(
            factories,
            build_owner_custom_type_index(&snapshots),
            &SchemaRegistry::new(),
        );
        let known = resolver.known_custom_types();
        assert_eq!(known.len(), 2, "only owners with the same kind are grouped");
        let int_type = known
            .iter()
            .find(|owned| owned.scalar_kind() == RefinedScalarKind::Int)
            .unwrap();
        assert!(int_type.provider_names().eq(["one", "two"]));
        assert!(known.contains(&OwnedTypeIdentity::provider(
            "three",
            identity,
            RefinedScalarKind::Float
        )));

        let context = ProviderContext {
            custom_types: ProviderCustomTypes::loaded(
                known.into_iter().collect(),
                Box::new(|_, _| Ok(())),
            ),
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        };
        assert!(matches!(
            crate::validation::resolve_type_expr(
                &TypeExpr::DottedUnresolved(ResourceTypePath::new("shared", "counter.Count")),
                &context
            ),
            Err(StaticTypeResolutionError::ConflictingScalarKinds { .. })
        ));
    }

    #[test]
    fn loaded_custom_types_derive_known_config_identities_and_resolver_from_factories() {
        let identity = TypeIdentity::new(Some("test"), ["config"], "Endpoint");
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> =
            std::sync::Arc::new(vec![Box::new(ConfigOnlyCustomTypeFactory)]);
        let custom_types = provider_custom_types_from_factories(factories);

        assert!(
            custom_types
                .known()
                .contains(&crate::parser::OwnedTypeIdentity::provider(
                    "config-owner",
                    identity.clone(),
                    crate::schema::RefinedScalarKind::String
                ))
        );
        crate::parser::validate_custom_type_value(
            &custom_types,
            crate::parser::CustomTypeValidationTarget::Annotation(
                &crate::parser::OwnedTypeIdentity::provider(
                    "config-owner",
                    identity,
                    crate::schema::RefinedScalarKind::String,
                ),
            ),
            &(Value::Concrete(ConcreteValue::String("https://example.test".into())))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        )
        .expect("the same factory source must drive recognition and resolution");
    }

    struct SnapshotCountingFactory {
        schema_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ProviderFactory for SnapshotCountingFactory {
        fn name(&self) -> &str {
            "snapshot"
        }

        fn display_name(&self) -> &str {
            "snapshot fixture"
        }

        fn schemas(&self) -> Vec<ValidatedResourceSchema> {
            vec![
                PatternResourceSchema::new("test.Item")
                    .attribute(
                        PatternAttributeSchema::optional(
                            "identifier",
                            PatternAttributeType::refined_string(
                                Some(TypeIdentity::new(Some("test"), ["item"], "Identifier")),
                                None,
                                None,
                                None,
                            ),
                        )
                        .build(),
                    )
                    .validate()
                    .expect("snapshot fixture schema must validate"),
            ]
        }

        fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
            self.schema_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
                Vec::new(),
            )
            .expect("snapshot fixture must expose one valid aggregate witness")
        }

        fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
            ValidatedConfigSchema::default()
        }

        fn validate_config(
            &self,
            _attributes: &CanonicalProviderConfigAttributes,
            _session: &crate::provider::ProviderValidationInputs,
        ) -> ProviderResult<()> {
            Ok(())
        }

        fn create_provider(
            &self,
            _config: &crate::provider::ProviderSession,
            _initialize_outcome: InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
            Box::pin(async { Err(ProviderError::internal("not used")) })
        }
    }

    #[test]
    fn loaded_provider_snapshot_mints_both_proofs_from_one_schema_capture() {
        let schema_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> =
            std::sync::Arc::new(vec![Box::new(SnapshotCountingFactory {
                schema_calls: std::sync::Arc::clone(&schema_calls),
            })]);

        let snapshot = loaded_provider_snapshot_from_factories(
            factories,
            SchemaRegistry::new(),
            crate::EnvironmentSnapshot::empty(),
        );

        assert_eq!(
            schema_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "schema evidence and the custom-type index must share one capture"
        );
        assert_eq!(snapshot.schema_registry().len(), 1);
        assert!(snapshot.provider_context().known_custom_types().contains(
            &crate::parser::OwnedTypeIdentity::provider(
                "snapshot",
                TypeIdentity::new(Some("test"), ["item"], "Identifier"),
                crate::schema::RefinedScalarKind::String
            )
        ));
    }

    #[test]
    fn aggregate_dispatch_deduplicates_one_shared_validator_pointer() {
        use crate::schema::legacy_validator;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let identity = TypeIdentity::new(Some("test"), ["value"], "Shared");
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&calls);
        let validator = legacy_validator(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let schema = PatternResourceSchema::new("test.Values")
            .attribute(
                PatternAttributeSchema::optional(
                    "first",
                    PatternAttributeType::refined_string_with_validator(
                        identity.clone(),
                        None,
                        None,
                        validator.clone(),
                        None,
                    ),
                )
                .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "second",
                    PatternAttributeType::refined_string_with_validator(
                        identity.clone(),
                        None,
                        None,
                        validator,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .expect("same-kind declarations remain a valid resource schema");

        let table = CustomTypeDispatchTable::build(&[schema], &ValidatedConfigSchema::default())
            .expect("pointer-identical validators are one v13 dispatch binding");
        table
            .dispatch(&identity, "value")
            .expect("the shared callback accepts the value");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn custom_type_scalar_canonicalizer_covers_every_wire_scalar_kind() {
        let identity = TypeIdentity::new(Some("test"), ["widget"], "Mode");
        let enum_type = AttributeType::enum_(
            identity.clone(),
            Some(vec!["API_VALUE".to_string()]),
            vec![("API_VALUE".to_string(), "dsl_value".to_string())],
            None,
            None,
        );
        let cases = vec![
            (
                AttributeType::refined_string(Some(identity.clone()), None, None, None),
                Value::Concrete(ConcreteValue::String("literal".into())),
                "literal",
            ),
            (
                AttributeType::refined_int(Some(identity.clone()), None),
                Value::Concrete(ConcreteValue::Int(-42)),
                "-42",
            ),
            (
                AttributeType::refined_float(Some(identity.clone()), None),
                Value::Concrete(ConcreteValue::Float(crate::finite(1.25))),
                "1.25",
            ),
            (
                enum_type.clone(),
                Value::Concrete(ConcreteValue::enum_identifier("test.widget.Mode.dsl_value")),
                "API_VALUE",
            ),
        ];

        for (declared_type, value, expected) in cases {
            let canonical = canonicalize_custom_type_value(&declared_type, &value)
                .expect("supported scalar must canonicalize")
                .expect("supported scalar must produce bridge text");
            assert_eq!(canonical.as_str(), expected);
        }

        assert!(
            canonicalize_custom_type_value(
                &enum_type,
                &Value::Concrete(ConcreteValue::enum_identifier("unresolved")),
            )
            .is_err(),
            "an unresolved enum identifier must not be treated as canonical"
        );

        let error = canonicalize_custom_type_value(
            &AttributeType::refined_int(Some(identity.clone()), None),
            &Value::Concrete(ConcreteValue::Bool(true)),
        )
        .expect_err("a concrete value with the wrong scalar kind must be rejected");
        assert!(matches!(
            error,
            CustomTypeCanonicalizationError::ConcreteKindMismatch {
                identity: ref actual,
                expected: RefinedScalarKind::Int,
                actual: "boolean",
            } if actual == &identity
        ));
    }

    macro_rules! unexpected_resume_create {
        () => {
            fn resume_create(
                &self,
                _request: ResumeCreateRequest,
            ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
                Box::pin(async { panic!("unexpected resume_create") })
            }
        };
    }

    #[test]
    fn fresh_pending_keeps_the_first_accepted_timestamp_through_poll_and_persistence() {
        let id = ResourceId::with_provider_identity("test", "widget", "example", None);
        let started_at =
            Timestamp::try_from("2026-10-23T08:00:00Z").expect("fixture timestamp is valid");
        let first = PendingCreate::new(
            id,
            PendingToken::new("first-token"),
            CreateProgress::Operating { identifier: None },
            Sent::AllRequestAttributes,
            1,
            started_at.clone(),
        );
        let second = match first.into_poll() {
            Poll::Unissued(attempt) => attempt.pending(PendingToken::new("second-token"), 1),
            Poll::Issued(issued) => {
                panic!("identifier-less pending fixture became issued: {issued:?}")
            }
        };

        let persisted = InFlightCreate::from_pending(second);

        assert_eq!(persisted.started_at(), &started_at);
    }

    #[test]
    fn issued_resume_repend_cannot_replace_the_recorded_identifier() {
        let id = ResourceId::with_provider_identity("test", "widget", "example", None);
        let started_at =
            Timestamp::try_from("2026-10-23T08:00:00Z").expect("fixture timestamp is valid");
        let original = AppliedIdentifier::new("original-id").expect("valid identifier");
        let pending = PendingCreate::new(
            id,
            PendingToken::new("first-token"),
            CreateProgress::Operating {
                identifier: Some(original.clone()),
            },
            Sent::AllRequestAttributes,
            1,
            started_at,
        );
        let resumed = ResumedCreate {
            pending: InFlightCreate::from_pending(pending),
            identifier: original.clone(),
        };
        let Resume::Issued(repended) = resumed.repend(PendingToken::new("second-token")) else {
            panic!("issued fixture must remain an issued resume witness")
        };

        assert_eq!(
            repended.pending.identifier(),
            Some(&original),
            "an issued continuation must keep the identifier it was issued with"
        );
    }

    #[test]
    fn resumed_partial_success_preserves_persisted_sent_authority() {
        use super::HostResumeOutcomeKind as ResumeOutcomeKind;
        let id = ResourceId::with_provider_identity("test", "widget", "example", None);
        let started_at =
            Timestamp::try_from("2026-10-23T08:00:00Z").expect("fixture timestamp is valid");
        let identifier = AppliedIdentifier::new("persisted-id").expect("valid identifier");
        let pending = PendingCreate::new(
            id,
            PendingToken::new("persisted-token"),
            CreateProgress::ReadingBack {
                identifier: identifier.clone(),
            },
            Sent::AllRequestAttributes,
            1,
            started_at,
        )
        .persist(
            PersistedAttributes::default(),
            PersistedAttributes::default(),
        );
        let diagnostic: PartialReadDiagnostic = PartialReadDiagnostic::new(
            crate::provider_text::ProviderDiagnostic::message(
                "read-back remained incomplete".into(),
            ),
            (vec!["computed".to_string()])
                .try_into()
                .expect("nonempty partial attributes"),
        );

        let outcome = ResumedCreate {
            pending,
            identifier: identifier.clone(),
        }
        .partial(
            HashMap::from([(
                "arn".to_string(),
                Value::Concrete(ConcreteValue::String("known-arn".into())),
            )]),
            diagnostic.clone(),
        );
        let diagnostic = diagnostic.into_host_for_test();
        let outcome = outcome.into_host_for_test();
        let ResumeOutcomeKind::PartialSuccess(partial) = outcome.into_kind() else {
            panic!("resumed partial read-back must remain distinct from a resent create")
        };
        let (created, state, actual_diagnostic, read_back_end) = partial.into_parts();

        let SentRecord::Persisted(envelope) = created.sent() else {
            panic!("resumed partial read-back fabricated request sent authority")
        };
        assert_eq!(
            envelope.token().expose_for_provider_call(),
            "persisted-token"
        );
        assert_eq!(envelope.identifier(), Some(&identifier));
        assert_eq!(actual_diagnostic, diagnostic);
        assert_eq!(read_back_end, ReadBackEnd::ProviderReported);
        assert_eq!(
            state
                .partial_read
                .as_ref()
                .expect("partial result retains a state marker")
                .missing_attributes(),
            &BTreeSet::from(["computed".to_string()])
        );
        assert_eq!(
            state.attributes.get("arn"),
            Some(&(Value::Concrete(ConcreteValue::String("known-arn".into()))).to_snapshot())
        );
    }

    #[test]
    fn classified_create_failure_display_is_context_only() {
        use std::error::Error as _;

        let id = ResourceId::with_provider_identity("test", "widget", "example", None);
        let failure = CreateAttempt::new(id)
            .0
            .not_created(ProviderError::api_error("leaf create failure"));

        let failure = failure.into_host_for_test();
        assert_eq!(failure.to_string(), "create failed and left no resource");
        assert_eq!(
            failure.source().map(ToString::to_string).as_deref(),
            Some("leaf create failure")
        );
    }

    #[tokio::test]
    async fn guest_create_failure_after_egress_is_outcome_unknown() {
        let id = ResourceId::with_provider_identity("test", "widget", "example", None);
        let started = CreateAttempt::new(id.clone()).0.start_guest_call();
        started.egress_recorder().begin_request().dispatched();

        let failure = started
            .finish()
            .failed(ProviderError::internal("guest trapped"))
            .await
            .classify();

        let HostCreateFailureKind::OutcomeUnknown { unknown, error } =
            failure.into_host_for_test().into_kind()
        else {
            panic!("a sent guest call must not be classified as not_created")
        };
        assert_eq!(unknown.id(), &id);
        assert_eq!(error.to_string(), "guest trapped");
    }

    #[tokio::test]
    async fn guest_create_failure_without_egress_is_not_created() {
        let id = ResourceId::with_provider_identity("test", "widget", "example", None);
        let failure = CreateAttempt::new(id)
            .0
            .start_guest_call()
            .finish()
            .failed(ProviderError::internal("guest trapped"))
            .await
            .classify();

        let HostCreateFailureKind::NoResource(error) = failure.into_host_for_test().into_kind()
        else {
            panic!("a guest call with no dispatched request must remain not_created")
        };
        assert_eq!(error.to_string(), "guest trapped");
    }

    #[tokio::test]
    async fn guest_decode_failure_waits_for_pending_egress_decision() {
        let id = ResourceId::with_provider_identity("test", "widget", "example", None);
        let started = CreateAttempt::new(id).0.start_guest_call();
        let pending = started.egress_recorder().begin_request();
        let classification = tokio::spawn(async move {
            started
                .finish()
                .decode((), |()| {
                    Err::<(), _>(ProviderError::internal("malformed guest result"))
                })
                .await
                .expect_err("the guest result must fail decoding")
        });

        tokio::task::yield_now().await;
        assert!(
            !classification.is_finished(),
            "classification must wait for the host's dispatch decision"
        );
        drop(pending);

        let failure = classification.await.expect("classification task");
        let HostCreateFailureKind::NoResource(error) = failure.into_host_for_test().into_kind()
        else {
            panic!("a host-denied pending request must not count as egress")
        };
        assert_eq!(error.to_string(), "malformed guest result");
    }

    #[test]
    fn outcome_unknown_requires_the_egress_sent_witness() {
        let id = ResourceId::with_provider_identity("test", "widget", "example", None);
        let failure = CreateAttempt::new(id.clone()).0.outcome_unknown(
            EgressSent::for_test(),
            ProviderError::internal("returned value could not be decoded"),
        );

        let HostCreateFailureKind::OutcomeUnknown { unknown, error } =
            failure.into_host_for_test().into_kind()
        else {
            panic!("the egress witness must mint an outcome-unknown failure")
        };
        assert_eq!(unknown.id(), &id);
        assert_eq!(error.to_string(), "returned value could not be decoded");

        let pending_id = ResourceId::with_provider_identity("test", "widget", "pending", None);
        let (attempt, receiver) = CreateAttempt::new(pending_id.clone());
        let started = attempt.start_guest_call();
        let pending = started.egress_recorder().begin_request();
        drop(started);
        let reports = receiver.drain();
        let [CancelledCreateReport::OutcomeUnknown(unknown)] = reports.as_slice() else {
            panic!("a dropped call with an unresolved dispatch must report one unknown outcome")
        };
        assert_eq!(unknown.id(), &pending_id);
        drop(pending);
    }

    #[test]
    fn dropped_started_guest_create_reports_only_the_two_cancellation_states() {
        let sent_id = ResourceId::with_provider_identity("test", "widget", "sent", None);
        let (attempt, receiver) = CreateAttempt::new(sent_id.clone());
        let started = attempt.start_guest_call();
        started.egress_recorder().begin_request().dispatched();
        drop(started);
        let reports = receiver.drain();
        let [CancelledCreateReport::OutcomeUnknown(unknown)] = reports.as_slice() else {
            panic!("a dropped call after dispatch must report one unknown outcome")
        };
        assert_eq!(unknown.id(), &sent_id);

        let unsent_id = ResourceId::with_provider_identity("test", "widget", "unsent", None);
        let (attempt, receiver) = CreateAttempt::new(unsent_id);
        drop(attempt.start_guest_call());
        assert!(matches!(
            receiver.drain().as_slice(),
            [CancelledCreateReport::NotCreated]
        ));
    }

    #[test]
    fn classified_resume_failure_display_is_context_only() {
        use std::error::Error as _;

        let failure = ResumeFailure::no_resource_from_verdict(
            ProviderError::not_found("leaf resume failure")
                .with_secrets(&crate::provider_text::RedactionSet::for_test(&[])),
        );

        assert_eq!(failure.to_string(), "resumed create left no resource");
        assert_eq!(
            failure.source().map(ToString::to_string).as_deref(),
            Some("leaf resume failure")
        );
    }

    #[test]
    fn provider_text_post_create_note_sanitizes_provider_identifier() {
        let raw = format!("\x1b]52;c;ZXZpbA==\x07\x1b[2J{}", "x".repeat(20 * 1024));
        let identified: IdentifiedCreate = IdentifiedCreate::from_create_observation(
            ResourceId::with_provider_identity("test", "widget", "example", None),
            AppliedIdentifier::new(&raw).unwrap(),
            CreateObservation::Unobserved(UnobservedReason::NotFound),
            SentRecord::Request(Sent::AllRequestAttributes),
        );
        let note = identified.post_error_note();
        assert!(!note.contains(['\x1b', '\x07']));
        assert!(note.contains("\\u{1b}"));
        assert!(note.len() <= crate::provider_text::MAX_PROVIDER_TEXT_BYTES);
        assert_eq!(identified.identifier().as_str(), raw);
    }

    #[test]
    fn create_identified_post_error_note_indents_multiline_reason() {
        let id = ResourceId::with_provider_identity("test", "widget", "example", None);
        let identified: HostIdentifiedCreate = IdentifiedCreate::from_create_observation(
            id,
            AppliedIdentifier::new("widget-id").expect("fixture identifier is non-empty"),
            CreateObservation::Unobserved(UnobservedReason::ReadFailed(
                ProviderError::api_error("read failed\nrequest id: req-123\nretry later")
                    .with_secrets(&crate::provider_text::RedactionSet::for_test(&[])),
            )),
            SentRecord::Request(Sent::AllRequestAttributes),
        );

        assert_eq!(
            identified.post_error_note(),
            "test reported identifier 'widget-id' but the resource could not be observed; recorded in state so the next plan re-checks it\n  reason: read failed\n  request id: req-123\n  retry later"
        );
    }

    #[test]
    fn enum_canonicalization_error_renders_its_reason_once() {
        let mut resources: Vec<Resource> = vec![
            Resource::with_provider("test", "widget", "example", None).with_attribute(
                "mode",
                Value::Concrete(ConcreteValue::enum_identifier("test.widget.Mode.invalid")),
            ),
        ];
        let error = crate::value::canonicalize_resources_with_schemas(
            &mut resources,
            &SchemaRegistry::new(),
        )
        .expect_err("an untyped enum identifier must be rejected");
        let leaf_reason =
            "unresolved enum identifier `test.widget.Mode.invalid` survived canonicalization";

        let provider_error = HostError::from(error);

        {
            use crate::provider::HostError as ProviderError;
            assert!(matches!(provider_error, ProviderError::InvalidInput(_)));
        }
        assert_eq!(
            provider_error.detail().context(),
            "Failed to canonicalize enum value"
        );
        assert!(std::error::Error::source(&provider_error).is_none());
        assert!(provider_error.detail().cause().is_some());
        assert_eq!(
            provider_error.to_string().matches(&leaf_reason).count(),
            1,
            "the leaf TypeError must render exactly once"
        );
    }

    #[test]
    fn marker_shaped_strings_remain_plain_in_persisted_attributes() {
        let marker = format!("_secret:argon2:{}", "ab".repeat(32));
        let encoded = serde_json::json!({ "token": marker });
        let persisted = PersistedAttributes::from_encoded(
            serde_json::from_value(encoded.clone()).expect("attribute object"),
        )
        .expect("marker-shaped strings are ordinary persisted values");
        let plain = Value::Concrete(ConcreteValue::String(marker.clone().into()));
        assert_eq!(persisted.get("token"), Some((plain.clone()).to_snapshot()));

        let persisted = PersistedAttributes::from_encoded(
            serde_json::from_value(encoded).expect("attribute object"),
        )
        .expect("state row decode");
        assert_eq!(persisted.get("token"), Some((plain).to_snapshot()));
    }

    #[test]
    fn invalid_persisted_attribute_preserves_decode_error_as_source_only() {
        let error = PersistedAttributes::from_encoded(HashMap::from([(
            "payload".to_string(),
            serde_json::json!({"$unknown": true}),
        )]))
        .expect_err("unknown host payload must be rejected");

        assert_eq!(
            error.to_string(),
            "persisted attribute \"payload\" is not valid persisted JSON",
        );
        let source = std::error::Error::source(&error).expect("decode error is the source");
        assert!(source.downcast_ref::<PersistedValueDecodeError>().is_some());
        assert!(!error.to_string().contains(&source.to_string()));
    }

    #[test]
    fn persisted_wire_guest_round_trip_preserves_user_keys_enums_and_secrets() {
        fn host_value_to_guest_wire(
            value: crate::resource::SnapshotValue,
        ) -> crate::resource::SnapshotValue {
            match value {
                crate::resource::Value::Concrete(
                    crate::resource::ConcreteValue::CanonicalEnum(value),
                ) => crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                    value.api_value().to_string().into(),
                )),
                crate::resource::Value::Concrete(crate::resource::ConcreteValue::Map(values)) => {
                    crate::resource::Value::Concrete(crate::resource::ConcreteValue::Map(
                        values
                            .into_iter()
                            .map(|(key, value)| (key, host_value_to_guest_wire(value)))
                            .collect(),
                    ))
                }
                crate::resource::Value::Concrete(crate::resource::ConcreteValue::List(values)) => {
                    crate::resource::Value::Concrete(crate::resource::ConcreteValue::List(
                        values.into_iter().map(host_value_to_guest_wire).collect(),
                    ))
                }
                value => value,
            }
        }

        let enum_value = Value::Concrete(ConcreteValue::CanonicalEnum(
            CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("test"), ["widget"], "Mode"),
                "ACTIVE",
            ),
        ));
        let enum_shaped_user_map = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "identity".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    (
                        "provider".to_string(),
                        Value::Concrete(ConcreteValue::String("user".into())),
                    ),
                    (
                        "segments".to_string(),
                        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                            ConcreteValue::String("data".into()),
                        )])),
                    ),
                    (
                        "kind".to_string(),
                        Value::Concrete(ConcreteValue::String("Payload".into())),
                    ),
                ]))),
            ),
            (
                "api_value".to_string(),
                Value::Concrete(ConcreteValue::String("user-value".into())),
            ),
        ])));
        let payload = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "$or".to_string(),
                Value::Concrete(ConcreteValue::String("event-pattern".into())),
            ),
            (
                "$$x".to_string(),
                Value::Concrete(ConcreteValue::String("double-dollar".into())),
            ),
            ("$enum".to_string(), enum_shaped_user_map),
        ])));
        let secret = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String(
                "guest-secret".into(),
            )))
            .into(),
        ));

        let persisted = PersistedAttributes::from_persisted_values(HashMap::from([
            (
                "payload".to_string(),
                PersistedValue::encode_provider_value(&payload, None).expect("encode payload"),
            ),
            (
                "mode".to_string(),
                PersistedValue::encode_provider_value(&enum_value, None).expect("encode enum"),
            ),
            (
                "token".to_string(),
                PersistedValue::encode_provider_value(&secret, None).expect("encode secret"),
            ),
        ]))
        .expect("host saved attributes");
        let mut expected = persisted.to_values();
        expected.insert(
            "mode".to_string(),
            Value::Concrete(ConcreteValue::String("ACTIVE".into())).to_snapshot(),
        );

        let wire_values: HashMap<_, _> = persisted
            .to_values()
            .into_iter()
            .map(|(key, value)| (key, host_value_to_guest_wire(value)))
            .collect();
        let guest = PersistedAttributes::from_typed_persisted_wire_values(wire_values)
            .expect("guest must reconstruct host-decoded saved attributes");

        assert_eq!(guest.to_values(), expected);
    }

    #[test]
    fn typed_persisted_wire_values_preserve_explicit_legacy_digest() {
        let digest = format!("argon2-legacy:73616c74:{}", "ab".repeat(32));
        let hash =
            RedactedSecretHash::from_encoded_digest(&digest).expect("valid explicit legacy digest");
        let typed = crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
            crate::resource::SnapshotLeaf::RedactedSecret { hash },
        ));
        let guest = PersistedAttributes::from_typed_persisted_wire_values(HashMap::from([(
            "token".to_string(),
            typed.clone(),
        )]))
        .expect("typed persisted wire values must encode");

        assert_eq!(guest.get("token"), Some(typed));
        assert_eq!(
            guest.encoded_value("token").map(PersistedValue::as_json),
            Some(&serde_json::json!({ "$secret": digest })),
        );
    }

    #[test]
    fn state_loaded_fallback_salt_legacy_digest_is_self_contained_and_verifiable() {
        let material = br#""fallback-write-only""#;
        let mut output = [0_u8; 32];
        argon2::Argon2::default()
            .hash_password_into(material, b"carina-secret-v1", &mut output)
            .expect("hash pre-v12 fallback-salt fixture");
        let encoded =
            crate::resource::legacy_secret_migration::migrate_legacy_state_attribute_for_test(
                serde_json::json!(format!("_secret:argon2:{}", hex::encode(output))),
                "mock.test.resource",
                "old",
                "password",
            )
            .expect("migrate row-bound legacy digest");
        let persisted =
            PersistedValue::from_serialized_json(encoded).expect("migrated structural secret");
        let crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
            crate::resource::SnapshotLeaf::RedactedSecret { hash },
        )) = persisted.decode().expect("typed migrated secret")
        else {
            panic!("migration must produce a typed redacted secret")
        };

        assert!(hash.verify(material));
        assert!(hash.encoded_digest().starts_with("argon2-legacy:"));
    }

    #[test]
    fn state_row_upgrade_rejects_legacy_salt_too_long_for_its_digest_format() {
        let error =
            crate::resource::legacy_secret_migration::migrate_legacy_state_attribute_for_test(
                serde_json::json!(format!("_secret:argon2:{}", "ab".repeat(32))),
                "mock.test.resource",
                &"x".repeat(5000),
                "token",
            )
            .expect_err("an upgraded legacy digest must never exceed the parser's salt bound");
        match error {
            crate::resource::legacy_secret_migration::LegacySecretMigrationError::StateAttribute {
                key,
                source,
            } => {
                assert_eq!(key, "token");
                assert!(matches!(
                    *source,
                    crate::resource::legacy_secret_migration::LegacySecretMigrationError::LegacySaltTooLong {
                        actual,
                        maximum: 4096,
                    } if actual > 4096
                ));
            }
            other => panic!("unexpected migration error: {other:?}"),
        }
    }

    #[test]
    fn saved_plan_hash_inventory_ignores_non_placeholder_siblings() {
        let secret = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String(
                "SAVED_PLAN_SECRET".into(),
            )))
            .into(),
        ));
        let placeholder =
            crate::value::redact_secrets_in_value(&secret).expect("redact saved-plan secret");
        let attributes = HashMap::from([
            (
                "tags".to_string(),
                crate::resource::SnapshotValue::Concrete(crate::resource::ConcreteValue::Map(
                    IndexMap::from([
                        ("S".to_string(), placeholder.clone()),
                        (
                            "Vpc".to_string(),
                            (Value::root_resource_ref("vpc", "identifier", Vec::new()))
                                .to_snapshot(),
                        ),
                    ]),
                )),
            ),
            (
                "items".to_string(),
                crate::resource::SnapshotValue::Concrete(crate::resource::ConcreteValue::List(
                    vec![
                        Value::root_resource_ref("vpc", "identifier", Vec::new()).to_snapshot(),
                        placeholder,
                    ],
                )),
            ),
        ]);

        let hashes = PersistedAttributes::from_saved_plan_attributes(&attributes);
        let tags = hashes
            .encoded_value("tags")
            .and_then(|value| value.as_json().as_object())
            .expect("sparse secret inventory object");
        assert!(
            tags.get("S")
                .and_then(|value| value.get(crate::value::PERSISTED_SECRET_KEY))
                .and_then(serde_json::Value::as_str)
                .and_then(RedactedSecretHash::from_encoded_digest)
                .is_some(),
        );
        assert!(!tags.contains_key("Vpc"));
        let items = hashes
            .encoded_value("items")
            .and_then(|value| value.as_json().as_array())
            .expect("sparse secret inventory list");
        assert!(items[0].is_null(), "list indexes must remain stable");
        assert!(
            items[1]
                .get(crate::value::PERSISTED_SECRET_KEY)
                .and_then(serde_json::Value::as_str)
                .and_then(RedactedSecretHash::from_encoded_digest)
                .is_some(),
        );
    }

    #[test]
    fn persistence_merges_propagate_typed_unavailable_secret() {
        let secret = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String(
                "previous secret".into(),
            )))
            .into(),
        ));
        let previous = PersistedValue::encode_desired_value(&secret, None).unwrap();
        let opaque = previous.decode().unwrap();
        let provider = PersistedValue::encode_provider_value(
            &crate::resource::SnapshotValue::Concrete(crate::resource::ConcreteValue::Map(
                IndexMap::from([("token".into(), opaque)]),
            )),
            None,
        )
        .unwrap();
        let error = hash_provider_json_as_secret(&provider, Some(&previous)).unwrap_err();
        assert!(
            matches!(error, SerializationError::UnavailableSecret(ref error)
            if error.path() == "(secret)")
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("token"), "{rendered}");
        }
        let map = |value: &PersistedValue| {
            PersistedValue::from_serialized_json(serde_json::json!({"protected": value.as_json()}))
                .unwrap()
        };
        let list = |value: &PersistedValue| {
            PersistedValue::from_serialized_json(serde_json::json!([value.as_json()])).unwrap()
        };
        for (previous, provider) in [
            (previous.clone(), provider.clone()),
            (map(&previous), map(&provider)),
            (list(&previous), list(&provider)),
        ] {
            let error =
                merge_previous_secret_hashes_into_provider_value(&previous, Some(&provider))
                    .expect_err("an unavailable provider secret must not retain stale evidence");
            assert!(
                matches!(error, SerializationError::UnavailableSecret(ref error)
                if error.path() == "(secret)")
            );
            for rendered in [error.to_string(), format!("{error:?}")] {
                assert!(!rendered.contains("token"), "{rendered}");
            }
        }
    }

    #[test]
    fn provider_secret_hash_drops_null_fields_before_hashing() {
        let provider_json = serde_json::json!({ "a": "x", "b": null });
        let provider_value = PersistedValue::from_serialized_json(provider_json).unwrap();
        let encoded_secret = hash_provider_json_as_secret(&provider_value, None)
            .expect("hash canonical provider secret");
        let digest = encoded_secret
            .as_json()
            .get(crate::value::PERSISTED_SECRET_KEY)
            .and_then(serde_json::Value::as_str)
            .and_then(RedactedSecretHash::from_encoded_digest)
            .expect("typed secret digest");
        let desired_material =
            serde_json::to_vec(&serde_json::json!({ "a": "x" })).expect("serialize desired secret");

        assert!(
            digest.verify(desired_material),
            "provider-only null fields must not create a phantom secret diff",
        );
    }

    #[test]
    fn default_tag_encoding_requires_classified_input() {
        let value: Value = Value::Concrete(ConcreteValue::Int(7));
        let proof = value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(assert_default_tag_value_encodable(&proof).is_ok());
    }

    #[test]
    fn default_tag_opacity_preserves_attempted_failure_inside_secret() {
        let unavailable = Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![],
            reason: crate::resource::UnavailableReason::from_builtin_error("attempted".into()),
            source_line: 0.into(),
        });
        let protected = Value::Deferred(DeferredValue::Secret(Box::new(unavailable).into()));
        let config = ProviderConfig {
            name: "aws".into(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::from([("Protected".into(), protected.clone())]),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let mut resources: Vec<Resource> = vec![Resource::with_provider(
            "aws",
            "service.Widget",
            "example",
            None,
        )];
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            PatternResourceSchema::new("service.Widget")
                .attribute(
                    PatternAttributeSchema::optional(
                        "tags",
                        PatternAttributeType::map(PatternAttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        merge_default_tags(&config, &mut resources, &registry).unwrap();
        assert_eq!(
            resources[0].get_attr("tags"),
            Some(&Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "Protected".into(),
                protected
            )]))))
        );
    }

    #[test]
    fn merge_default_tags_skips_unresolved_values() {
        let config = ProviderConfig {
            name: "aws".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::from([
                (
                    "ManagedBy".to_string(),
                    Value::Concrete(ConcreteValue::String("carina".into())),
                ),
                (
                    "Owner".to_string(),
                    Value::root_resource_ref("management", "owner", Vec::new()),
                ),
                (
                    "Count".to_string(),
                    crate::integer_arithmetic::evaluate_integer_values(
                        Value::root_resource_ref("management", "count", Vec::new()),
                        crate::integer_arithmetic::IntegerOp::Add,
                        Value::Concrete(ConcreteValue::Int(1)),
                        crate::integer_arithmetic::IntegerOperatorSpan::synthetic(),
                    )
                    .expect("deferred integer fixture must be structurally valid"),
                ),
            ]),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let resource: Resource = Resource::with_provider("aws", "service.Widget", "example", None);
        let mut resources = vec![resource];
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (PatternResourceSchema::new("service.Widget").attribute(
                PatternAttributeSchema::optional(
                    "tags",
                    PatternAttributeType::map(PatternAttributeType::string()),
                )
                .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        merge_default_tags(&config, &mut resources, &registry).unwrap();

        assert_eq!(
            resources[0].get_attr("tags"),
            Some(&Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "ManagedBy".to_string(),
                Value::Concrete(ConcreteValue::String("carina".into())),
            )]))))
        );
        assert!(resources[0].default_tag_keys.contains("ManagedBy"));
        assert!(!resources[0].default_tag_keys.contains("Owner"));
        assert!(!resources[0].default_tag_keys.contains("Count"));
        assert!(
            resources[0]
                .attributes
                .keys()
                .all(|key| !key.starts_with('_')),
            "host-owned default-tag provenance must not enter provider attributes"
        );
    }

    #[test]
    fn merge_default_tags_remerge_is_idempotent_and_never_removes_existing_tags() {
        let mut config = ProviderConfig {
            name: "aws".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::from([(
                "ManagedBy".to_string(),
                Value::Concrete(ConcreteValue::String("carina".into())),
            )]),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let resource: Resource = Resource::with_provider("aws", "service.Widget", "example", None)
            .with_attribute(
                "tags",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "Name".to_string(),
                    Value::Concrete(ConcreteValue::String("example".into())),
                )]))),
            );
        let mut resources = vec![resource];
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (PatternResourceSchema::new("service.Widget").attribute(
                PatternAttributeSchema::optional(
                    "tags",
                    PatternAttributeType::map(PatternAttributeType::string()),
                )
                .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        merge_default_tags(&config, &mut resources, &registry).unwrap();
        let Some(Value::Concrete(ConcreteValue::Map(tags))) = resources[0].get_attr_mut("tags")
        else {
            panic!("merged tags must remain a concrete map");
        };
        tags.insert(
            "ManagedBy".to_string(),
            Value::Concrete(ConcreteValue::String("rehydrated-carina".into())),
        );
        // Simulate provenance decoded from an untrusted saved plan. Even if it
        // claims an authored tag, merging must never delete that tag.
        resources[0].default_tag_keys.insert("Name".to_string());
        let before_remerge = resources[0].clone();

        merge_default_tags(&config, &mut resources, &registry).unwrap();

        assert_eq!(
            resources[0], before_remerge,
            "re-merging the same defaults must not overwrite rehydrated values, remove tags, or change provenance"
        );

        config.default_tags.insert(
            "ManagedBy".to_string(),
            Value::root_resource_ref("management", "owner", Vec::new()),
        );
        config.default_tags.insert(
            "Team".to_string(),
            Value::Concrete(ConcreteValue::String("platform".into())),
        );
        merge_default_tags(&config, &mut resources, &registry).unwrap();

        let Some(Value::Concrete(ConcreteValue::Map(tags))) = resources[0].get_attr("tags") else {
            panic!("merged tags must remain a concrete map");
        };
        assert_eq!(
            tags.get("ManagedBy"),
            Some(&Value::Concrete(ConcreteValue::String(
                "rehydrated-carina".into()
            ))),
            "a skipped default must not overwrite its existing tag"
        );
        assert_eq!(
            tags.get("Name"),
            Some(&Value::Concrete(ConcreteValue::String("example".into()))),
            "pre-existing provenance must never cause an authored tag to be removed"
        );
        assert_eq!(
            tags.get("Team"),
            Some(&Value::Concrete(ConcreteValue::String("platform".into()))),
            "the valid default keeps the merge loop non-vacuous"
        );
        assert!(resources[0].default_tag_keys.contains("ManagedBy"));
        assert!(resources[0].default_tag_keys.contains("Name"));
        assert!(resources[0].default_tag_keys.contains("Team"));
    }

    fn widget_status_registry() -> SchemaRegistry {
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (PatternResourceSchema::new("service.Widget").attribute(
                PatternAttributeSchema::optional(
                    "status",
                    PatternAttributeType::enum_(
                        crate::schema::enum_identity("Status", Some("aws.service.Widget")),
                        Some(vec!["Enabled".to_string()]),
                        vec![("Enabled".to_string(), "enabled".to_string())],
                        None,
                        None,
                    ),
                )
                .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        registry
    }

    #[test]
    fn lift_covers_orphan_entries_absent_from_any_resource_slice() {
        let orphan = ResourceId::with_provider_identity("aws", "service.Widget", "orphan", None);
        let raw = crate::test_support::raw_saved_attrs(HashMap::from([(
            orphan.clone(),
            HashMap::from([(
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("Enabled".into())),
            )]),
        )]));

        let lifted = raw.lift(&widget_status_registry());

        assert!(matches!(
            &lifted.get(&orphan).unwrap()["status"],
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::CanonicalEnum(value))
                if value.api_value() == "Enabled"
        ));
    }

    #[test]
    fn raw_saved_attrs_lift_is_map_driven_and_remappable() {
        let from = ResourceId::with_provider_identity("aws", "service.Widget", "old", None);
        let to = ResourceId::with_provider_identity("aws", "service.Widget", "new", None);
        let raw = crate::test_support::raw_saved_attrs(HashMap::from([(
            from.clone(),
            HashMap::from([(
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("Enabled".into())),
            )]),
        )]));
        let registry = widget_status_registry();

        let mut lifted = raw.lift(&registry);
        assert!(matches!(
            &lifted.get(&from).unwrap()["status"],
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::CanonicalEnum(value))
                if value.api_value() == "Enabled"
        ));

        lifted.remap_resource_id(&from, to.clone());
        assert!(lifted.get(&from).is_none());
        assert!(lifted.get(&to).is_some());
    }

    #[test]
    fn raw_saved_attrs_lift_structurally_canonicalizes_nested_values() {
        let statement = PatternAttributeType::struct_(
            "Statement".to_string(),
            vec![
                PatternStructField::optional(
                    "action",
                    PatternAttributeType::union(vec![
                        PatternAttributeType::string(),
                        PatternAttributeType::list(PatternAttributeType::string()),
                    ]),
                )
                .build(),
            ],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (PatternResourceSchema::new("iam.policy")
                .attribute(PatternAttributeSchema::optional("statement", statement).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let id = ResourceId::with_provider_identity("aws", "iam.policy", "p1", None);
        let raw = crate::test_support::raw_saved_attrs(HashMap::from([(
            id.clone(),
            HashMap::from([(
                "statement".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "action".to_string(),
                    Value::Concrete(ConcreteValue::String("s3:GetObject".into())),
                )]))),
            )]),
        )]));

        let lifted = raw.lift(&registry);
        let crate::resource::Value::Concrete(crate::resource::ConcreteValue::Map(statement)) =
            &lifted.get(&id).unwrap()["statement"]
        else {
            panic!("statement must remain a map");
        };
        assert_eq!(
            statement["action"],
            Value::Concrete(ConcreteValue::StringList(vec!["s3:GetObject".into()])).to_snapshot(),
            "carina#3740: RawSavedAttrs::lift must apply the shared structural canonicalizer"
        );
    }

    #[test]
    fn raw_saved_attrs_lift_preserves_attribute_absent_from_schema() {
        let id = ResourceId::with_provider_identity("aws", "service.Widget", "widget", None);
        let original = Value::Concrete(ConcreteValue::String("opaque".into()));
        let raw = crate::test_support::raw_saved_attrs(HashMap::from([(
            id.clone(),
            HashMap::from([("provider_extension".to_string(), original.clone())]),
        )]));

        let lifted = raw.lift(&widget_status_registry());

        assert_eq!(
            lifted.get(&id).unwrap()["provider_extension"],
            (original).to_snapshot(),
            "carina#3740: a saved attribute absent from a known resource schema must pass through"
        );
    }

    #[test]
    fn raw_saved_attrs_lift_skips_resource_type_absent_from_registry() {
        let id = ResourceId::with_provider_identity("aws", "service.Unknown", "unknown", None);
        let original = Value::Concrete(ConcreteValue::String("Enabled".into()));
        let raw = crate::test_support::raw_saved_attrs(HashMap::from([(
            id.clone(),
            HashMap::from([("status".to_string(), original.clone())]),
        )]));

        let lifted = raw.lift(&widget_status_registry());

        assert_eq!(
            lifted.get(&id).unwrap()["status"],
            (original).to_snapshot(),
            "carina#3740: a saved entry whose resource type is absent from the registry must be skipped"
        );
    }

    #[test]
    fn raw_saved_attrs_lift_is_idempotent() {
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (PatternResourceSchema::new("service.Widget")
                .attribute(
                    PatternAttributeSchema::optional(
                        "status",
                        PatternAttributeType::enum_(
                            crate::schema::enum_identity("Status", Some("aws.service.Widget")),
                            Some(vec!["Enabled".to_string()]),
                            vec![("Enabled".to_string(), "enabled".to_string())],
                            None,
                            None,
                        ),
                    )
                    .build(),
                )
                .attribute(
                    PatternAttributeSchema::optional(
                        "subjects",
                        PatternAttributeType::union(vec![
                            PatternAttributeType::string(),
                            PatternAttributeType::list(PatternAttributeType::string()),
                        ]),
                    )
                    .build(),
                ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let id = ResourceId::with_provider_identity("aws", "service.Widget", "widget", None);
        let raw = crate::test_support::raw_saved_attrs(HashMap::from([(
            id.clone(),
            HashMap::from([
                (
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("Enabled".into())),
                ),
                (
                    "subjects".to_string(),
                    Value::Concrete(ConcreteValue::String("repo:foo:*".into())),
                ),
            ]),
        )]));

        let once = raw.lift(&registry);
        let twice = crate::test_support::raw_saved_attrs(once.attributes.0.clone()).lift(&registry);
        let sorted_pairs = |attrs: &HashMap<String, crate::resource::SnapshotValue>| {
            let mut pairs: Vec<_> = attrs
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            pairs.sort_by(|(left, _), (right, _)| left.cmp(right));
            pairs
        };

        assert_eq!(
            sorted_pairs(once.get(&id).unwrap()),
            sorted_pairs(twice.get(&id).unwrap()),
            "carina#3740: applying RawSavedAttrs::lift twice must reach the same fixpoint"
        );
    }

    #[test]
    fn raw_saved_attrs_lift_preserves_redacted_secret_in_string_or_list_union() {
        let config = PatternAttributeType::struct_(
            "Config".to_string(),
            vec![
                PatternStructField::optional(
                    "password",
                    PatternAttributeType::union(vec![
                        PatternAttributeType::string(),
                        PatternAttributeType::list(PatternAttributeType::string()),
                    ]),
                )
                .build(),
            ],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            (PatternResourceSchema::new("service.Widget")
                .attribute(PatternAttributeSchema::optional("config", config).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let id = ResourceId::with_provider_identity("test", "service.Widget", "widget", None);
        let secret_hash: crate::resource::SnapshotValue =
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
                crate::resource::SnapshotLeaf::RedactedSecret {
                    hash: RedactedSecretHash::generate(b"saved-attrs-test"),
                },
            ));
        let raw = crate::test_support::raw_saved_attrs(HashMap::from([(
            id.clone(),
            HashMap::from([(
                "config".to_string(),
                crate::resource::SnapshotValue::Concrete(crate::resource::ConcreteValue::Map(
                    IndexMap::from([("password".to_string(), secret_hash.clone())]),
                )),
            )]),
        )]));

        let lifted = raw.lift(&registry);
        let crate::resource::Value::Concrete(crate::resource::ConcreteValue::Map(config)) =
            &lifted.get(&id).unwrap()["config"]
        else {
            panic!("config must remain a map");
        };
        assert_eq!(
            config["password"], secret_hash,
            "carina#3740: a persisted secret hash is an opaque scalar sentinel, never a list"
        );
    }

    #[test]
    fn raw_saved_attrs_lift_uses_state_text_for_union_wrapped_enum_identifier() {
        use crate::schema::enum_identity;

        // Persisted JSON strings currently decode as ConcreteValue::String, so
        // EnumIdentifier is a seam-only phase probe rather than a reachable
        // state-file shape. StateText case-matches `foo` to API member `FOO`;
        // RawDsl would treat it as the alias for the different member `API_A`.
        // The Union wrapper ensures the separate enum-lift walker does not
        // perform the conversion first.
        let mode = PatternAttributeType::enum_(
            enum_identity("Mode", Some("awscc.example.Widget")),
            Some(vec!["API_A".to_string(), "FOO".to_string()]),
            vec![("API_A".to_string(), "foo".to_string())],
            None,
            None,
        );
        let settings = PatternAttributeType::struct_(
            "Settings".to_string(),
            vec![
                PatternStructField::optional(
                    "mode",
                    PatternAttributeType::union(vec![
                        mode.clone(),
                        PatternAttributeType::list(mode),
                    ]),
                )
                .build(),
            ],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "awscc",
            (PatternResourceSchema::new("example.Widget")
                .attribute(PatternAttributeSchema::optional("settings", settings).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let id = ResourceId::with_provider_identity("awscc", "example.Widget", "widget", None);
        let raw = crate::test_support::raw_saved_attrs(HashMap::from([(
            id.clone(),
            HashMap::from([(
                "settings".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "mode".to_string(),
                    Value::Concrete(ConcreteValue::enum_identifier("foo")),
                )]))),
            )]),
        )]));

        let lifted = raw.lift(&registry);
        let crate::resource::Value::Concrete(crate::resource::ConcreteValue::Map(settings)) =
            &lifted.get(&id).unwrap()["settings"]
        else {
            panic!("settings must remain a map");
        };
        let crate::resource::Value::Concrete(crate::resource::ConcreteValue::CanonicalEnum(value)) =
            &settings["mode"]
        else {
            panic!("enum state text must be lifted to CanonicalEnum");
        };
        assert_eq!(
            value.api_value(),
            "FOO",
            "carina#3740: saved-side structural canonicalization must use StateText semantics \
             when an EnumIdentifier reaches an Enum through a Union"
        );
    }

    fn resolved_for_test(resource: Resource) -> ProviderReadyResource {
        let bindings = crate::binding_index::ResolvedBindings::default();
        let gate = crate::executor::ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = crate::test_utils::preparation_schemas(
            &resource.id,
            crate::schema::SchemaKind::Resource,
        );
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |type_checks| {
                let context = crate::executor::ProviderPreparationContext::new(
                    &bindings,
                    &gate,
                    &[],
                    &NoopNormalizer,
                    &[],
                    &schemas,
                    type_checks,
                );
                futures::executor::block_on(crate::executor::prepare_provider_ready_resource(
                    resource, &context,
                ))
                .expect("test resource should pass provider preparation")
            },
        )
    }

    fn ready_data_source_for_test(resource: DataSource) -> ProviderReadyDataSource {
        let bindings = crate::binding_index::ResolvedBindings::default();
        let gate = crate::executor::ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = crate::test_utils::preparation_schemas(
            &resource.id,
            crate::schema::SchemaKind::DataSource,
        );
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |type_checks| {
                let context = crate::executor::ProviderPreparationContext::new(
                    &bindings,
                    &gate,
                    &[],
                    &NoopNormalizer,
                    &[],
                    &schemas,
                    type_checks,
                );
                crate::executor::prepare_provider_ready_data_source(resource, &context)
                    .expect("test data source should pass provider preparation")
            },
        )
    }

    enum DiagnosticEmission {
        Task,
        Blocking,
        Thread,
        Overlapping(Arc<tokio::sync::Barrier>),
    }
    struct AsyncDiagnosticFloodProvider {
        diagnostics: crate::provider_text::ProviderDiagnosticChannel,
        emission: DiagnosticEmission,
        inner: MockProvider,
    }
    fn emit_native_flood(
        diagnostics: &crate::provider_text::ProviderDiagnosticChannel,
        count: usize,
        context: &'static str,
    ) {
        let diagnostic = ProviderDiagnostic::message("detached native warning".into());
        for _ in 0..count {
            diagnostics.emit(crate::provider_text::ProviderEvent::diagnostic(
                crate::provider_text::EventLevel::Warn,
                context.into(),
                &diagnostic,
            ));
        }
    }
    impl Provider for AsyncDiagnosticFloodProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn read(
            &self,
            id: &ResourceId,
            identifier: &AppliedIdentifier,
            _: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            let id = id.clone();
            let diagnostics = self.diagnostics.clone();
            let (count, context) = if identifier.as_str() == "first" {
                (300, "first-read")
            } else {
                (600, "second-read")
            };
            Box::pin(async move {
                match &self.emission {
                    DiagnosticEmission::Task => tokio::spawn(async move {
                        emit_native_flood(&diagnostics, 2048, "detached-task");
                    })
                    .await
                    .unwrap(),
                    DiagnosticEmission::Blocking => tokio::task::spawn_blocking(move || {
                        emit_native_flood(&diagnostics, 2048, "detached-blocking");
                    })
                    .await
                    .unwrap(),
                    DiagnosticEmission::Thread => std::thread::spawn(move || {
                        emit_native_flood(&diagnostics, 2048, "detached-thread");
                    })
                    .join()
                    .unwrap(),
                    DiagnosticEmission::Overlapping(barrier) => {
                        barrier.wait().await;
                        let diagnostic =
                            ProviderDiagnostic::message("overlapping native warning".into());
                        for _ in 0..count {
                            diagnostics.emit(crate::provider_text::ProviderEvent::diagnostic(
                                crate::provider_text::EventLevel::Warn,
                                context.into(),
                                &diagnostic,
                            ));
                            tokio::task::yield_now().await;
                        }
                    }
                }
                Ok(ReadOutcome::not_found(id))
            })
        }
        fn read_data_source(
            &self,
            resource: &ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.inner.read_data_source(resource)
        }
        fn create(
            &self,
            id: &ResourceId,
            request: CreateRequest,
            attempt: CreateAttempt,
        ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
            self.inner.create(id, request, attempt)
        }
        unexpected_resume_create!();
        fn update(
            &self,
            id: &ResourceId,
            identifier: &AppliedIdentifier,
            request: UpdateRequest,
            attempt: UpdateAttempt,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            self.inner.update(id, identifier, request, attempt)
        }
        fn delete(&self, attempt: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
            self.inner.delete(attempt)
        }
        fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
            self.inner.required_permissions(id, op)
        }
    }
    fn captured_async_flood(
        emission: DiagnosticEmission,
    ) -> (
        ProviderBoundary<'static>,
        Arc<std::sync::Mutex<Vec<crate::provider_text::BoundProviderEvent>>>,
    ) {
        let (session, events) =
            captured_session(ProviderPlan::state_only().session_builder("mock", None, None));
        let mut boundary = ProviderBoundary::fixture(AsyncDiagnosticFloodProvider {
            diagnostics: session.diagnostics(),
            emission,
            inner: MockProvider,
        });
        boundary.set_fixture_session(session, &SchemaRegistry::new());
        (boundary, events)
    }

    #[tokio::test]
    async fn detached_native_flood_flushes_once_when_the_owner_drops() {
        for emission in [
            DiagnosticEmission::Task,
            DiagnosticEmission::Blocking,
            DiagnosticEmission::Thread,
        ] {
            let (boundary, events) = captured_async_flood(emission);
            let id = ResourceId::with_provider_identity("mock", "test", "detached", None);
            boundary
                .read(&id, &fixture_applied_identifier("detached"), ReadRequest)
                .await
                .unwrap();
            let admitted = {
                let captured = events.lock().unwrap();
                assert!(captured.iter().all(|event| {
                    crate::provider_text::present_event(event)
                        .to_string()
                        .contains("detached native warning")
                }));
                assert!(
                    (1..=256).contains(&captured.len()),
                    "the spare budget must bound the flood"
                );
                captured.len()
            };
            drop(boundary); // Drops the last host-owned session/normalizer inputs.
            let captured = events.lock().unwrap();
            let text = captured
                .iter()
                .map(|event| crate::provider_text::present_event(event).to_string())
                .collect::<Vec<_>>();
            assert_eq!(
                text.iter()
                    .filter(|event| event.contains("detached native warning"))
                    .count(),
                admitted
            );
            assert_eq!(
                text.iter()
                    .filter(|event| event.contains("provider events suppressed"))
                    .collect::<Vec<_>>(),
                vec![&format!("{} provider events suppressed", 2048 - admitted)]
            );
            assert_eq!(captured.len(), admitted + 1);
        }
    }

    #[test]
    fn fixture_normalizer_owns_live_diagnostic_inputs() {
        let normalizer = ConfiguredNormalizer::fixture(NoopNormalizer);
        let inputs: &ProviderValidationInputs = &normalizer.diagnostic_inputs;
        assert!(
            inputs
                .diagnostics()
                .register_protected(
                    carina_provider_protocol::diagnostics::ProtectedMaterial::GithubToken(
                        "fixture-token".into()
                    )
                )
                .is_ok(),
            "fixture inputs must retain their diagnostic owner"
        );
    }

    #[tokio::test]
    async fn overlapping_native_reads_have_independent_budgets_and_notices() {
        let (boundary, events) = captured_async_flood(DiagnosticEmission::Overlapping(Arc::new(
            tokio::sync::Barrier::new(2),
        )));
        let id = ResourceId::with_provider_identity("mock", "test", "overlapping", None);
        let first = fixture_applied_identifier("first");
        let second = fixture_applied_identifier("second");
        let (first_result, second_result) = tokio::join!(
            boundary.read(&id, &first, ReadRequest),
            boundary.read(&id, &second, ReadRequest),
        );
        first_result.unwrap();
        second_result.unwrap();
        drop(boundary);
        let captured = events.lock().unwrap();
        let text = captured
            .iter()
            .map(|event| crate::provider_text::present_event(event).to_string())
            .collect::<Vec<_>>();
        let mut expected = Vec::new();
        for (context, total) in [("first-read", 300), ("second-read", 600)] {
            let admitted = text.iter().filter(|event| event.contains(context)).count();
            assert!(
                (64..=256).contains(&admitted),
                "each call must admit its own quota: {context}: {admitted}"
            );
            expected.push(format!("{} provider events suppressed", total - admitted));
        }
        let mut notices = text
            .into_iter()
            .filter(|event| event.contains("provider events suppressed"))
            .collect::<Vec<_>>();
        notices.sort();
        expected.sort();
        assert_eq!(
            notices, expected,
            "one count-only notice for each overlapping call"
        );
    }

    struct DiagnosticFloodProvider {
        diagnostics: crate::provider_text::ProviderDiagnosticChannel,
        inner: MockProvider,
    }
    impl Provider for DiagnosticFloodProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn read(
            &self,
            id: &ResourceId,
            identifier: &AppliedIdentifier,
            request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            // Emit before returning the future too: dispatch must scope invocation,
            // not only the future's subsequent polls.
            let diagnostic = ProviderDiagnostic::message("native flood warning".into());
            for _ in 0..300 {
                self.diagnostics
                    .emit(crate::provider_text::ProviderEvent::diagnostic(
                        crate::provider_text::EventLevel::Warn,
                        "native-call".into(),
                        &diagnostic,
                    ));
            }
            self.inner.read(id, identifier, request)
        }
        fn read_data_source(
            &self,
            resource: &ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.inner.read_data_source(resource)
        }
        fn create(
            &self,
            id: &ResourceId,
            request: CreateRequest,
            attempt: CreateAttempt,
        ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
            self.inner.create(id, request, attempt)
        }
        unexpected_resume_create!();
        fn update(
            &self,
            id: &ResourceId,
            identifier: &AppliedIdentifier,
            request: UpdateRequest,
            attempt: UpdateAttempt,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            self.inner.update(id, identifier, request, attempt)
        }
        fn delete(&self, attempt: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
            self.inner.delete(attempt)
        }
        fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
            self.inner.required_permissions(id, op)
        }
    }

    #[tokio::test]
    async fn native_calls_have_independent_budgets_and_one_notice_each() {
        let (session, events) =
            captured_session(ProviderPlan::state_only().session_builder("mock", None, None));
        let mut boundary = ProviderBoundary::fixture(DiagnosticFloodProvider {
            diagnostics: session.diagnostics(),
            inner: MockProvider,
        });
        boundary.set_fixture_session(session, &SchemaRegistry::new());
        let id = ResourceId::with_provider_identity("mock", "test", "calls", None);
        for _ in 0..2 {
            boundary
                .read(&id, &fixture_applied_identifier("calls"), ReadRequest)
                .await
                .unwrap();
            let captured = std::mem::take(&mut *events.lock().unwrap());
            let rendered = captured
                .iter()
                .map(|event| crate::provider_text::present_event(event).to_string())
                .collect::<Vec<_>>();
            let admitted = rendered
                .iter()
                .filter(|text| text.contains("native flood warning"))
                .count();
            assert!(admitted > 0, "each native call must receive a fresh budget");
            assert_eq!(
                rendered
                    .iter()
                    .filter(|text| text.contains("provider events suppressed"))
                    .collect::<Vec<_>>(),
                vec![&format!("{} provider events suppressed", 300 - admitted)]
            );
        }
    }

    // Mock Provider for testing
    struct MockProvider;

    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
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
            resource: &ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(
            &self,
            id: &ResourceId,
            request: CreateRequest,
            _attempt: CreateAttempt,
        ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
            let id = id.clone();
            let attrs = request.resource().as_resource().attributes.clone();
            Box::pin(async move {
                let state =
                    AppliedState::new(id, "mock-id-123", crate::resource::attrs_to_hashmap(&attrs))
                        .expect("fixture identifier is non-empty");
                Ok(CreateOutcome::fixture_success(state))
            })
        }

        unexpected_resume_create!();

        fn update(
            &self,
            id: &ResourceId,
            identifier: &AppliedIdentifier,
            request: UpdateRequest,
            _attempt: crate::provider::UpdateAttempt,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            let id = id.clone();
            let identifier = identifier.clone();
            // Apply the patch on top of `from` so the test sees changes and
            // write-only reassertions round-tripped into State.
            let mut attrs = request.from().attributes.clone();
            for op in &request.patch().ops {
                match op {
                    PatchOp::Add { key, value }
                    | PatchOp::Replace { key, value }
                    | PatchOp::Reassert { key, value } => {
                        attrs.insert(key.clone(), (value.clone()).to_snapshot());
                    }
                    PatchOp::Remove { key } => {
                        attrs.remove(key);
                    }
                }
            }
            Box::pin(async move {
                Ok(UpdateOutcome::success(AppliedState::from_snapshot_parts(
                    id, identifier, attrs,
                )))
            })
        }

        fn delete(
            &self,
            _attempt: crate::provider::DeleteAttempt,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn mock_provider_read_returns_not_found() {
        let provider = MockProvider;
        let id = ResourceId::with_identity("test", "example");
        let identifier = fixture_applied_identifier("mock-id");
        let state = provider
            .read(&id, &identifier, ReadRequest)
            .await
            .unwrap()
            .into_state();
        assert!(!state.exists);
    }

    #[test]
    fn create_outcome_exposes_state_and_diagnostic() {
        let id = ResourceId::with_identity("test", "example");
        let state = AppliedState::new(id.clone(), "mock-id", HashMap::new())
            .expect("fixture identifier is non-empty");
        let expected = State::existing(id, HashMap::new()).with_identifier("mock-id");
        let diagnostic: PartialReadDiagnostic = PartialReadDiagnostic::new(
            crate::provider_text::ProviderDiagnostic::message("mock partial create".into()),
            (vec!["dns_name".to_string()])
                .try_into()
                .expect("nonempty partial attributes"),
        );

        let success: CreateOutcome = CreateOutcome::fixture_success(state.clone());
        let success = success.into_host_for_test();
        assert_eq!(success.diagnostic(), None);
        assert_eq!(success.identifier().as_str(), "mock-id");
        assert_eq!(success.into_state_for_writeback(), expected);

        let partial = CreateOutcome::partial_success(
            state.clone(),
            crate::provider_text::ProviderDiagnostic::message("mock partial create".into()),
            (vec!["dns_name".to_string()])
                .try_into()
                .expect("nonempty partial attributes"),
        );

        let diagnostic = diagnostic.into_host_for_test();
        let partial = partial.into_host_for_test();
        assert_eq!(partial.diagnostic(), Some(&diagnostic));
        assert_eq!(partial.identifier().as_str(), "mock-id");
        let state = partial.into_state_for_writeback();
        assert_eq!(
            state
                .partial_read
                .expect("partial outcome records a marker")
                .missing_attributes()
                .len(),
            1
        );
    }

    #[test]
    fn partial_create_and_update_share_the_canonical_pending_reread_value() {
        use crate::resource::{ApplyTimeUnknown, PendingReRead, ReReadReason};

        fn assert_materialized(
            mut state: crate::resource::SnapshotState,
            expected: &PendingReRead,
        ) {
            let marker = state
                .partial_read
                .as_ref()
                .expect("partial outcome records a marker");
            assert_eq!(marker.pending(), expected);
            assert_eq!(
                marker.missing_attributes(),
                &BTreeSet::from(["computed".to_string()])
            );

            state.materialize_partial_read_unknowns();
            let unknown = state
                .attributes
                .get("computed")
                .expect("missing attribute is materialized");
            let crate::resource::Value::Deferred(crate::resource::DeferredValue::Unknown(
                ApplyTimeUnknown::PendingReRead(pending),
            )) = unknown
            else {
                panic!(
                    "expected the marker to materialize a pending re-read unknown, got {unknown:?}"
                );
            };
            assert_eq!(pending, expected);
            let rendered =
                crate::value::render_unknown(&ApplyTimeUnknown::<
                    crate::resource::AbsoluteBindingAddress,
                >::PendingReRead(pending.clone()));
            assert_eq!(
                rendered,
                "(unknown until next refresh: read after apply was incomplete — provider returned a partial read)"
            );
            assert!(!rendered.contains("known after next apply"), "{rendered}");
            assert!(!rendered.contains("post-create"), "{rendered}");
        }

        let id = ResourceId::with_identity("test", "example");
        let applied = || {
            AppliedState::new(id.clone(), "mock-id", HashMap::new())
                .expect("fixture identifier is non-empty")
        };
        let expected = PendingReRead::new(ReReadReason::Incomplete {
            detail: "provider returned a partial read".into(),
        });

        let created = CreateOutcome::partial_success(
            applied(),
            crate::provider_text::ProviderDiagnostic::message(
                "provider returned a partial read".into(),
            ),
            (vec!["computed".to_string()])
                .try_into()
                .expect("nonempty partial attributes"),
        )
        .with_secrets(&RedactionSet::for_test(&[]));
        assert_materialized(created.into_state_for_writeback(), &expected);

        let updated = UpdateOutcome::partial_success(
            applied(),
            crate::provider_text::ProviderDiagnostic::message(
                "provider returned a partial read".into(),
            ),
            (vec!["computed".to_string()])
                .try_into()
                .expect("nonempty partial attributes"),
        )
        .with_secrets(&RedactionSet::for_test(&[]));
        assert_materialized(updated.into_state_for_writeback(), &expected);
    }

    #[test]
    fn native_partial_diagnostic_rejects_empty_missing_attributes() {
        assert!(NonEmptyMissingAttributes::try_from(Vec::<String>::new()).is_err());
    }

    #[test]
    fn applied_state_rejects_empty_identifier() {
        let id = ResourceId::with_identity("test", "example");
        assert_eq!(
            AppliedState::new(id, "", HashMap::new()),
            Err(EmptyAppliedIdentifier)
        );
    }

    #[test]
    fn applied_identifier_deserialization_rejects_empty_string() {
        let error = serde_json::from_str::<AppliedIdentifier>("\"\"")
            .expect_err("deserialization must preserve the non-empty invariant");
        assert!(error.to_string().contains("must not be empty"), "{error}");
    }

    #[test]
    fn host_deadline_provenance_is_not_provider_diagnostic_identity() {
        let provider_reported = PartialReadDiagnostic::new(
            crate::provider_text::ProviderDiagnostic::message(
                "read-back remained incomplete".into(),
            ),
            (vec!["computed".to_string()])
                .try_into()
                .expect("nonempty partial attributes"),
        )
        .with_secrets(&RedactionSet::for_test(&[]));
        let encoded = serde_json::to_value(&provider_reported).unwrap();
        let round_tripped: HostPartialReadDiagnostic =
            serde_json::from_value::<PartialReadDiagnostic>(encoded.clone())
                .unwrap()
                .with_secrets(&RedactionSet::for_test(&[]));

        assert_eq!(
            encoded,
            serde_json::json!({
                "reason": "read-back remained incomplete",
                "missing_attributes": ["computed"],
            }),
            "provider diagnostic formats must not carry host provenance",
        );
        assert_eq!(
            provider_reported, round_tripped,
            "a persisted diagnostic retains only provider-reported data",
        );

        let id = ResourceId::with_identity("test", "deadline-roundtrip");
        let state = AppliedState::new(id, "provider-id", HashMap::new())
            .expect("the fixture identifier is non-empty");
        let mut provider_outcome = Some(UpdateOutcome::partial_success(
            state,
            crate::provider_text::ProviderDiagnostic::message(
                "read-back remained incomplete".into(),
            ),
            (vec!["computed".to_string()])
                .try_into()
                .expect("nonempty partial attributes"),
        ));
        let provider_kind = provider_outcome.as_ref().unwrap().kind().clone();
        let driven =
            futures::executor::block_on(carina_provider_protocol::drive_pending_infallible(
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(30),
                std::future::ready(carina_provider_protocol::OperationStep::<
                    UpdateOutcome,
                    (),
                    (),
                >::Pending {
                    pending: (),
                    poll_after_secs: 1,
                    phase: carina_provider_protocol::PendingPhase::ReadBack,
                }),
                |(), finality| {
                    assert_eq!(
                        finality,
                        carina_provider_protocol::PollFinality::FinalAttempt
                    );
                    std::future::ready(carina_provider_protocol::OperationStep::Done(
                        provider_outcome
                            .take()
                            .expect("the final attempt runs exactly once"),
                    ))
                },
                |_| std::future::ready(()),
            ));
        let carina_provider_protocol::DriveOutcome::Done(completed) = driven else {
            panic!("the final attempt must complete with a partial update")
        };
        let host_outcome = UpdateOutcome::from_host_completion(completed);
        assert_eq!(host_outcome.read_back_end(), ReadBackEnd::HostDeadline);

        assert_eq!(
            host_outcome.kind(),
            &provider_kind,
            "host provenance does not change the provider-reported outcome kind",
        );

        let host_outcome = host_outcome.into_host_for_test();
        let writeback = host_outcome.into_state_for_writeback();
        let encoded_state = serde_json::to_string(&writeback).unwrap();
        assert!(!encoded_state.contains("HostDeadline"));
        let round_tripped_state: State = serde_json::from_str(&encoded_state).unwrap();
        assert!(
            round_tripped_state.partial_read.is_some(),
            "the partial-read marker survives without same-run budget provenance",
        );
    }

    #[test]
    fn provider_default_satisfier_hint_is_empty() {
        let provider = MockProvider;
        let id = ResourceId::with_identity("test", "example");
        assert_eq!(
            provider
                .satisfier_hint(&id, &AttrPath::single("status"))
                .expect("the default satisfier hint should succeed"),
            Vec::new()
        );
    }

    /// A provider that only accepts a data source when the full `Resource`
    /// carrying its input attributes is delivered. Exercises the new
    /// `read_data_source` path introduced to enable resources like
    /// `identitystore.user` that look themselves up by user-provided inputs.
    struct InputAwareProvider;

    impl Provider for InputAwareProvider {
        fn name(&self) -> &str {
            "input-aware"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            let id = id.clone();
            // Intentionally fails if the data source path doesn't deliver
            // the full Resource — the regular `read` route has no access
            // to input attributes.
            Box::pin(async move {
                Err(ProviderError::internal("read cannot see inputs").for_resource(id))
            })
        }

        fn read_data_source(
            &self,
            resource: &ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            // Echoes the resource's input attributes back into state so the
            // test can assert they were delivered.
            let id = resource.id.clone();
            let attrs = resource.attributes.clone();
            Box::pin(async move {
                Ok(State::existing(
                    id,
                    crate::resource::attrs_to_hashmap(&attrs),
                ))
            })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            attempt: CreateAttempt,
        ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
            Box::pin(
                async move { Err(attempt.not_created(ProviderError::internal("not supported"))) },
            )
        }

        unexpected_resume_create!();

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
            _attempt: crate::provider::UpdateAttempt,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }

        fn delete(
            &self,
            _attempt: crate::provider::DeleteAttempt,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn read_data_source_handles_zero_input_without_managed_identifier() {
        // Data-source reads have their own boundary and do not manufacture a
        // managed-resource identifier merely to call `read`.
        let provider = MockProvider;
        let resource = ready_data_source_for_test(DataSource::new("test", "example"));
        let state = provider.read_data_source(&resource).await.unwrap();
        assert!(!state.exists);
    }

    #[tokio::test]
    async fn read_data_source_override_receives_full_resource() {
        // InputAwareProvider's override echoes the resource attributes into
        // state. If dispatch actually uses the override, the echoed input is
        // visible; if it falls through to read() by mistake, the test fails
        // because InputAwareProvider::read returns an error.
        let provider = InputAwareProvider;
        let mut resource = DataSource::new("identitystore.user", "mizzy");
        resource.set_attr(
            "identity_store_id".to_string(),
            Value::Concrete(ConcreteValue::String("d-9567916d09".into())),
        );
        resource.set_attr(
            "user_name".to_string(),
            Value::Concrete(ConcreteValue::String("gosukenator@gmail.com".into())),
        );
        let resource = ready_data_source_for_test(resource);

        let state = provider.read_data_source(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(
            state.attributes.get("identity_store_id"),
            Some(&Value::Concrete(ConcreteValue::String(
                "d-9567916d09".into()
            )))
        );
        assert_eq!(
            state.attributes.get("user_name"),
            Some(&Value::Concrete(ConcreteValue::String(
                "gosukenator@gmail.com".into()
            )))
        );
    }

    #[tokio::test]
    async fn box_dyn_provider_forwards_read_data_source_override() {
        // When the provider is held as Box<dyn Provider>, the override must
        // still be called. Without explicit forwarding in the impl, Rust
        // would use the trait's default impl and bypass the override.
        let provider: Box<dyn Provider> = Box::new(InputAwareProvider);
        let mut resource = DataSource::new("identitystore.user", "mizzy");
        resource.set_attr(
            "user_name".to_string(),
            Value::Concrete(ConcreteValue::String("x".into())),
        );
        let resource = ready_data_source_for_test(resource);
        let state = provider.read_data_source(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(
            state.attributes.get("user_name"),
            Some(&Value::Concrete(ConcreteValue::String("x".into())))
        );
    }

    #[tokio::test]
    async fn provider_router_dispatches_read_data_source_to_override() {
        // The router must route read_data_source to the underlying provider
        // so that overrides work across provider boundaries.
        let mut router = ProviderRouter::new();
        router.add_provider("input-aware".to_string(), initialized(InputAwareProvider));

        let mut resource =
            DataSource::with_provider("input-aware", "identitystore.user", "mizzy", None);
        resource.set_attr(
            "user_name".to_string(),
            Value::Concrete(ConcreteValue::String("x".into())),
        );
        let resource = ready_data_source_for_test(resource);
        let state = router.read_data_source(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(
            state.attributes.get("user_name"),
            Some(&Value::Concrete(ConcreteValue::String("x".into())))
        );
    }

    #[tokio::test]
    async fn mock_provider_create_returns_existing() {
        let provider = MockProvider;
        let resource = Resource::new("test", "example");
        let provider_boundary = crate::provider::ProviderBoundary::fixture(&provider);
        let prepared = crate::replacement::create_permit_for_test(CreateRequest::checked(
            resolved_for_test(resource),
        ));
        let (create, _cancellation) =
            crate::executor::dispatch_create(&provider_boundary, prepared);
        let state = create.await.unwrap().into_state_for_writeback();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("mock-id-123".to_string()));
    }

    #[tokio::test]
    async fn provider_router_dispatches_read_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), initialized(MockProvider));

        let id = ResourceId::with_provider_identity("mock", "test", "example", None);
        let identifier = fixture_applied_identifier("mock-id");
        let state = router
            .read(&id, &identifier, ReadRequest)
            .await
            .unwrap()
            .into_state();
        assert!(!state.exists);
    }

    #[tokio::test]
    async fn provider_router_attaches_provider_context_to_normalizer_errors() {
        struct FailingNormalizer;

        impl ProviderNormalizer for FailingNormalizer {
            fn normalize_desired<'a>(
                &'a self,
                _resources: &'a mut [ProviderResource],
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }

            fn normalize_state<'a>(
                &'a self,
                _current_states: &'a mut StateTransformMap,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                Box::pin(async { Err(ProviderError::internal("normalization failed")) })
            }

            fn hydrate_read_state<'a>(
                &'a self,
                _current_states: &'a mut StateTransformMap,
                _saved_attrs: &'a SavedAttrs,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }
        }

        let mut router = ProviderRouter::new();
        router.add_normalizer("awscc", Box::new(FailingNormalizer));

        let mut states = StateTransformMap::from_states(HashMap::new());
        let error = router
            .normalize_state(&mut states)
            .await
            .expect_err("normalizer failure must propagate");

        assert_eq!(
            error
                .detail()
                .provider_name
                .as_ref()
                .map(|text| text.as_diagnostic())
                .as_deref(),
            Some("awscc")
        );
    }

    #[tokio::test]
    async fn provider_router_dispatches_create_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), initialized(MockProvider));

        let resource = Resource::with_provider("mock", "test", "example", None);
        let prepared = crate::replacement::create_permit_for_test(CreateRequest::checked(
            resolved_for_test(resource),
        ));
        let (create, _cancellation) = crate::executor::dispatch_create(&router, prepared);
        let state = create.await.unwrap().into_state_for_writeback();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("mock-id-123".to_string()));
    }

    #[test]
    fn provider_error_is_terminal_but_keeps_typed_cause() {
        use std::error::Error;
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::api_error_caused_by("Failed to create resource", cause)
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        assert!(err.source().is_none(), "ProviderError must be terminal");
        let cause = err
            .detail()
            .cause()
            .expect("typed cause must remain available");
        assert_eq!(cause.to_string(), "connection refused");
    }

    #[test]
    fn provider_error_display_includes_cause() {
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::api_error_caused_by("Failed to create resource", cause)
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        let display = format!("{}", err);
        assert!(
            display.contains("connection refused"),
            "Display should include cause message, got: {}",
            display
        );
    }

    #[test]
    fn provider_error_display_without_cause() {
        let err = ProviderError::internal("simple error")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        let display = format!("{}", err);
        assert_eq!(display, "simple error");
    }

    #[test]
    fn provider_error_throttling_classifies_raw_wire_message_code_and_cause() {
        for marker in ["ThrottlingException", "Rate exceeded"] {
            for prefix in ["x".repeat(9 * 1024), "\x1b".repeat(1500)] {
                let raw = format!("{prefix} {marker}");
                for (message, code, cause) in [
                    (raw.clone(), None, None),
                    ("request failed".into(), Some(raw.clone()), None),
                    ("request failed".into(), None, Some(raw.clone())),
                ] {
                    let error = ProviderError::from_wire(WireProviderError {
                        kind: WireProviderErrorKind::ApiError,
                        message,
                        resource_id: None,
                        cause,
                        provider_name: None,
                        operation: None,
                        status: None,
                        code,
                        request_id: None,
                    });
                    assert!(!error.to_string().contains(marker));
                    assert!(error.is_throttled(), "missing raw marker {marker}");
                }
            }
        }
    }

    #[test]
    fn provider_error_throttling_classifies_every_native_constructor() {
        let message_constructors: [fn(String) -> ProviderError; 5] = [
            ProviderError::invalid_input,
            ProviderError::api_error,
            ProviderError::not_found,
            ProviderError::timeout,
            ProviderError::internal,
        ];
        let cause_constructors: [fn(&'static str, std::io::Error) -> ProviderError; 5] = [
            ProviderError::invalid_input_caused_by,
            ProviderError::api_error_caused_by,
            ProviderError::not_found_caused_by,
            ProviderError::timeout_caused_by,
            ProviderError::internal_caused_by,
        ];
        for marker in ["ThrottlingException", "Rate exceeded"] {
            let raw = format!("{} {marker}", "x".repeat(9 * 1024));
            for construct in message_constructors {
                let error = construct(raw.clone());
                assert!(!error.to_string().contains(marker));
                assert!(error.is_throttled());
            }
            for construct in cause_constructors {
                let error = construct("request failed", std::io::Error::other(raw.clone()));
                assert!(!error.to_string().contains(marker));
                assert!(error.is_throttled());
            }
        }
    }

    #[test]
    fn provider_error_throttling_uses_raw_code_and_preserves_classification_on_redaction() {
        let raw = format!("{} ThrottlingException", "x".repeat(9 * 1024));
        let error = ProviderError::api_error("request failed")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .with_code(raw.clone());
        assert!(!error.to_string().contains("ThrottlingException"));
        assert!(error.is_throttled());
        assert!(!error.with_code("AccessDenied").is_throttled());

        let error = ProviderError::api_error(raw)
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .with_redacted_message("redacted")
            .with_code("AccessDenied");
        assert_eq!(error.detail().context(), "redacted");
        assert!(error.is_throttled());
    }

    #[test]
    fn provider_error_throttling_keeps_existing_case_sensitive_marker_semantics() {
        for message in [
            "prefix ThrottlingException suffix",
            "prefix Rate exceeded suffix",
        ] {
            assert!(ProviderError::api_error(message).is_throttled());
        }
        for message in [
            "throttlingexception",
            "rate exceeded",
            "Throttling",
            "TooManyRequests",
            "RequestLimitExceeded",
            "SlowDown",
        ] {
            assert!(!ProviderError::api_error(message).is_throttled());
            assert!(
                !ProviderError::api_error("request failed")
                    .with_code(message)
                    .is_throttled()
            );
        }
        assert!(
            !ProviderError::api_error("request failed")
                .with_status(429)
                .is_throttled()
        );
    }

    #[test]
    fn provider_error_throttling_classifies_nested_typed_causes() {
        #[derive(Debug)]
        struct ServiceError(std::io::Error);

        impl std::fmt::Display for ServiceError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("service error")
            }
        }

        impl std::error::Error for ServiceError {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let raw = format!("{} Rate exceeded", "x".repeat(9 * 1024));
        let error = ProviderError::api_error_caused_by(
            "request failed",
            ServiceError(std::io::Error::other(raw.clone())),
        );
        assert!(!error.to_string().contains("Rate exceeded"));
        assert!(error.is_throttled());

        let nested =
            ProviderError::internal_caused_by("operation failed", ProviderError::api_error(raw));
        assert!(!nested.to_string().contains("Rate exceeded"));
        assert!(nested.is_throttled());
    }

    #[test]
    fn provider_error_throttling_inspects_original_diagnostic_cause() {
        let raw = format!("{} ThrottlingException", "x".repeat(9 * 1024));
        let diagnostic = ProviderDiagnostic::new(std::io::Error::other(raw));
        assert!(!diagnostic.to_string().contains("ThrottlingException"));
        let error = ProviderError::api_error_caused_by("request failed", diagnostic);
        assert!(error.is_throttled());
    }

    #[test]
    fn host_error_cause_is_not_transported_over_the_provider_protocol() {
        let suspended_input = SuspendedValidationId::from_keyed_fingerprint([7_u8; 32]);
        let error = ProviderError::timeout("suspended")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .with_host_cause(HostErrorCause::ValidationSuspended(suspended_input));
        assert_eq!(
            error.detail().host_cause(),
            Some(HostErrorCause::ValidationSuspended(suspended_input))
        );

        let decoded = ProviderError::from_wire(error.to_wire())
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));

        assert_eq!(decoded.kind(), ProviderErrorKind::Timeout);
        assert_eq!(decoded.to_string(), "suspended");
        assert_eq!(decoded.detail().host_cause(), None);
    }

    #[test]
    fn suspended_validation_id_is_comparable_hashable_and_debug_redacted() {
        let first = SuspendedValidationId::from_keyed_fingerprint([1_u8; 32]);
        let same = SuspendedValidationId::from_keyed_fingerprint([1_u8; 32]);
        let second = SuspendedValidationId::from_keyed_fingerprint([2_u8; 32]);

        assert_eq!(first, same);
        assert_ne!(first, second);
        let ids = std::collections::HashSet::from([first, same, second]);
        assert_eq!(ids.len(), 2);

        let debug = format!("{first:?}");
        assert_eq!(debug, "SuspendedValidationId(<redacted>)");
    }

    #[test]
    fn provider_error_display_and_wire_include_external_cause_chain_once() {
        #[derive(Debug)]
        struct ServiceError {
            source: std::io::Error,
        }

        impl std::fmt::Display for ServiceError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("service error")
            }
        }

        impl std::error::Error for ServiceError {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.source)
            }
        }

        let error = ProviderError::api_error_caused_by(
            "request failed",
            ServiceError {
                source: std::io::Error::other("throttled"),
            },
        )
        .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));

        let rendered = error.to_string();
        assert_eq!(rendered, "request failed: service error: throttled");
        assert_eq!(rendered.matches("throttled").count(), 1, "{rendered}");

        let wire_cause = error.to_wire().cause.expect("wire cause");
        assert_eq!(wire_cause, "service error: throttled");
        assert_eq!(wire_cause.matches("throttled").count(), 1, "{wire_cause}");

        let structured = ProviderError::api_error_caused_by(
            "request failed",
            ServiceError {
                source: std::io::Error::other("throttled"),
            },
        )
        .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
        .with_operation("s3.PutObject");
        assert_eq!(
            structured.to_string(),
            "request failed\n  operation: s3.PutObject\n  cause: service error: throttled"
        );
    }

    #[test]
    fn provider_error_keeps_in_repo_terminal_cause_unchanged() {
        let cause = crate::value::SecretRehydrationSiteError::new(
            "config.token",
            crate::value::SecretRehydrationError::HashMismatch,
        );
        let error = ProviderError::internal_caused_by("verification failed", cause)
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        let expected =
            "verification failed: config.token: the secret changed since the plan was created";

        assert_eq!(error.to_string(), expected);
        assert_eq!(
            error.to_wire().cause.as_deref(),
            Some("config.token: the secret changed since the plan was created")
        );
    }

    #[test]
    fn nested_provider_error_source_renders_leaf_once_in_display_and_wire() {
        #[derive(Debug)]
        struct ProviderBoundaryError(HostError);

        impl std::fmt::Display for ProviderBoundaryError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "provider boundary failed: {}", self.0)
            }
        }
        impl std::error::Error for ProviderBoundaryError {}

        let inner = ProviderError::api_error_caused_by(
            "provider request failed",
            std::io::Error::other("leaf service reason"),
        )
        .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        let outer = ProviderError::internal_caused_by(
            "outer provider operation failed",
            ProviderBoundaryError(inner),
        )
        .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));

        let rendered = outer.to_string();
        assert_eq!(
            rendered.matches("leaf service reason").count(),
            1,
            "{rendered}"
        );

        let wire_cause = outer.to_wire().cause.expect("wire cause");
        assert_eq!(
            wire_cause.matches("leaf service reason").count(),
            1,
            "{wire_cause}"
        );
    }

    #[test]
    fn provider_error_display_with_resource_id_and_cause() {
        let cause = std::io::Error::other("timeout");
        let id = ResourceId::with_identity("s3.Bucket", "my-bucket");
        let err = ProviderError::api_error_caused_by("Failed to read", cause)
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .for_resource(id);
        let display = format!("{}", err);
        assert!(
            display.contains("timeout"),
            "Display should include cause message, got: {}",
            display
        );
        assert!(
            display.contains("s3.Bucket"),
            "Display should include resource type, got: {}",
            display
        );
    }

    /// carina#3242: when the AWS-style structured fields (operation,
    /// status, code, request_id) are populated, `Display` renders the
    /// new multi-line, labeled shape. The header still uses the
    /// existing `[type.name] message` form for consistency with
    /// non-AWS errors.
    #[test]
    fn provider_error_display_multi_line_when_structured_fields_populated() {
        let id = ResourceId::with_identity("iam.Roles", "admin_access_roles");
        let err = ProviderError::api_error("Failed to list IAM roles")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .for_resource(id)
            .with_operation("iam.ListRoles")
            .with_status(403)
            .with_code("AccessDenied")
            .with_request_id("997aa923-2aa4-4d2b-8d16-44fd21c81368");

        let rendered = format!("{}", err);
        let lines: Vec<&str> = rendered.lines().collect();

        // Header line is the existing `[type.name] message` form.
        assert_eq!(
            lines.first().copied(),
            Some("[iam.Roles.admin_access_roles] Failed to list IAM roles"),
            "header line must keep the existing [type.name] message form, got: {}",
            rendered
        );

        // status and code go on the same line, as `<status> <code>`.
        assert!(
            lines.contains(&"  status: 403 AccessDenied"),
            "status and code must render on one line as `<status> <code>`, got: {}",
            rendered
        );

        // operation gets its own labeled line.
        assert!(
            lines.contains(&"  operation: iam.ListRoles"),
            "operation must render on its own labeled line, got: {}",
            rendered
        );

        // request_id is the last labeled line, no blank line before it.
        assert!(
            lines.contains(&"  request_id: 997aa923-2aa4-4d2b-8d16-44fd21c81368"),
            "request_id must render on its own labeled line, got: {}",
            rendered
        );

        // The raw `Debug`-style scaffolding (`ServiceError`, `SdkBody`,
        // `Headers`, `Extensions`) must not appear. Use a substring
        // that would only show up if we were `Debug`-printing an SDK
        // error.
        assert!(
            !rendered.contains("SdkBody"),
            "multi-line render must not leak SDK scaffolding, got: {}",
            rendered
        );
    }

    /// carina#3242: when only `status` is populated (no `code`), the
    /// status line still renders cleanly without a trailing space or
    /// `unknown` placeholder.
    #[test]
    fn provider_error_display_multi_line_status_without_code() {
        let err = ProviderError::api_error("Failed")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .with_operation("s3.HeadBucket")
            .with_status(500);
        let rendered = format!("{}", err);
        assert!(
            rendered.lines().any(|l| l == "  status: 500"),
            "status-only line must render without a trailing space or placeholder, got: {}",
            rendered
        );
    }

    /// carina#3242: when only `code` is populated (no `status`), the
    /// code line renders on its own.
    #[test]
    fn provider_error_display_multi_line_code_without_status() {
        let err = ProviderError::api_error("Failed")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .with_operation("s3.HeadBucket")
            .with_code("NoSuchBucket");
        let rendered = format!("{}", err);
        assert!(
            rendered.lines().any(|l| l == "  code: NoSuchBucket"),
            "code-only line must render on its own, got: {}",
            rendered
        );
    }

    /// carina#3242: with `operation` alone (no status/code/request_id),
    /// only that one labeled line follows the header — the status/code
    /// match arm collapses to its `(None, None)` branch and produces
    /// nothing.
    #[test]
    fn provider_error_display_multi_line_operation_only() {
        let err = ProviderError::api_error("Failed")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .with_operation("iam.ListRoles");
        let rendered = format!("{}", err);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "expected exactly 2 lines, got: {}",
            rendered
        );
        assert_eq!(lines[0], "Failed");
        assert_eq!(lines[1], "  operation: iam.ListRoles");
    }

    /// carina#3242: when structured fields are populated but no
    /// `resource_id` is attached, the header degenerates to the bare
    /// `message`, with the labeled lines unaffected. Locks the no-id
    /// branch of the header so future renderer changes can't silently
    /// drop the message text.
    #[test]
    fn provider_error_display_multi_line_without_resource_id() {
        let err = ProviderError::api_error("Failed to list IAM roles")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .with_operation("iam.ListRoles")
            .with_status(403)
            .with_code("AccessDenied");
        let rendered = format!("{}", err);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines.first().copied(),
            Some("Failed to list IAM roles"),
            "header must be bare message when resource_id is absent, got: {}",
            rendered
        );
        assert!(
            lines.contains(&"  status: 403 AccessDenied"),
            "structured lines must still render without resource_id, got: {}",
            rendered
        );
    }

    /// carina#3242: when both `cause` and structured cloud fields are
    /// populated, the renderer emits `cause:` as its OWN labeled line
    /// at the bottom (not joined to the header). Transport failures
    /// (DNS, TLS handshake, network timeouts) only carry their
    /// diagnostic in the terminal cause — there was no HTTP response, so
    /// `status`/`code`/`request_id` are `None` but `operation` may
    /// still be set. Without preserving cause, the operator loses
    /// visibility into the underlying error. The labeled-line shape
    /// keeps the new multi-line aesthetic while still surfacing the
    /// diagnostic.
    #[test]
    fn provider_error_display_multi_line_appends_cause_as_labeled_line() {
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::api_error_caused_by("Failed to list IAM roles", cause)
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .with_operation("iam.ListRoles");
        let rendered = format!("{}", err);
        let lines: Vec<&str> = rendered.lines().collect();

        // Header line stays clean.
        assert_eq!(
            lines.first().copied(),
            Some("Failed to list IAM roles"),
            "header line must not be extended by the cause, got: {}",
            rendered
        );

        // operation labeled line.
        assert!(
            lines.contains(&"  operation: iam.ListRoles"),
            "operation labeled line must appear, got: {}",
            rendered
        );

        // cause is appended as its own labeled line — NOT joined to
        // the header — so transport diagnostics stay visible.
        assert!(
            lines.contains(&"  cause: connection refused"),
            "cause must render as its own labeled line at the bottom, got: {}",
            rendered
        );
    }

    /// carina#3242: realistic provider-aws shape after migration —
    /// resource_id + all four structured fields + cause all populated
    /// at once. Locks in the full line ordering so a future change to
    /// `render_structured_cloud_fields` can't silently reshuffle the
    /// output: header → operation → status+code → request_id → cause.
    #[test]
    fn provider_error_display_multi_line_all_fields_including_cause() {
        let cause = std::io::Error::other("tls handshake failed");
        let id = ResourceId::with_identity("iam.Roles", "admin_access_roles");
        let err = ProviderError::api_error_caused_by("Failed to list IAM roles", cause)
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .for_resource(id)
            .with_operation("iam.ListRoles")
            .with_status(403)
            .with_code("AccessDenied")
            .with_request_id("997aa923-2aa4-4d2b-8d16-44fd21c81368");

        let rendered = format!("{}", err);
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(
            lines,
            vec![
                "[iam.Roles.admin_access_roles] Failed to list IAM roles",
                "  operation: iam.ListRoles",
                "  status: 403 AccessDenied",
                "  request_id: 997aa923-2aa4-4d2b-8d16-44fd21c81368",
                "  cause: tls handshake failed",
            ],
            "full structured render must lock the line ordering, got: {}",
            rendered
        );
    }

    /// carina#3242: a terminal cause carries all of its detail in Display,
    /// including detail held by a typed inner error.
    #[test]
    fn provider_error_display_multi_line_preserves_complete_terminal_cause() {
        #[derive(Debug)]
        struct ChainErr {
            msg: &'static str,
            detail: Option<Box<ChainErr>>,
        }
        impl std::fmt::Display for ChainErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.msg)?;
                if let Some(detail) = &self.detail {
                    write!(f, ": {detail}")?;
                }
                Ok(())
            }
        }
        impl std::error::Error for ChainErr {}

        let inner = ChainErr {
            msg: "dns lookup failed",
            detail: None,
        };
        let outer = ChainErr {
            msg: "transport error",
            detail: Some(Box::new(inner)),
        };
        let err = ProviderError::api_error_caused_by("Failed", outer)
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .with_operation("iam.ListRoles");
        let rendered = format!("{}", err);
        assert!(
            rendered.contains("\n  cause: transport error: dns lookup failed"),
            "cause line must preserve the complete terminal cause, got: {}",
            rendered
        );
    }

    /// carina#3242: when **none** of the structured fields are
    /// populated, `Display` uses the existing single-line cause form.
    #[test]
    fn provider_error_display_uses_single_line_cause_when_unstructured() {
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::api_error_caused_by("Failed to create resource", cause)
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        let rendered = format!("{}", err);
        // The existing render is a single line with `: <cause>` appended.
        assert_eq!(
            rendered, "Failed to create resource: connection refused",
            "fallback render must match the existing form exactly, got: {}",
            rendered
        );
    }

    #[test]
    fn provider_error_variant_constructors() {
        let inv = ProviderError::invalid_input("bad")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        {
            use crate::provider::HostError as ProviderError;
            assert!(matches!(inv, ProviderError::InvalidInput(_)));
        }
        assert_eq!(inv.to_string(), "bad");

        let api = ProviderError::api_error("rejected")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        {
            use crate::provider::HostError as ProviderError;
            assert!(matches!(api, ProviderError::ApiError(_)));
        }

        let nf = ProviderError::not_found("missing")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        {
            use crate::provider::HostError as ProviderError;
            assert!(matches!(nf, ProviderError::NotFound(_)));
        }

        let to = ProviderError::timeout("slow")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        {
            use crate::provider::HostError as ProviderError;
            assert!(matches!(to, ProviderError::Timeout(_)));
        }

        let intl = ProviderError::internal("bug")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]));
        {
            use crate::provider::HostError as ProviderError;
            assert!(matches!(intl, ProviderError::Internal(_)));
        }
    }

    #[test]
    fn build_update_patch_classifies_ops() {
        use crate::schema::CompiledResourceSchema as ResourceSchema;

        let id = ResourceId::with_identity("test", "example");
        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "a".to_string(),
            Value::Concrete(ConcreteValue::String("old".into())),
        );
        from_attrs.insert(
            "c".to_string(),
            Value::Concrete(ConcreteValue::String("removed".into())),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("test", "example");
        to.set_attr(
            "a".to_string(),
            Value::Concrete(ConcreteValue::String("new".into())),
        );
        to.set_attr(
            "b".to_string(),
            Value::Concrete(ConcreteValue::String("added".into())),
        );

        let to = resolved_for_test(to);
        let schema = ResourceSchema::new("test");
        let changed = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let patch = build_update_patch(&changed, &to, &from, &schema).expect("valid patch");
        assert_eq!(patch.ops.len(), 3);

        let by_key: HashMap<&str, &PatchOp> = patch.ops.iter().map(|op| (op.key(), op)).collect();
        let a = by_key["a"];
        assert!(matches!(
            a,
            PatchOp::Replace {
                value: Value::Concrete(ConcreteValue::String(value)),
                ..
            } if value == "new"
        ));
        let b = by_key["b"];
        assert!(matches!(
            b,
            PatchOp::Add {
                value: Value::Concrete(ConcreteValue::String(value)),
                ..
            } if value == "added"
        ));
        let c = by_key["c"];
        assert!(matches!(c, PatchOp::Remove { .. }));
        assert_eq!(c.sent_value(), None);
    }

    fn patch_test_value(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }

    fn nested_write_only_patch_fixture() -> (crate::schema::ValidatedResourceSchema, Value) {
        let schema = PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional("description", PatternAttributeType::string())
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "default_actions",
                    PatternAttributeType::list(PatternAttributeType::struct_(
                        "Action",
                        vec![
                            PatternStructField::optional(
                                "authenticate_oidc_config",
                                PatternAttributeType::struct_(
                                    "AuthenticateOidcConfig",
                                    vec![
                                        PatternStructField::optional(
                                            "issuer",
                                            PatternAttributeType::string(),
                                        )
                                        .build(),
                                        PatternStructField::optional(
                                            "client_secret",
                                            PatternAttributeType::string(),
                                        )
                                        .write_only()
                                        .build(),
                                    ],
                                ),
                            )
                            .build(),
                        ],
                    )),
                )
                .build(),
            );
        let value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "authenticate_oidc_config".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    (
                        "issuer".to_string(),
                        patch_test_value("https://issuer.example.com"),
                    ),
                    (
                        "client_secret".to_string(),
                        Value::Deferred(DeferredValue::Secret(
                            Box::new(patch_test_value("client-secret")).into(),
                        )),
                    ),
                ]))),
            )])),
        )]));
        let schema = schema.validate().expect("patch schema must validate");
        (schema, value)
    }

    fn nested_struct_write_only_patch_fixture() -> (crate::schema::ValidatedResourceSchema, Value) {
        let schema = PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional("description", PatternAttributeType::string())
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "default_actions",
                    PatternAttributeType::struct_(
                        "Action",
                        vec![
                            PatternStructField::optional(
                                "authenticate_oidc_config",
                                PatternAttributeType::struct_(
                                    "AuthenticateOidcConfig",
                                    vec![
                                        PatternStructField::optional(
                                            "issuer",
                                            PatternAttributeType::string(),
                                        )
                                        .build(),
                                        PatternStructField::optional(
                                            "client_secret",
                                            PatternAttributeType::string(),
                                        )
                                        .write_only()
                                        .build(),
                                    ],
                                ),
                            )
                            .build(),
                        ],
                    ),
                )
                .build(),
            );
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "authenticate_oidc_config".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "issuer".to_string(),
                    patch_test_value("https://issuer.example.com"),
                ),
                (
                    "client_secret".to_string(),
                    Value::Deferred(DeferredValue::Secret(
                        Box::new(patch_test_value("client-secret")).into(),
                    )),
                ),
            ]))),
        )])));
        let schema = schema.validate().expect("patch schema must validate");
        (schema, value)
    }

    fn ignored_path(parts: &[&str]) -> crate::resource::ParsedIgnorePath {
        crate::resource::ParsedIgnorePath::new(
            parts
                .iter()
                .map(|part| {
                    crate::resource::ParsedIgnorePathSegment::Attribute((*part).to_string())
                })
                .collect(),
        )
    }

    fn ready_with_ignored_path(
        mut resource: Resource,
        schema: &crate::schema::ValidatedResourceSchema,
        path: crate::resource::ParsedIgnorePath,
    ) -> ProviderReadyResource {
        resource.directives.ignore_changes = vec![path];
        let mut registry = crate::schema::SchemaRegistry::new();
        registry.insert("", schema.clone());
        let bindings = crate::binding_index::ResolvedBindings::default();
        let gate = crate::executor::ModuleConstraintGate::from_live_compositions(&[]);
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |type_checks| {
                let context = crate::executor::ProviderPreparationContext::new(
                    &bindings,
                    &gate,
                    &[],
                    &NoopNormalizer,
                    &[],
                    &registry,
                    type_checks,
                );
                futures::executor::block_on(crate::executor::prepare_provider_ready_resource(
                    resource, &context,
                ))
                .expect("ignore_changes path must resolve during provider preparation")
            },
        )
    }

    #[test]
    fn ignored_nested_write_only_leaf_prevents_parent_reassert() {
        let (schema, default_actions) = nested_struct_write_only_patch_fixture();
        let from = State::existing(
            ResourceId::with_identity("test", "example"),
            HashMap::from([
                ("description".to_string(), patch_test_value("before")),
                (
                    "default_actions".to_string(),
                    schema
                        .as_schema()
                        .strip_write_only_leaves("default_actions", &default_actions),
                ),
            ]),
        );
        let to = ready_with_ignored_path(
            Resource::new("test", "example")
                .with_attribute("description", patch_test_value("after"))
                .with_attribute("default_actions", default_actions),
            &schema,
            ignored_path(&[
                "default_actions",
                "authenticate_oidc_config",
                "client_secret",
            ]),
        );

        let patch =
            build_update_patch(&["description".to_string()], &to, &from, schema.as_schema())
                .expect("fully ignored write-only leaves need no reassertion");

        assert!(matches!(
            &patch.ops[..],
            [PatchOp::Replace { key, .. }] if key == "description"
        ));
    }

    #[test]
    fn plan_and_apply_use_the_same_update_patch_decision() {
        let (schema, default_actions) = nested_struct_write_only_patch_fixture();
        let from = State::existing(
            ResourceId::with_identity("test", "example"),
            HashMap::from([
                ("description".to_string(), patch_test_value("before")),
                (
                    "default_actions".to_string(),
                    schema
                        .as_schema()
                        .strip_write_only_leaves("default_actions", &default_actions),
                ),
            ]),
        );
        let to = ready_with_ignored_path(
            Resource::new("test", "example")
                .with_attribute("description", patch_test_value("after"))
                .with_attribute("default_actions", default_actions),
            &schema,
            ignored_path(&[
                "default_actions",
                "authenticate_oidc_config",
                "client_secret",
            ]),
        );
        let changed = ["description".to_string()];

        let planned = plan_update_patch(&changed, to.as_inner(), &from, schema.as_schema())
            .expect("planning must accept the patch");
        let applied = build_update_patch(&changed, &to, &from, schema.as_schema())
            .expect("apply must accept the same patch");

        assert_eq!(planned.ops, applied.ops);
    }

    #[test]
    fn ignored_readable_leaf_uses_observed_value_when_sibling_is_sent() {
        let schema = PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional(
                    "config",
                    PatternAttributeType::struct_(
                        "Config",
                        vec![
                            PatternStructField::optional("version", PatternAttributeType::string())
                                .build(),
                            PatternStructField::optional("memory", PatternAttributeType::int())
                                .build(),
                        ],
                    ),
                )
                .build(),
            )
            .validate()
            .expect("ignore_changes patch schema must validate");
        let struct_value = |version: &str, memory: i64| {
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                ("version".to_string(), patch_test_value(version)),
                (
                    "memory".to_string(),
                    Value::Concrete(ConcreteValue::Int(memory)),
                ),
            ])))
        };
        let from = State::existing(
            ResourceId::with_identity("test", "example"),
            HashMap::from([("config".to_string(), struct_value("external", 128))]),
        );
        let to = ready_with_ignored_path(
            Resource::new("test", "example")
                .with_attribute("config", struct_value("recorded", 256)),
            &schema,
            ignored_path(&["config", "version"]),
        );

        let patch = build_update_patch(&["config".to_string()], &to, &from, schema.as_schema())
            .expect("readable ignored leaf can be filled from the read");
        let PatchOp::Replace { value, .. } = &patch.ops[0] else {
            panic!("expected config replacement: {:?}", patch.ops);
        };
        let Value::Concrete(ConcreteValue::Map(config)) = value else {
            panic!("expected config map")
        };
        assert_eq!(config["version"], patch_test_value("external"));
        assert_eq!(config["memory"], Value::Concrete(ConcreteValue::Int(256)));
    }

    #[test]
    fn sending_parent_of_ignored_write_only_leaf_is_a_typed_error() {
        let schema = PatternResourceSchema::new("test")
            .attribute(
                PatternAttributeSchema::optional(
                    "config",
                    PatternAttributeType::struct_(
                        "Config",
                        vec![
                            PatternStructField::optional("secret", PatternAttributeType::string())
                                .write_only()
                                .build(),
                            PatternStructField::optional("memory", PatternAttributeType::int())
                                .build(),
                        ],
                    ),
                )
                .build(),
            )
            .validate()
            .expect("ignore_changes patch schema must validate");
        let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("secret".to_string(), patch_test_value("recorded")),
            (
                "memory".to_string(),
                Value::Concrete(ConcreteValue::Int(256)),
            ),
        ])));
        let observed = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "memory".to_string(),
            Value::Concrete(ConcreteValue::Int(128)),
        )])));
        let from = State::existing(
            ResourceId::with_identity("test", "example"),
            HashMap::from([("config".to_string(), observed)]),
        );
        let to = ready_with_ignored_path(
            Resource::new("test", "example").with_attribute("config", desired),
            &schema,
            ignored_path(&["config", "secret"]),
        );

        let error = build_update_patch(&["config".to_string()], &to, &from, schema.as_schema())
            .expect_err("the current ignored write-only value is unknowable");

        let UpdatePatchError::IgnoredWriteOnly(error) = error else {
            panic!("expected ignored write-only error")
        };
        assert_eq!(error.resource_id(), &to.id);
        assert_eq!(error.attribute(), "config");
        assert_eq!(error.path().to_string(), "config.secret");
        assert!(error.to_string().contains("current value cannot be known"));
    }

    #[test]
    fn nested_write_only_record_sent_skips_non_bearing_keys() {
        use crate::schema::{
            CompiledAttributeSchema as AttributeSchema, CompiledAttributeType as AttributeType,
            CompiledStructField as StructField,
        };

        let schema = ResourceSchema::new("test")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    "credentials",
                    AttributeType::struct_(
                        "Credentials",
                        vec![
                            StructField::optional("password", AttributeType::string())
                                .write_only()
                                .build(),
                        ],
                    ),
                )
                .build(),
            );
        let attributes = IndexMap::from([
            ("name".to_string(), patch_test_value("example")),
            (
                "credentials".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "password".to_string(),
                    patch_test_value("accepted-secret"),
                )]))),
            ),
        ]);
        let mut record = NestedWriteOnlyRecord::new();

        record
            .record_sent(&schema, ["name", "credentials"], &attributes, None)
            .expect("ordinary sent keys must be ignored");

        assert!(record.get("name").is_none());
        assert!(record.get("credentials").is_some());
    }

    #[test]
    fn build_update_patch_reasserts_whole_write_only_bearing_attribute() {
        let (schema, default_actions) = nested_write_only_patch_fixture();
        let id = ResourceId::with_identity("test", "example");
        let from = State::existing(
            id,
            HashMap::from([
                ("description".to_string(), patch_test_value("before")),
                ("default_actions".to_string(), default_actions.clone()),
            ]),
        );
        let to = resolved_for_test(
            Resource::new("test", "example")
                .with_attribute("description", patch_test_value("after"))
                .with_attribute("default_actions", default_actions.clone()),
        );
        let provider_ready_actions = to
            .as_resource()
            .attributes
            .get("default_actions")
            .expect("fixture includes default_actions")
            .clone();

        let patch =
            build_update_patch(&["description".to_string()], &to, &from, schema.as_schema())
                .expect("valid reassert patch");

        assert!(matches!(
            &patch.ops[..],
            [
                PatchOp::Replace { key: changed, .. },
                PatchOp::Reassert { key: reasserted, value },
            ] if changed == "description"
                && reasserted == "default_actions"
                && value == &provider_ready_actions
        ));
    }

    #[test]
    fn build_update_patch_reasserts_only_desired_updatable_write_only_keys() {
        use crate::schema::{
            CompiledAttributeSchema as AttributeSchema, CompiledAttributeType as AttributeType,
            CompiledResourceSchema as ResourceSchema,
        };

        let id = ResourceId::with_identity("test", "example");
        let from = State::existing(
            id,
            HashMap::from([
                ("description".to_string(), patch_test_value("before")),
                ("token_b".to_string(), patch_test_value("token-b")),
                ("token_a".to_string(), patch_test_value("token-a")),
                ("create_token".to_string(), patch_test_value("create")),
                ("omitted_token".to_string(), patch_test_value("omitted")),
            ]),
        );
        let to = resolved_for_test(
            Resource::new("test", "example")
                .with_attribute("description", patch_test_value("after"))
                .with_attribute("token_b", patch_test_value("token-b"))
                .with_attribute("token_a", patch_test_value("token-a"))
                .with_attribute("create_token", patch_test_value("create")),
        );
        let schema = ResourceSchema::new("test")
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("token_b", AttributeType::string())
                    .write_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("token_a", AttributeType::string())
                    .write_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("create_token", AttributeType::string())
                    .write_only()
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("omitted_token", AttributeType::string())
                    .write_only()
                    .build(),
            );

        let patch = build_update_patch(&["description".to_string()], &to, &from, &schema)
            .expect("valid write-only patch");

        assert!(matches!(
            &patch.ops[0],
            PatchOp::Replace { key, .. } if key == "description"
        ));
        assert!(matches!(
            &patch.ops[1],
            PatchOp::Reassert { key, value } if key == "token_a" && value == &patch_test_value("token-a")
        ));
        assert!(matches!(
            &patch.ops[2],
            PatchOp::Reassert { key, value } if key == "token_b" && value == &patch_test_value("token-b")
        ));
        assert_eq!(patch.ops.len(), 3);
    }

    #[test]
    fn build_update_patch_does_not_duplicate_changed_write_only_keys() {
        use crate::schema::{
            CompiledAttributeSchema as AttributeSchema, CompiledAttributeType as AttributeType,
            CompiledResourceSchema as ResourceSchema,
        };

        let id = ResourceId::with_identity("test", "example");
        let from = State::existing(
            id,
            HashMap::from([
                ("replaced".to_string(), patch_test_value("before")),
                ("removed".to_string(), patch_test_value("removed")),
            ]),
        );
        let to = resolved_for_test(
            Resource::new("test", "example")
                .with_attribute("replaced", patch_test_value("after"))
                .with_attribute("added", patch_test_value("added")),
        );
        let schema = ResourceSchema::new("test")
            .attribute(
                AttributeSchema::optional("replaced", AttributeType::string())
                    .write_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("added", AttributeType::string())
                    .write_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("removed", AttributeType::string())
                    .write_only()
                    .build(),
            );

        let patch = build_update_patch(
            &[
                "replaced".to_string(),
                "added".to_string(),
                "removed".to_string(),
            ],
            &to,
            &from,
            &schema,
        )
        .expect("valid changed write-only patch");

        assert!(matches!(
            &patch.ops[0],
            PatchOp::Replace { key, .. } if key == "replaced"
        ));
        assert!(matches!(
            &patch.ops[1],
            PatchOp::Add { key, .. } if key == "added"
        ));
        assert!(matches!(
            &patch.ops[2],
            PatchOp::Remove { key } if key == "removed"
        ));
        assert_eq!(patch.ops.len(), 3);
    }

    #[test]
    fn build_update_patch_never_creates_a_reassert_only_patch() {
        use crate::schema::{
            CompiledAttributeSchema as AttributeSchema, CompiledAttributeType as AttributeType,
            CompiledResourceSchema as ResourceSchema,
        };

        let id = ResourceId::with_identity("test", "example");
        let from = State::existing(
            id,
            HashMap::from([("token".to_string(), patch_test_value("same"))]),
        );
        let to = resolved_for_test(
            Resource::new("test", "example").with_attribute("token", patch_test_value("same")),
        );
        let schema = ResourceSchema::new("test").attribute(
            AttributeSchema::optional("token", AttributeType::string())
                .write_only()
                .build(),
        );

        assert!(
            build_update_patch(&[], &to, &from, &schema)
                .expect("empty patch is valid")
                .ops
                .is_empty()
        );
        assert!(
            build_update_patch(&["absent".to_string()], &to, &from, &schema)
                .expect("absent patch is valid")
                .ops
                .is_empty(),
            "a changed key absent from both sides must be dropped and cannot trigger reassertion"
        );

        let (schema, default_actions) = nested_write_only_patch_fixture();
        let from = State::existing(
            ResourceId::with_identity("test", "nested"),
            HashMap::from([("default_actions".to_string(), default_actions.clone())]),
        );
        let to = resolved_for_test(
            Resource::new("test", "nested").with_attribute("default_actions", default_actions),
        );
        assert!(
            build_update_patch(&[], &to, &from, schema.as_schema())
                .expect("empty nested patch is valid")
                .ops
                .is_empty(),
            "a write-only-bearing attribute cannot create a reassert-only patch"
        );
    }

    #[test]
    fn build_update_patch_generated_inputs_preserve_reassert_invariants() {
        use crate::schema::{
            CompiledAttributeSchema as AttributeSchema, CompiledAttributeType as AttributeType,
            CompiledResourceSchema as ResourceSchema,
        };

        const KEYS: [&str; 3] = ["a", "b", "c"];
        for desired_mask in 0_u8..8 {
            for from_mask in 0_u8..8 {
                for write_only_mask in 0_u8..8 {
                    for create_only_mask in 0_u8..8 {
                        for changed_mask in 0_u8..8 {
                            let id = ResourceId::with_identity("test", "generated");
                            let mut from_attributes = HashMap::new();
                            let mut desired = Resource::new("test", "generated");
                            let mut schema = ResourceSchema::new("test");
                            let mut changed = Vec::new();

                            for (index, key) in KEYS.iter().enumerate() {
                                let bit = 1 << index;
                                if from_mask & bit != 0 {
                                    from_attributes
                                        .insert((*key).to_string(), patch_test_value("from"));
                                }
                                if desired_mask & bit != 0 {
                                    desired.set_attr((*key).to_string(), patch_test_value("to"));
                                }
                                let builder =
                                    AttributeSchema::optional(*key, AttributeType::string());
                                let attribute =
                                    match (write_only_mask & bit != 0, create_only_mask & bit != 0)
                                    {
                                        (false, false) => builder.build(),
                                        (true, false) => builder.write_only().build(),
                                        (false, true) => builder.create_only().build(),
                                        (true, true) => builder.write_only().create_only().build(),
                                    };
                                schema = schema.attribute(attribute);
                                if changed_mask & bit != 0 {
                                    changed.push((*key).to_string());
                                }
                            }

                            let from = State::existing(id, from_attributes);
                            let desired = resolved_for_test(desired);
                            let patch = build_update_patch(&changed, &desired, &from, &schema)
                                .expect("generated fixture has no ignored paths");
                            let patch_keys: Vec<&str> =
                                patch.ops.iter().map(PatchOp::key).collect();
                            let unique: std::collections::HashSet<_> =
                                patch_keys.iter().copied().collect();
                            assert_eq!(unique.len(), patch_keys.len());
                            if changed.is_empty() {
                                assert!(patch.ops.is_empty());
                            }

                            for op in &patch.ops {
                                let key = op.key();
                                let index =
                                    KEYS.iter().position(|candidate| candidate == &key).unwrap();
                                let bit = 1 << index;
                                let is_changed = changed_mask & bit != 0;
                                let satisfies_reassert = desired_mask & bit != 0
                                    && write_only_mask & bit != 0
                                    && create_only_mask & bit == 0
                                    && !is_changed;
                                assert!(is_changed || satisfies_reassert);
                                if matches!(op, PatchOp::Reassert { .. }) {
                                    assert!(satisfies_reassert);
                                }
                            }

                            for (index, key) in KEYS.iter().enumerate() {
                                let bit = 1 << index;
                                let is_desired = desired_mask & bit != 0;
                                let is_from = from_mask & bit != 0;
                                let is_changed = changed_mask & bit != 0;
                                let operation = patch.ops.iter().find(|op| op.key() == *key);

                                if is_changed && (is_desired || is_from) {
                                    match (is_desired, is_from) {
                                        (true, false) => assert!(
                                            matches!(operation, Some(PatchOp::Add { .. })),
                                            "changed desired-only key {key} must be Add"
                                        ),
                                        (true, true) => assert!(
                                            matches!(operation, Some(PatchOp::Replace { .. })),
                                            "changed key {key} present on both sides must be Replace"
                                        ),
                                        (false, true) => assert!(
                                            matches!(operation, Some(PatchOp::Remove { .. })),
                                            "changed current-only key {key} must be Remove"
                                        ),
                                        (false, false) => unreachable!(),
                                    }
                                }

                                let must_reassert = !patch.ops.is_empty()
                                    && is_desired
                                    && write_only_mask & bit != 0
                                    && create_only_mask & bit == 0
                                    && !is_changed;
                                if must_reassert {
                                    assert!(
                                        matches!(operation, Some(PatchOp::Reassert { .. })),
                                        "eligible unchanged write-only key {key} must be Reassert"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn provider_router_dispatches_update_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), initialized(MockProvider));

        let id = ResourceId::with_provider_identity("mock", "test", "example", None);
        let from = State::existing(id.clone(), HashMap::new());
        let ready = resolved_for_test(Resource::with_provider("mock", "test", "example", None));
        let schema = crate::schema::CompiledResourceSchema::new("test");
        let request = UpdateRequest::checked(from, &[], &ready, &schema)
            .expect("fixture has no ignored write-only paths");
        let identifier =
            AppliedIdentifier::new("mock-id-123").expect("fixture identifier is non-empty");
        let prepared = crate::replacement::update_permit_for_test(identifier.clone(), request);
        let state = crate::executor::dispatch_update(&router, prepared)
            .await
            .unwrap()
            .into_state_for_writeback();
        assert!(state.exists);
    }

    #[tokio::test]
    async fn provider_router_dispatches_delete_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), initialized(MockProvider));

        let id = ResourceId::with_provider_identity("mock", "test", "example", None);
        let identifier =
            AppliedIdentifier::new("mock-id-123").expect("fixture identifier is non-empty");
        let result = crate::executor::dispatch_delete(
            &router,
            crate::executor::delete_permit_for_test(
                id.clone(),
                identifier.clone(),
                Default::default(),
            ),
            &crate::schema::SchemaRegistry::new(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn provider_router_returns_error_for_unknown_provider() {
        let router = ProviderRouter::new();
        let id = ResourceId::with_provider_identity("nonexistent", "test", "example", None);
        let identifier = fixture_applied_identifier("mock-id");
        let result = router.read(&id, &identifier, ReadRequest).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        {
            use crate::provider::HostError as ProviderError;
            assert!(matches!(err, ProviderError::Internal(_)));
        }
        assert!(err.to_string().contains("Unknown provider: nonexistent"));
    }

    /// A provider that records its identity in the returned state
    /// so a routing test can tell which instance handled a call.
    struct TaggedProvider {
        tag: &'static str,
    }
    impl Provider for TaggedProvider {
        fn name(&self) -> &str {
            "tagged"
        }
        fn read(
            &self,
            id: &ResourceId,
            identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            let mut attrs = HashMap::new();
            attrs.insert(
                "tag".to_string(),
                Value::Concrete(ConcreteValue::String(self.tag.to_string().into())),
            );
            let state = AppliedState::new(id.clone(), identifier.as_str(), attrs)
                .expect("typed read identifier is non-empty");
            Box::pin(async move { Ok(ReadOutcome::found(state)) })
        }
        fn read_data_source(
            &self,
            _resource: &ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }
        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            attempt: CreateAttempt,
        ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
            Box::pin(
                async move { Err(attempt.not_created(ProviderError::internal("not supported"))) },
            )
        }

        unexpected_resume_create!();
        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
            _attempt: crate::provider::UpdateAttempt,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }
        fn delete(
            &self,
            _attempt: crate::provider::DeleteAttempt,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn provider_router_dispatches_to_named_instance() {
        // Two instances of the same kind ("mock") routed by binding.
        let mut router = ProviderRouter::new();
        router.add_provider(
            "mock".to_string(),
            initialized(TaggedProvider { tag: "default" }),
        );
        let named_config: ProviderConfig<
            crate::resource::NoPlaceholder,
            crate::resource::RelativeBindingName,
        > = ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: Some("us".to_string()),
        };
        router.add_provider_config(&named_config, initialized(TaggedProvider { tag: "us" }));

        let default_id = ResourceId::with_provider_identity("mock", "test", "a", None);
        let identifier = fixture_applied_identifier("mock-id");
        let state = router
            .read(&default_id, &identifier, ReadRequest)
            .await
            .unwrap()
            .into_state();
        assert_eq!(
            state.attributes.get("tag"),
            Some(&(Value::Concrete(ConcreteValue::String("default".into()))).to_snapshot()),
            "resources without provider_instance must route to the kind's default instance"
        );

        let us_id = ResourceId::with_provider_identity("mock", "test", "b", Some("us".to_string()));
        let state = router
            .read(&us_id, &identifier, ReadRequest)
            .await
            .unwrap()
            .into_state();
        assert_eq!(
            state.attributes.get("tag"),
            Some(&(Value::Concrete(ConcreteValue::String("us".into()))).to_snapshot()),
            "resources tagged with binding=Some('us') must route to that named instance"
        );
    }

    fn reference_form_registry(owner: &str) -> (crate::schema::SchemaRegistry, AttributeType) {
        use crate::schema::{
            ReferenceForm, ReferenceForms, ReferenceTemplate, ValidatedProviderSchemaWitness,
        };

        let identity = TypeIdentity::new(Some(owner), ["lambda", "Function"], "Name");
        let attr_type =
            PatternAttributeType::refined_string(Some(identity.clone()), None, None, None);
        let schema = PatternResourceSchema::new("lambda.Permission")
            .attribute(PatternAttributeSchema::optional("function_name", attr_type.clone()).build())
            .validate()
            .expect("fixture schema must validate");
        let forms = ReferenceForms::from([(
            identity,
            ReferenceForm::Template {
                long: ReferenceTemplate::new(
                    "arn:{partition}:lambda:{region}:{account}:function:{name}",
                )
                .expect("fixture reference template must validate"),
            },
        )]);
        let witness = ValidatedProviderSchemaWitness::new(
            vec![schema],
            ValidatedConfigSchema::default(),
            forms,
            Vec::new(),
        )
        .expect("fixture provider schema must validate");
        let mut registry = crate::schema::SchemaRegistry::new();
        registry
            .insert_provider_witness(owner, &witness)
            .expect("fixture owner has one validated witness");
        let attr_type = witness.resources()[0]
            .effective_attribute("function_name")
            .unwrap()
            .field_type()
            .clone();
        (registry, attr_type)
    }

    fn reference_scope(region: &str) -> ReferenceScope {
        ReferenceScope::available("aws", region, "123456789012").expect("fixture scope is valid")
    }

    fn string_value(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }

    #[test]
    fn comparison_context_routes_by_provider_kind_and_named_instance() {
        let (registry, attr_type) = reference_form_registry("mock");
        let schema = registry
            .get(
                "mock",
                "lambda.Permission",
                crate::schema::SchemaKind::Resource,
            )
            .expect("fixture schema must be registered");
        let mut router = ProviderRouter::new();
        router.add_provider_instance(
            ProviderInstanceKey::new("mock", Some("east".to_string())),
            InitializedProvider::for_test(Box::new(MockProvider), reference_scope("us-east-1")),
        );
        router.add_provider_instance(
            ProviderInstanceKey::new("mock", Some("west".to_string())),
            InitializedProvider::for_test(Box::new(MockProvider), reference_scope("us-west-2")),
        );
        let id = ResourceId::with_provider_identity(
            "mock",
            "lambda.Permission",
            "invoke",
            Some("west".to_string()),
        );

        let context = router
            .comparison_context_for(&id, schema)
            .expect("the named provider instance must supply its scope")
            .observation_for_test();
        let name = string_value("processor");
        let west_arn = string_value("arn:aws:lambda:us-west-2:123456789012:function:processor");
        let east_arn = string_value("arn:aws:lambda:us-east-1:123456789012:function:processor");
        assert!(crate::differ::type_aware_equal(
            &name,
            &west_arn,
            Some(&attr_type),
            crate::schema::empty_defs_for_schema_walks(),
            &context,
        ));
        assert!(!crate::differ::type_aware_equal(
            &name,
            &east_arn,
            Some(&attr_type),
            crate::schema::empty_defs_for_schema_walks(),
            &context,
        ));
    }

    #[test]
    fn comparison_context_resolves_a_pending_id_by_instance_and_resource_type() {
        let (registry, _attr_type) = reference_form_registry("mock");
        let mut router = ProviderRouter::new();
        router.add_provider_instance(
            ProviderInstanceKey::new("mock", Some("west".to_string())),
            InitializedProvider::for_test(Box::new(MockProvider), reference_scope("us-west-2")),
        );
        let pending = ResourceId::pending_with_provider(
            "mock",
            "lambda.Permission",
            Some("west".to_string()),
        );

        let comparison = router
            .comparison_for(&registry, &pending)
            .expect("pending identity is irrelevant to comparison resolution");

        assert_eq!(comparison.owner_schema().owner(), "mock");
        assert_eq!(
            comparison.owner_schema().schema().as_schema().resource_type,
            "lambda.Permission"
        );
    }

    #[test]
    fn resolved_comparison_table_reuses_a_pending_ids_type_context_after_resolution() {
        let (registry, _attr_type) = reference_form_registry("mock");
        let mut router = ProviderRouter::new();
        router.add_provider_instance(
            ProviderInstanceKey::new("mock", Some("west".to_string())),
            InitializedProvider::for_test(Box::new(MockProvider), reference_scope("us-west-2")),
        );
        let pending = ResourceId::pending_with_provider(
            "mock",
            "lambda.Permission",
            Some("west".to_string()),
        );
        let resolved = ResourceId::with_provider_identity(
            "mock",
            "lambda.Permission",
            "invoke",
            Some("west".to_string()),
        );

        let table = router
            .resolve_comparisons(&registry, std::iter::once(&pending))
            .expect("a pending id must pre-resolve its comparison context");
        let comparison = table
            .comparison_for(&resolved)
            .expect("the resolved id must reuse the instance/type comparison");

        assert_eq!(comparison.owner_schema().owner(), "mock");
    }

    #[test]
    fn comparison_context_rejects_a_schema_owned_by_another_provider() {
        let (registry, _attr_type) = reference_form_registry("mock");
        let schema = registry
            .get(
                "mock",
                "lambda.Permission",
                crate::schema::SchemaKind::Resource,
            )
            .expect("fixture schema must be registered");
        let router = ProviderRouter::new();
        let id = ResourceId::with_provider_identity(
            "other",
            "lambda.Permission",
            "invoke",
            Some("west".to_string()),
        );

        let error = match router.comparison_context_for(&id, schema) {
            Ok(_) => panic!("a provider must not compare through another owner's form table"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("routes to provider `other` but its schema is owned by `mock`")
        );
    }

    #[test]
    fn comparison_resolution_reports_missing_schema_as_typed_error() {
        let mut router = ProviderRouter::new();
        router.add_provider(
            "mock".to_string(),
            InitializedProvider::for_test(Box::new(MockProvider), ReferenceScope::Unavailable),
        );
        let registry = crate::schema::SchemaRegistry::new();
        let id = ResourceId::with_provider_identity("mock", "lambda.Permission", "invoke", None);

        let error = router
            .comparison_for(&registry, &id)
            .expect_err("a comparison requires the owner's managed schema");
        assert!(matches!(
            error,
            ComparisonResolutionError::MissingResourceSchema { id: actual } if actual == id
        ));
    }

    #[test]
    fn resolved_comparison_table_reports_an_unresolved_id_without_panicking() {
        let router = ProviderRouter::new();
        let registry = crate::schema::SchemaRegistry::new();
        let table = router
            .resolve_comparisons(&registry, std::iter::empty())
            .expect("an empty apply-start comparison set is valid");
        let id = ResourceId::with_provider_identity("mock", "test.Widget", "late", None);

        let error = table
            .comparison_for(&id)
            .expect_err("a writeback-only id must return a typed pre-resolution error");

        assert!(matches!(
            error,
            ComparisonResolutionError::ComparisonNotResolvedAtApplyStart { id: actual }
                if actual == id
        ));
    }

    #[tokio::test]
    async fn provider_router_unknown_named_instance_errors_with_binding() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), initialized(MockProvider));

        let id =
            ResourceId::with_provider_identity("mock", "test", "x", Some("missing".to_string()));
        let identifier = fixture_applied_identifier("mock-id");
        let err = router
            .read(&id, &identifier, ReadRequest)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing") && msg.contains("mock"),
            "error must name both the binding and the kind, got: {msg}"
        );
    }

    #[tokio::test]
    async fn provider_factory_create_provider_propagates_error() {
        // Issue #2407: providers can fail to initialize on user input
        // (e.g. allowed_account_ids mismatch). The trait method must
        // return ProviderResult so the host can surface the error
        // verbatim instead of panicking with .expect(...).
        struct FailingFactory;

        impl ProviderFactory for FailingFactory {
            fn name(&self) -> &str {
                "failing"
            }
            fn display_name(&self) -> &str {
                "Failing provider"
            }
            fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
                Default::default()
            }
            fn validate_config(
                &self,
                _attrs: &CanonicalProviderConfigAttributes,
                _session: &crate::provider::ProviderValidationInputs,
            ) -> crate::provider::ProviderResult<()> {
                Ok(())
            }
            fn create_provider(
                &self,
                _config: &crate::provider::ProviderSession,
                _initialize_outcome: crate::provider::InitializeOutcomeMint,
            ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
                Box::pin(async {
                    Err(ProviderError::invalid_input(
                        "AWS account ID '019115212452' is not in the provider's \
                         allowed_account_ids [\"151116838382\"]. Refusing to operate \
                         against this account. Check the AWS credentials in your environment.",
                    ))
                })
            }
            fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
                crate::schema::ValidatedProviderSchemaWitness::new(
                    self.schemas(),
                    self.provider_config_attribute_types(),
                    Default::default(),
                    Vec::new(),
                )
                .unwrap()
            }

            fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
                Vec::new()
            }
        }

        let factory = FailingFactory;
        let resolved_configs =
            crate::parser::ResolvedProviderConfigs::from_test_configs(Vec::new()).unwrap();
        let resolved_config = resolved_configs.provider_or_implicit(factory.name());
        let config =
            crate::executor::prepare_provider_ready_config(&factory, resolved_config.as_ref())
                .unwrap();
        let result = ProviderBoundary::initialize(
            &factory,
            &crate::provider::prepare_provider_session::<
                crate::resource::ModuleSource,
                crate::resource::Directives,
            >(factory.name(), None, Some(&config), &[], &[]),
        )
        .await;
        let err = match result {
            Ok(_) => panic!("create_provider must surface the init error"),
            Err(e) => e,
        };
        // Inner message must be preserved verbatim; no implementation-detail wrapper.
        let msg = err.to_string();
        assert!(
            msg.contains("019115212452"),
            "actual account missing: {msg}"
        );
        assert!(
            msg.contains("allowed_account_ids"),
            "kind label missing: {msg}"
        );
        assert!(
            !msg.contains("WASM provider instance"),
            "must not leak WASM hosting detail: {msg}"
        );
        assert!(
            !msg.contains("panicked"),
            "must not surface panic framing: {msg}"
        );
    }

    /// carina#2191 Phase 4: every `ProviderFactory::create_provider`
    /// implementation must receive the `binding` argument and have the
    /// opportunity to vary its returned provider per binding. The
    /// `WasmProviderFactory` uses this as a cache key to keep one
    /// `SharedWasmInstance` per named instance; this in-memory test
    /// covers the trait-level contract.
    #[tokio::test]
    async fn provider_factory_create_provider_receives_binding() {
        use std::sync::Arc;
        use std::sync::Mutex as StdMutex;

        #[derive(Default)]
        struct BindingCapturingFactory {
            calls: Arc<StdMutex<Vec<Option<String>>>>,
        }

        impl ProviderFactory for BindingCapturingFactory {
            fn name(&self) -> &str {
                "capture"
            }
            fn display_name(&self) -> &str {
                "Capturing provider"
            }
            fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
                Default::default()
            }
            fn validate_config(
                &self,
                _attrs: &CanonicalProviderConfigAttributes,
                _session: &crate::provider::ProviderValidationInputs,
            ) -> crate::provider::ProviderResult<()> {
                Ok(())
            }
            fn create_provider(
                &self,
                _config: &crate::provider::ProviderSession,
                _initialize_outcome: crate::provider::InitializeOutcomeMint,
            ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(_config.binding().map(|s| s.to_string()));
                Box::pin(async {
                    Ok(InitializeOutcome::unavailable()
                        .with_provider(Box::new(MockProvider) as Box<dyn Provider>))
                })
            }
            fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
                crate::schema::ValidatedProviderSchemaWitness::new(
                    self.schemas(),
                    self.provider_config_attribute_types(),
                    Default::default(),
                    Vec::new(),
                )
                .unwrap()
            }

            fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
                Vec::new()
            }
        }

        let factory = BindingCapturingFactory::default();
        let resolved_configs =
            crate::parser::ResolvedProviderConfigs::from_test_configs(Vec::new()).unwrap();
        let resolved_config = resolved_configs.provider_or_implicit(factory.name());
        let config =
            crate::executor::prepare_provider_ready_config(&factory, resolved_config.as_ref())
                .unwrap();
        let _ = ProviderBoundary::initialize(
            &factory,
            &crate::provider::prepare_provider_session::<
                crate::resource::ModuleSource,
                crate::resource::Directives,
            >(factory.name(), None, Some(&config), &[], &[]),
        )
        .await
        .unwrap();
        let _ = ProviderBoundary::initialize(
            &factory,
            &crate::provider::prepare_provider_session::<
                crate::resource::ModuleSource,
                crate::resource::Directives,
            >(factory.name(), Some("us"), Some(&config), &[], &[]),
        )
        .await
        .unwrap();
        let _ = ProviderBoundary::initialize(
            &factory,
            &crate::provider::prepare_provider_session::<
                crate::resource::ModuleSource,
                crate::resource::Directives,
            >(factory.name(), Some("tokyo"), Some(&config), &[], &[]),
        )
        .await
        .unwrap();

        let calls = factory.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![None, Some("us".to_string()), Some("tokyo".to_string())],
            "factory must observe each binding distinctly so it can key per-instance state \
             (cache the WASM instance / hand back independent providers)"
        );
    }

    #[tokio::test]
    async fn provider_normalizer_separate_from_runtime() {
        // Verify that ProviderNormalizer can be implemented independently from Provider.
        // A provider implementing both traits should have its schema extension
        // methods callable without going through the Provider trait.
        struct SchemaOnlyProvider;

        impl ProviderNormalizer for SchemaOnlyProvider {
            fn normalize_desired<'a>(
                &'a self,
                resources: &'a mut [ProviderResource],
            ) -> BoxFuture<'a, ProviderResult<()>> {
                Box::pin(async move {
                    // Prefix all string attribute values with "normalized:"
                    for resource in resources.iter_mut() {
                        for value in resource.attributes.values_mut() {
                            if let crate::resource::Value::Concrete(
                                crate::resource::ConcreteValue::String(s),
                            ) = value
                            {
                                *s = format!("normalized:{}", s.expose_plaintext()).into();
                            }
                        }
                    }
                    Ok(())
                })
            }

            fn normalize_state<'a>(
                &'a self,
                _current_states: &'a mut StateTransformMap,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }

            fn hydrate_read_state<'a>(
                &'a self,
                states: &'a mut StateTransformMap,
                saved: &'a SavedAttrs,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                Box::pin(async move {
                    for (id, saved_attrs) in saved {
                        if let Some(state) = states.get_mut(id) {
                            for (key, value) in saved_attrs {
                                state
                                    .attributes
                                    .entry(key.clone())
                                    .or_insert_with(|| value.clone());
                            }
                        }
                    }
                    Ok(())
                })
            }
        }

        // Test normalize_desired
        let ext = SchemaOnlyProvider;
        let mut resources = vec![
            Resource::<crate::resource::ModuleSource>::new("test", "example")
                .with_attribute(
                    "key",
                    Value::Concrete(ConcreteValue::String("value".into())),
                )
                .into_provider_resource(),
        ];
        ext.normalize_desired(&mut resources).await.unwrap();
        assert_eq!(
            resources[0].get_attr("key"),
            Some(&Value::Concrete(ConcreteValue::String(
                "normalized:value".into()
            )))
        );

        // Test hydrate_read_state
        let id = ResourceId::with_identity("test", "example");
        let mut states = StateTransformMap::from_states(HashMap::from([(
            id.clone(),
            State::existing(id.clone(), HashMap::new()),
        )]));
        let saved = crate::test_support::raw_saved_attrs(HashMap::from([(
            id.clone(),
            HashMap::from([(
                "restored".to_string(),
                Value::Concrete(ConcreteValue::String("data".into())),
            )]),
        )]))
        .lift(&SchemaRegistry::new());
        ext.hydrate_read_state(&mut states, saved.as_provider_saved_attrs())
            .await
            .unwrap();
        assert_eq!(
            states.get(&id).unwrap().attributes.get("restored"),
            Some(&(Value::Concrete(ConcreteValue::String("data".into()))).to_snapshot())
        );
    }

    #[tokio::test]
    async fn provider_router_delegates_normalizer() {
        // Test that ProviderRouter delegates ProviderNormalizer methods to sub-providers
        struct NormalizingProvider;

        impl Provider for NormalizingProvider {
            fn name(&self) -> &str {
                "normalizing"
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
                resource: &ProviderReadyDataSource,
            ) -> BoxFuture<'_, ProviderResult<State>> {
                let id = resource.id.clone();
                Box::pin(async move { Ok(State::not_found(id)) })
            }

            fn create(
                &self,
                id: &ResourceId,
                _request: CreateRequest,
                _attempt: CreateAttempt,
            ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
                let id = id.clone();
                Box::pin(async move {
                    Ok(CreateOutcome::fixture_success(
                        AppliedState::new(id, "normalizing-id", HashMap::new())
                            .expect("fixture identifier is non-empty"),
                    ))
                })
            }

            unexpected_resume_create!();

            fn update(
                &self,
                id: &ResourceId,
                identifier: &AppliedIdentifier,
                _request: UpdateRequest,
                _attempt: crate::provider::UpdateAttempt,
            ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
                let id = id.clone();
                let identifier = identifier.to_string();
                Box::pin(async move {
                    Ok(UpdateOutcome::success(
                        AppliedState::new(id, identifier, HashMap::new())
                            .expect("provider input identifier is non-empty"),
                    ))
                })
            }

            fn delete(
                &self,
                _attempt: crate::provider::DeleteAttempt,
            ) -> BoxFuture<'_, ProviderResult<()>> {
                Box::pin(async { Ok(()) })
            }

            fn required_permissions(
                &self,
                _id: &ResourceId,
                _op: PlanOp,
            ) -> ProviderResult<Vec<String>> {
                Ok(Vec::new())
            }
        }

        // Separate schema ext struct for the router
        struct TestNormalizer;
        impl ProviderNormalizer for TestNormalizer {
            fn normalize_desired<'a>(
                &'a self,
                resources: &'a mut [ProviderResource],
            ) -> BoxFuture<'a, ProviderResult<()>> {
                Box::pin(async move {
                    for resource in resources.iter_mut() {
                        if resource.id.provider == "normalizing" {
                            assert!(matches!(
                                resource.get_attr("peer"),
                                Some(crate::resource::Value::Deferred(
                                    crate::resource::DeferredValue::ResourceRef { .. }
                                ))
                            ));
                            assert!(matches!(
                                resource.get_attr("pending"),
                                Some(crate::resource::Value::Deferred(
                                    crate::resource::DeferredValue::Unknown(_)
                                ))
                            ));
                            for value in resource.attributes.values_mut() {
                                if let crate::resource::Value::Concrete(
                                    crate::resource::ConcreteValue::String(s),
                                ) = value
                                {
                                    *s = format!("norm:{}", s.expose_plaintext()).into();
                                }
                            }
                        }
                    }
                    Ok(())
                })
            }

            fn normalize_state<'a>(
                &'a self,
                _current_states: &'a mut StateTransformMap,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }

            fn hydrate_read_state<'a>(
                &'a self,
                _current_states: &'a mut StateTransformMap,
                _saved_attrs: &'a SavedAttrs,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }
        }

        let mut router = ProviderRouter::new();
        router.add_provider("normalizing".to_string(), initialized(NormalizingProvider));
        router.add_normalizer("normalizing", Box::new(TestNormalizer));

        let peer_ref = Value::root_resource_ref("peer", "identifier", Vec::new());
        let pending = Value::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::UpstreamAllExports {
                binding: absolute_binding("upstream"),
            },
        ));
        let resource: Resource = Resource::with_provider("normalizing", "test", "example", None)
            .with_attribute("key", Value::Concrete(ConcreteValue::String("val".into())))
            .with_attribute("peer", peer_ref.clone())
            .with_attribute("pending", pending.clone());
        let mut resources = vec![resource.into_provider_resource()];
        router.normalize_desired(&mut resources).await.unwrap();
        assert_eq!(
            resources[0].get_attr("key"),
            Some(&Value::Concrete(ConcreteValue::String("norm:val".into())))
        );
        assert_eq!(resources[0].get_attr("peer"), Some(&peer_ref));
        assert!(matches!(
            resources[0].get_attr("pending"),
            Some(crate::resource::Value::Deferred(
                crate::resource::DeferredValue::Unknown(
                    crate::resource::ApplyTimeUnknown::UpstreamAllExports { .. }
                )
            ))
        ));
    }
}

#[cfg(test)]
mod provider_text_route_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Seen(Mutex<Vec<Vec<String>>>);
    #[derive(Clone)]
    struct Probe(Arc<Seen>);
    fn bindings<'a>(ids: impl Iterator<Item = &'a ResourceId>) -> Vec<String> {
        let mut values = ids
            .map(|id| id.provider_instance.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        values.sort();
        values
    }
    impl ProviderNormalizer for Probe {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [ProviderResource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            self.0
                .0
                .lock()
                .unwrap()
                .push(bindings(resources.iter().map(|r| &r.id)));
            let echo = resources
                .iter()
                .map(|r| match &r.attributes["password"] {
                    Value::Concrete(ConcreteValue::String(value)) => value.expose_plaintext(),
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>()
                .join(" ");
            Box::pin(async move { Err(ProviderError::api_error(echo)) })
        }
        fn normalize_state<'a>(
            &'a self,
            states: &'a mut StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            self.0.0.lock().unwrap().push(bindings(states.keys()));
            ready_noop()
        }
        fn hydrate_read_state<'a>(
            &'a self,
            states: &'a mut StateTransformMap,
            saved: &'a SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            self.0.0.lock().unwrap().push(bindings(states.keys()));
            self.0
                .0
                .lock()
                .unwrap()
                .push(bindings(saved.iter().map(|(id, _)| id)));
            ready_noop()
        }
    }
    struct Factory(Arc<Seen>);
    impl ProviderFactory for Factory {
        fn name(&self) -> &str {
            "route-probe"
        }
        fn display_name(&self) -> &str {
            "Route probe"
        }
        fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
            Default::default()
        }
        fn validate_config(
            &self,
            _: &CanonicalProviderConfigAttributes,
            _: &ProviderValidationInputs,
        ) -> ProviderResult<()> {
            Ok(())
        }
        fn schemas(&self) -> Vec<ValidatedResourceSchema> {
            Vec::new()
        }
        fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
            ValidatedProviderSchemaWitness::new(
                Vec::new(),
                ValidatedConfigSchema::default(),
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }
        fn create_provider(
            &self,
            _: &ProviderSession,
            _: InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
            unreachable!()
        }
        fn create_normalizer(
            &self,
            _: &ProviderSession,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
            let probe = Probe(self.0.clone());
            Box::pin(async move { Ok(Box::new(probe) as Box<dyn ProviderNormalizer>) })
        }
    }
    #[tokio::test]
    async fn review_round4_normalization_routes_by_binding_before_provider_ingress() {
        let mut desired = Vec::<Resource>::new();
        for (binding, password) in [("first", "first-private"), ("second", "second-private")] {
            let mut resource = Resource::with_provider("route-probe", "thing", binding, None)
                .with_attribute(
                    "password",
                    Value::Concrete(ConcreteValue::String(
                        crate::resource::ValueString::diagnostic_sensitive(password),
                    )),
                );
            resource.id.provider_instance = Some(binding.into());
            desired.push(resource);
        }
        let session = prepare_provider_session("route-probe", Some("first"), None, &desired, &[]);
        let seen = Arc::new(Seen::default());
        let normalizer = ProviderBoundary::initialize_normalizer(&Factory(seen.clone()), &session)
            .await
            .unwrap();
        let mut router = NormalizerRouter::new();
        router.add_initialized_normalizer(normalizer);
        let mut resources = desired
            .iter()
            .cloned()
            .map(Resource::into_provider_resource)
            .collect::<Vec<_>>();
        let error = router
            .normalize_desired(&mut resources)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains("second-private"),
            "another binding's input leaked: {error}"
        );
        assert!(error.contains("(secret)"));
        let mut states = StateTransformMap::from_states(
            desired
                .iter()
                .map(|r| {
                    (
                        r.id.clone(),
                        State::<crate::resource::NoPlaceholder>::existing(
                            r.id.clone(),
                            HashMap::new(),
                        )
                        .to_snapshot(),
                    )
                })
                .collect(),
        );
        let saved = SavedAttrs(
            desired
                .iter()
                .map(|r| (r.id.clone(), HashMap::new()))
                .collect(),
        );
        router.normalize_state(&mut states).await.unwrap();
        router
            .hydrate_read_state(&mut states, &saved)
            .await
            .unwrap();
        assert_eq!(*seen.0.lock().unwrap(), vec![vec!["first".to_owned()]; 4]);
        assert_eq!(
            states.len(),
            2,
            "routing must retain the other binding's state"
        );
    }
}

#[cfg(test)]
mod composition_partial_tests {
    use super::*;
    use crate::test_utils::provider_text::{BindProviderText, context, create_attempt_for_test};

    fn id() -> ResourceId {
        ResourceId::with_provider_identity("text-fixture", "resource", "fixture", None)
    }
    fn reason() -> PartialReadDiagnostic {
        PartialReadDiagnostic::new(
            ProviderDiagnostic::new(
                ProviderError::api_error_caused_by(
                    "read-back failed",
                    std::io::Error::other("partial-private"),
                )
                .with_retryability(Retryability::Transient),
            ),
            vec!["name".to_owned()].try_into().unwrap(),
        )
    }
    fn assert_native<S: TextState>(partial: &PartialReadDiagnostic<S>) {
        assert_eq!(partial.diagnostic().retryability(), Retryability::Transient);
        assert!(partial.diagnostic().source_is::<std::io::Error>());
    }
    #[test]
    fn native_create_partial_preserves_cause_and_policy_through_boundary_and_persistence() {
        let outcome = create_attempt_for_test(id())
            .issued(
                AppliedIdentifier::new("id").unwrap(),
                Sent::AllRequestAttributes,
            )
            .partial(HashMap::new(), reason());
        assert_native(outcome.diagnostic().unwrap());
        let outcome =
            BindProviderText::with_secrets_for_test(outcome, &context(&["partial-private"]));
        assert_native(outcome.diagnostic().unwrap());
        assert_eq!(
            outcome.diagnostic().unwrap().reason(),
            "read-back failed: (secret)"
        );
        let json = serde_json::to_string(&outcome.into_state_for_writeback()).unwrap();
        assert!(!json.contains("partial-private"));
        assert!(json.contains("read-back failed: (secret)"));
    }
    #[test]
    fn native_resume_partial_preserves_cause_and_policy_through_boundary_and_persistence() {
        let identifier = AppliedIdentifier::new("id").unwrap();
        let pending = create_attempt_for_test(id())
            .issued(identifier.clone(), Sent::AllRequestAttributes)
            .pending(PendingToken::new("read-back"), 0)
            .persist(Default::default(), Default::default());
        let outcome = ResumedCreate {
            pending,
            identifier,
        }
        .partial(HashMap::new(), reason());
        let ResumeOutcomeKind::PartialSuccess(partial) = outcome.kind() else {
            panic!("native partial resume")
        };
        assert_native(partial.diagnostic());
        let outcome =
            BindProviderText::with_secrets_for_test(outcome, &context(&["partial-private"]));
        let HostResumeOutcomeKind::PartialSuccess(partial) = outcome.into_kind() else {
            panic!("bound partial resume")
        };
        assert_native(partial.diagnostic());
        assert_eq!(partial.diagnostic().reason(), "read-back failed: (secret)");
        let (_, state, _, _) = partial.into_parts();
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("partial-private"));
        assert!(json.contains("read-back failed: (secret)"));
    }
    #[test]
    fn native_update_partial_preserves_cause_and_policy_through_boundary_and_persistence() {
        let outcome = UpdateOutcome::partial_success(
            AppliedState::new(id(), "id", HashMap::new()).unwrap(),
            reason().reason,
            vec!["name".to_owned()].try_into().unwrap(),
        );
        assert_native(outcome.diagnostic().unwrap());
        let outcome =
            BindProviderText::with_secrets_for_test(outcome, &context(&["partial-private"]));
        assert_native(outcome.diagnostic().unwrap());
        assert_eq!(
            outcome.diagnostic().unwrap().reason(),
            "read-back failed: (secret)"
        );
        let json = serde_json::to_string(&outcome.into_state_for_writeback()).unwrap();
        assert!(!json.contains("partial-private"));
        assert!(json.contains("read-back failed: (secret)"));
    }
}
