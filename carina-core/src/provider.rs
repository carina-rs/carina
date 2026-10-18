//! Provider - Trait abstracting resource operations
//!
//! A Provider defines operations for a specific infrastructure (AWS, GCP, etc.).
//! It is responsible for converting Effects into actual API calls.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

use carina_provider_protocol::types::{
    ProviderError as WireProviderError, ProviderErrorKind as WireProviderErrorKind,
};

use crate::effect::PlanOp;
use crate::parser::ProviderConfig;
use crate::resource::{
    CanonicalEnumValue, ConcreteValue, DeferredValue, Directives, PartialReadMarker,
    ResolvedResource, Resource, ResourceId, ResourceIdentity, State, Value, ValueString,
};
#[cfg(test)]
use crate::resource::{DataSource, RedactedSecretHash};
use crate::schema::{ResourceSchema, SchemaRegistry, Shape, TypeIdentity};
use crate::value::SerializationError;
use crate::wait::BindingPattern;
use crate::wait::predicate::AttrPath;

pub use crate::executor::provider_ready::{
    CanonicalProviderConfigAttributes, ProviderReady, ProviderReadyConfig, ProviderReadyDataSource,
    ProviderReadyResource,
};

/// Contextual metadata attached to every [`ProviderError`] variant.
///
/// Mirrors `error-detail` in `wit/types.wit`. The typed terminal `cause` stays
/// host-side (boxed `dyn Error`) and is flattened to a string when the
/// error crosses the WIT boundary.
///
/// Message and cause cannot be supplied together through a struct literal.
/// Cause-bearing errors must use one of the typed `*_caused_by` constructors
/// on [`ProviderError`].
///
/// ```compile_fail,E0451
/// use carina_core::provider::ErrorDetail;
///
/// let _ = ErrorDetail {
///     message: String::from("x"),
///     cause: Some(Box::new(std::io::Error::other("x"))),
///     ..ErrorDetail::default()
/// };
/// ```
#[derive(Debug, Default)]
pub struct ErrorDetail {
    /// Human-readable message.
    message: String,
    /// Resource the error pertains to, if known.
    pub resource_id: Option<Box<ResourceId>>,
    /// Underlying terminal cause. Flattened to a string when the error crosses
    /// the WIT boundary.
    cause: Option<Box<dyn std::error::Error + Send + Sync>>,
    /// Provider name (e.g. `"aws"`, `"awscc"`) attached when the error
    /// is raised at provider-init / `create_provider` boundary points
    /// where the name is known but no specific resource is in scope.
    /// Display ignores this field; CLI renderers can read it to label
    /// structured output.
    pub provider_name: Option<String>,
    /// Service-qualified cloud-API operation that failed, e.g.
    /// `"iam.ListRoles"`, `"s3.HeadBucket"`. Populating this field —
    /// or any of the sibling `status` / `code` / `request_id`
    /// fields — flips `Display` into the multi-line, labeled render
    /// shape introduced in carina#3242; see
    /// [`ErrorDetail::has_structured_cloud_fields`] for the exact gate.
    pub operation: Option<String>,
    /// HTTP status code from the cloud-API response, when the error
    /// came from an HTTP-based service call.
    pub status: Option<u16>,
    /// Application-level error code, e.g. `"AccessDenied"`,
    /// `"NoSuchBucket"`. Distinct from `status` — multiple codes can
    /// share a status (HTTP 403 covers `AccessDenied`,
    /// `InvalidAccessKeyId`, `SignatureDoesNotMatch`, etc.), so both
    /// belong in the rendered output.
    pub code: Option<String>,
    /// Correlation id from the cloud-API response (AWS
    /// `x-amzn-RequestId`, GCP `X-Goog-Request-Id`, Azure
    /// `x-ms-request-id`, …). Operators paste this into support
    /// tickets so the provider can look up server-side logs.
    pub request_id: Option<String>,
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
#[derive(Debug)]
pub enum ProviderError {
    /// User-supplied input is invalid (bad attribute value, schema
    /// violation, configuration mismatch). Not retriable.
    InvalidInput(Box<ErrorDetail>),
    /// The cloud API rejected the request (HTTP 4xx/5xx, server-side
    /// error). May be retriable depending on the cause.
    ApiError(Box<ErrorDetail>),
    /// Resource was not found at the cloud API. `read` returns this
    /// instead of an empty state when the underlying resource has been
    /// deleted out-of-band. `delete` returns this when the resource is
    /// already gone.
    NotFound(Box<ErrorDetail>),
    /// Operation timed out before completing. Retriable with backoff.
    Timeout(Box<ErrorDetail>),
    /// Provider-internal failure (panic, unexpected state, missing
    /// schema entry, etc.). Should be escalated as a bug rather than
    /// retried.
    Internal(Box<ErrorDetail>),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = self.detail();

        // Header line: always the `[type.name] message` form when a
        // resource id is attached, plain message otherwise. Shared
        // between the structured multi-line render and the single-line
        // cause render so non-AWS errors keep the same shape.
        if let Some(ref id) = detail.resource_id {
            write!(
                f,
                "[{}.{}] {}",
                id.resource_type,
                id.identity_display(),
                detail.context()
            )?;
        } else {
            write!(f, "{}", detail.context())?;
        }

        // carina#3242: when any of the structured cloud-API fields are
        // populated, render the multi-line labeled shape and skip the
        // single-line cause form (the structured fields carry the same
        // information without the SDK-internal scaffolding). When all
        // four are `None`, fall through to the single-line cause form so
        // non-cloud errors render
        // exactly as they did before.
        if detail.has_structured_cloud_fields() {
            return render_structured_cloud_fields(f, detail);
        }

        // In-repo typed causes are terminal diagnostics. External errors may
        // still carry actionable detail in their source chain, so expand that
        // chain at this single ProviderError rendering seam.
        if let Some(cause) = detail.cause() {
            write!(f, ": {}", ProviderCauseChain(cause))?;
        }
        Ok(())
    }
}

/// Complete rendering of one typed provider cause.
///
/// Errors defined in this workspace follow the terminal rule and therefore
/// have no source. External errors such as SDK transport/service errors can
/// keep detail in their source chain; render every such level exactly once.
struct ProviderCauseChain<'a>(&'a (dyn std::error::Error + Send + Sync + 'static));

impl std::fmt::Display for ProviderCauseChain<'_> {
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

/// Render the multi-line, labeled cloud-API error shape introduced in
/// carina#3242. The header line has already been written by the caller;
/// this function writes only the indented label lines that follow.
///
/// Field order is fixed: `operation`, then `status`/`code` (combined on
/// one line because they're semantically a pair), then `request_id`,
/// then `cause` if present. Optional fields are skipped entirely when
/// absent — no placeholder `(unknown)` lines.
///
/// `cause` is appended **even when the structured fields are present**
/// because transport-level failures (DNS, TLS handshake, network
/// timeout) carry their diagnostic only in the terminal cause — there was
/// no HTTP response, so `status` / `code` / `request_id` aren't set,
/// but `operation` may still be. Without appending `cause`, the
/// operator would see `[s3.Bucket.foo] Failed / operation: s3.HeadBucket`
/// with no hint that the underlying error was `connection refused`.
fn render_structured_cloud_fields(
    f: &mut std::fmt::Formatter<'_>,
    detail: &ErrorDetail,
) -> std::fmt::Result {
    if let Some(ref op) = detail.operation {
        write!(f, "\n  operation: {op}")?;
    }
    match (detail.status, detail.code.as_deref()) {
        (Some(s), Some(c)) => write!(f, "\n  status: {s} {c}")?,
        (Some(s), None) => write!(f, "\n  status: {s}")?,
        (None, Some(c)) => write!(f, "\n  code: {c}")?,
        (None, None) => {}
    }
    if let Some(ref id) = detail.request_id {
        write!(f, "\n  request_id: {id}")?;
    }
    if let Some(cause) = detail.cause() {
        write!(f, "\n  cause: {}", ProviderCauseChain(cause))?;
    }
    Ok(())
}

// `ProviderError` is a terminal diagnostic: Display renders its complete
// typed cause. Exposing that cause again through Error::source would
// make every ordinary chain walker repeat it. The typed cause remains
// available through `detail().cause()` and is flattened by `to_wire()`.
impl std::error::Error for ProviderError {}

impl From<crate::value::EnumCanonicalizationError> for ProviderError {
    fn from(error: crate::value::EnumCanonicalizationError) -> Self {
        Self::invalid_input_caused_by("Failed to canonicalize enum value", error)
    }
}

impl ProviderError {
    /// Borrow the inner [`ErrorDetail`] regardless of variant.
    pub fn detail(&self) -> &ErrorDetail {
        match self {
            ProviderError::InvalidInput(d)
            | ProviderError::ApiError(d)
            | ProviderError::NotFound(d)
            | ProviderError::Timeout(d)
            | ProviderError::Internal(d) => d,
        }
    }

    /// Mutably borrow the inner [`ErrorDetail`] regardless of variant.
    pub fn detail_mut(&mut self) -> &mut ErrorDetail {
        match self {
            ProviderError::InvalidInput(d)
            | ProviderError::ApiError(d)
            | ProviderError::NotFound(d)
            | ProviderError::Timeout(d)
            | ProviderError::Internal(d) => d,
        }
    }

    /// Variant name as a static string (used by serialization layers).
    pub fn variant_name(&self) -> &'static str {
        match self {
            ProviderError::InvalidInput(_) => "invalid_input",
            ProviderError::ApiError(_) => "api_error",
            ProviderError::NotFound(_) => "not_found",
            ProviderError::Timeout(_) => "timeout",
            ProviderError::Internal(_) => "internal",
        }
    }

    /// User-supplied input is invalid.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        ProviderError::InvalidInput(Box::new(ErrorDetail::new(message)))
    }

    /// The cloud API rejected the request.
    pub fn api_error(message: impl Into<String>) -> Self {
        ProviderError::ApiError(Box::new(ErrorDetail::new(message)))
    }

    /// Resource was not found at the cloud API.
    pub fn not_found(message: impl Into<String>) -> Self {
        ProviderError::NotFound(Box::new(ErrorDetail::new(message)))
    }

    /// Operation timed out.
    pub fn timeout(message: impl Into<String>) -> Self {
        ProviderError::Timeout(Box::new(ErrorDetail::new(message)))
    }

    /// Provider-internal failure / unexpected state.
    pub fn internal(message: impl Into<String>) -> Self {
        ProviderError::Internal(Box::new(ErrorDetail::new(message)))
    }

    /// User-supplied input is invalid because of a typed source error.
    ///
    /// `context` is deliberately static: dynamic data belongs in a typed
    /// cause, so its rendered text cannot also be copied into the message.
    pub fn invalid_input_caused_by(
        context: &'static str,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        ProviderError::InvalidInput(Box::new(ErrorDetail::caused_by(context, cause)))
    }

    /// The cloud API rejected the request because of a typed source error.
    pub fn api_error_caused_by(
        context: &'static str,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        ProviderError::ApiError(Box::new(ErrorDetail::caused_by(context, cause)))
    }

    /// A resource was not found because of a typed source error.
    pub fn not_found_caused_by(
        context: &'static str,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        ProviderError::NotFound(Box::new(ErrorDetail::caused_by(context, cause)))
    }

    /// An operation timed out because of a typed source error.
    pub fn timeout_caused_by(
        context: &'static str,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        ProviderError::Timeout(Box::new(ErrorDetail::caused_by(context, cause)))
    }

    /// A provider-internal operation failed because of a typed source error.
    pub fn internal_caused_by(
        context: &'static str,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        ProviderError::Internal(Box::new(ErrorDetail::caused_by(context, cause)))
    }

    /// Encode this error for the provider protocol/WIT boundary.
    ///
    /// The complete typed cause chain is flattened into a string because the
    /// wire format cannot carry `dyn Error`. The returned type is the exact
    /// protocol record consumed by guest SDK encoders.
    pub fn to_wire(&self) -> WireProviderError {
        let detail = self.detail();
        let kind = match self {
            ProviderError::InvalidInput(_) => WireProviderErrorKind::InvalidInput,
            ProviderError::ApiError(_) => WireProviderErrorKind::ApiError,
            ProviderError::NotFound(_) => WireProviderErrorKind::NotFound,
            ProviderError::Timeout(_) => WireProviderErrorKind::Timeout,
            ProviderError::Internal(_) => WireProviderErrorKind::Internal,
        };
        WireProviderError {
            kind,
            message: detail.context().to_string(),
            resource_id: detail.resource_id.as_deref().map(ResourceId::to_wire),
            cause: detail
                .cause()
                .map(|cause| ProviderCauseChain(cause).to_string()),
            provider_name: detail.provider_name.clone(),
            operation: detail.operation.clone(),
            status: detail.status,
            code: detail.code.clone(),
            request_id: detail.request_id.clone(),
        }
    }

    /// Decode untrusted provider data received through the protocol/WIT boundary.
    ///
    /// This is the only sanctioned entry point for a wire record containing
    /// both a runtime-built message and flattened cause string. It preserves
    /// that untrusted data for diagnostics; it does not provide the static
    /// context guarantees of the typed `*_caused_by` constructors.
    pub fn from_wire(error: WireProviderError) -> Self {
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
        let resource_id = resource_id.map(|id| Box::new(ResourceId::from_wire(id)));
        let detail = Box::new(ErrorDetail {
            message,
            resource_id,
            cause: cause.map(|cause| {
                Box::new(FlattenedWireCause(cause)) as Box<dyn std::error::Error + Send + Sync>
            }),
            provider_name,
            operation,
            status,
            code,
            request_id,
        });
        match kind {
            WireProviderErrorKind::InvalidInput => ProviderError::InvalidInput(detail),
            WireProviderErrorKind::ApiError => ProviderError::ApiError(detail),
            WireProviderErrorKind::NotFound => ProviderError::NotFound(detail),
            WireProviderErrorKind::Timeout => ProviderError::Timeout(detail),
            WireProviderErrorKind::Internal => ProviderError::Internal(detail),
        }
    }

    /// Attach a resource id to the inner detail.
    pub fn for_resource(mut self, id: ResourceId) -> Self {
        self.detail_mut().resource_id = Some(Box::new(id));
        self
    }

    /// Attach a provider name (e.g. `"aws"`, `"awscc"`) for boundary
    /// errors that don't carry a specific resource id (e.g.
    /// `create_provider` / provider init failures). Used by the CLI
    /// renderer to label structured account-guard output.
    pub fn for_provider(mut self, provider_name: impl Into<String>) -> Self {
        self.detail_mut().provider_name = Some(provider_name.into());
        self
    }

    /// Attach the service-qualified cloud-API operation that failed
    /// (e.g. `"iam.ListRoles"`, `"s3.HeadBucket"`). Populating this —
    /// or any of [`Self::with_status`] / [`Self::with_code`] /
    /// [`Self::with_request_id`] — flips `Display` into the multi-line
    /// labeled render shape introduced in carina#3242.
    pub fn with_operation(mut self, operation: impl Into<String>) -> Self {
        self.detail_mut().operation = Some(operation.into());
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
        self.detail_mut().code = Some(code.into());
        self
    }

    /// Attach the correlation id from the cloud-API response so the
    /// operator can paste it into support tickets.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.detail_mut().request_id = Some(request_id.into());
        self
    }
}

impl ErrorDetail {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    fn caused_by(
        context: &'static str,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: context.to_string(),
            cause: Some(Box::new(cause)),
            ..Self::default()
        }
    }

    /// Static context for a caused error, or the complete message for a
    /// message-only error.
    pub fn context(&self) -> &str {
        &self.message
    }

    /// Typed source error, when this detail was built by a cause-bearing path.
    pub fn cause(&self) -> Option<&(dyn std::error::Error + Send + Sync + 'static)> {
        self.cause.as_deref()
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

/// Synthetic typed source for a flattened cause received over WIT.
#[derive(Debug)]
struct FlattenedWireCause(String);

impl std::fmt::Display for FlattenedWireCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FlattenedWireCause {}

pub type ProviderResult<T> = Result<T, ProviderError>;

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
/// provider-side state; `patch` carries only the user's intended
/// changes. The patch is the sole source of truth for what the
/// provider should write — there is no separate `to: Resource`
/// because exposing the full desired resource invites providers to
/// touch fields the user never specified (the root cause of
/// `carina-rs/carina#2559`).
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
#[derive(Debug, Clone)]
pub struct UpdateRequest {
    /// Current provider-side state. May be used for read-modify-write
    /// paths or for resolving server-assigned identifiers; MUST NOT be
    /// used to derive additional fields to write back.
    from: State,
    /// Structured description of the user's intended change.
    patch: UpdatePatch,
}

impl UpdateRequest {
    pub(crate) fn checked(
        from: State,
        changed_attributes: &[String],
        resource: &ProviderReadyResource,
    ) -> Self {
        let patch = build_update_patch(changed_attributes, resource, &from);
        Self { from, patch }
    }

    /// Current provider-side state.
    pub fn from(&self) -> &State {
        &self.from
    }

    /// Patch derived from the checked desired resource.
    pub fn patch(&self) -> &UpdatePatch {
        &self.patch
    }
}

/// Per-operation request record for [`Provider::delete`].
///
/// Mirrors `delete-request` in `wit/types.wit`.
#[derive(Debug, Clone, Default)]
pub struct DeleteRequest {
    /// Carina-side directives for the resource.
    pub directives: Directives,
}

/// A structured description of the user's intended change to a resource.
///
/// Mirrors `update-patch` in `wit/types.wit`. Each [`PatchOp`]
/// corresponds to a key the user explicitly specified or removed in
/// the desired state. Fields the user has never specified do not
/// appear in the patch.
///
/// Providers MUST NOT modify any attribute that is not represented in
/// `ops`. Although patch values remain source-compatible for provider helper
/// code, an [`UpdateRequest`] cannot be constructed without the private
/// provider-readiness witness.
#[derive(Debug, Clone, Default)]
pub struct UpdatePatch {
    /// Operations derived from the desired resource.
    pub ops: Vec<PatchOp>,
}

/// A single operation inside an [`UpdatePatch`].
///
/// Mirrors `patch-op` in `wit/types.wit`.
#[derive(Debug, Clone)]
pub struct PatchOp {
    pub kind: PatchOpKind,
    /// Top-level attribute name. Nested-field patches are a future
    /// extension.
    pub key: String,
    /// `Some(_)` for `Add` and `Replace`; `None` for `Remove`.
    pub value: Option<Value>,
}

/// Kind of a single [`PatchOp`].
///
/// Mirrors `patch-op-kind` in `wit/types.wit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOpKind {
    /// User added a key that did not previously appear in the desired state.
    Add,
    /// User changed the value of an existing key in the desired state.
    Replace,
    /// User removed a key that previously appeared in the desired state.
    Remove,
}

/// Build an [`UpdatePatch`] from a list of changed attribute names plus
/// the desired (`to`) and current (`from`) views.
///
/// For each `key` in `changed_attributes`:
/// - present in `to` but missing in `from` → [`PatchOpKind::Add`]
/// - present in both                       → [`PatchOpKind::Replace`]
/// - missing in `to` but present in `from` → [`PatchOpKind::Remove`]
///
/// `Remove` ops carry `value: None`; others carry a clone of the
/// value from `to`.
///
/// ```compile_fail
/// use carina_core::provider::build_update_patch;
/// use carina_core::resource::{ResolvedResource, Resource, State};
///
/// let resource = ResolvedResource::new(Resource::new("test", "example"));
/// let state: State = unimplemented!();
/// let _patch = build_update_patch(&[], &resource, &state);
/// ```
pub fn build_update_patch(
    changed_attributes: &[String],
    to: &ProviderReadyResource,
    from: &State,
) -> UpdatePatch {
    let to = to.as_resource();
    let ops = changed_attributes
        .iter()
        .map(|key| {
            let in_to = to.attributes.contains_key(key);
            let in_from = from.attributes.contains_key(key);
            let kind = match (in_to, in_from) {
                (true, false) => PatchOpKind::Add,
                (true, true) => PatchOpKind::Replace,
                (false, true) => PatchOpKind::Remove,
                // Neither side has it. Treat as Replace with None value;
                // this should not happen with well-formed inputs but the
                // provider will see a no-op-ish entry rather than a panic.
                (false, false) => PatchOpKind::Remove,
            };
            let value = if matches!(kind, PatchOpKind::Remove) {
                None
            } else {
                to.attributes.get(key).cloned()
            };
            PatchOp {
                kind,
                key: key.clone(),
                value,
            }
        })
        .collect();
    UpdatePatch { ops }
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
pub struct SavedAttrs(HashMap<ResourceId, HashMap<String, Value>>);

impl SavedAttrs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &ResourceId) -> Option<&HashMap<String, Value>> {
        self.0.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ResourceId, &HashMap<String, Value>)> {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a SavedAttrs {
    type Item = (&'a ResourceId, &'a HashMap<String, Value>);
    type IntoIter = std::collections::hash_map::Iter<'a, ResourceId, HashMap<String, Value>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Attribute storage in the state-file JSON format.
///
/// Serde is the construction and serialization boundary because the state file
/// itself is JSON. The invariant is that values from this type enter the DSL
/// value layer only through [`Self::get`] or [`Self::to_values`], which decode
/// complete persisted secret markers as typed `RedactedSecret` values rather
/// than ordinary strings.
///
/// Raw JSON cannot be inserted after construction. Context-free callers use
/// [`Self::from_encoded`]. A state row that may contain raw pre-v12 markers is
/// decoded through [`EncodedStateRowAttributes`], which obtains the legacy
/// salt inputs from the same serialized row as the attributes.
///
/// ```compile_fail
/// use carina_core::provider::PersistedAttributes;
///
/// let mut attributes = PersistedAttributes::default();
/// attributes.insert_encoded("token".to_string(), serde_json::json!("plaintext"));
/// attributes.replace_encoded(Default::default());
/// ```
#[derive(Debug, Clone, Default)]
pub struct PersistedAttributes {
    encoded: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PersistedAttributesError {
    /// A context-free persisted attribute contained a raw pre-v12 marker.
    #[error(
        "persisted attribute {attribute_key:?} contains a raw pre-v12 secret marker; raw pre-v12 markers are accepted only from a state-file row"
    )]
    RawLegacyMarker { attribute_key: String },
    /// The row-derived salt cannot be represented by the self-contained
    /// legacy marker format.
    #[error(
        "persisted attribute {attribute_key:?} has a pre-v12 legacy salt of {actual} bytes; the maximum is {maximum} bytes"
    )]
    LegacySaltTooLong {
        attribute_key: String,
        actual: usize,
        maximum: usize,
    },
}

impl PersistedAttributesError {
    pub fn attribute_key(&self) -> &str {
        match self {
            Self::RawLegacyMarker { attribute_key }
            | Self::LegacySaltTooLong { attribute_key, .. } => attribute_key,
        }
    }
}

/// The identity and raw attributes decoded together from one state-file row.
///
/// Fields are private, so callers cannot pair a raw marker with a separately
/// supplied resource id. Serde is the only constructor; decoding immediately
/// upgrades raw pre-v12 markers using this row's own provider, type, and
/// identity.
///
/// ```compile_fail
/// use carina_core::provider::EncodedStateRowAttributes;
///
/// let _ = EncodedStateRowAttributes {
///     provider: "mock".to_string(),
///     resource_type: "test.resource".to_string(),
///     identity: "wrong-row".try_into().unwrap(),
///     attributes: Default::default(),
/// };
/// ```
#[derive(Debug, Deserialize)]
pub struct EncodedStateRowAttributes {
    provider: String,
    resource_type: String,
    #[serde(alias = "name")]
    identity: ResourceIdentity,
    attributes: HashMap<String, serde_json::Value>,
}

/// A decoded state row plus the private origin needed to decode its deposed
/// generations through the same raw-legacy upgrade boundary.
#[derive(Debug)]
pub struct DecodedStateRowAttributes {
    provider: String,
    resource_type: String,
    identity: ResourceIdentity,
    origin: StateRowLegacyOrigin,
    attributes: PersistedAttributes,
}

#[derive(Debug)]
struct StateRowLegacyOrigin {
    display_type: String,
    identity: String,
}

impl PartialEq for PersistedAttributes {
    fn eq(&self, other: &Self) -> bool {
        self.encoded == other.encoded
    }
}

impl Eq for PersistedAttributes {}

impl Serialize for PersistedAttributes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.encoded.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PersistedAttributes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = HashMap::<String, serde_json::Value>::deserialize(deserializer)?;
        Self::from_encoded(encoded).map_err(serde::de::Error::custom)
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

impl<'a> PreviousSecretHashAuthority<'a> {
    fn protects_key(self, key_is_desired: bool) -> bool {
        match self {
            Self::None => false,
            Self::AllPreviouslyHashedKeys(_) => true,
            Self::OnlyKeysAbsentFromDesired(_) => !key_is_desired,
        }
    }

    fn attributes(self) -> Option<&'a PersistedAttributes> {
        match self {
            Self::None => None,
            Self::AllPreviouslyHashedKeys(attributes)
            | Self::OnlyKeysAbsentFromDesired(attributes) => Some(attributes),
        }
    }
}

impl EncodedStateRowAttributes {
    pub fn decode(self) -> Result<DecodedStateRowAttributes, PersistedAttributesError> {
        let display_type = if self.provider.is_empty() {
            self.resource_type.clone()
        } else {
            format!("{}.{}", self.provider, self.resource_type)
        };
        let origin = StateRowLegacyOrigin {
            display_type,
            identity: self.identity.as_str().to_string(),
        };
        let attributes = PersistedAttributes::from_state_row_origin(self.attributes, &origin)?;
        Ok(DecodedStateRowAttributes {
            provider: self.provider,
            resource_type: self.resource_type,
            identity: self.identity,
            origin,
            attributes,
        })
    }
}

impl DecodedStateRowAttributes {
    /// Decode another attribute map stored in the same state row, such as a
    /// deposed generation, using the row's original legacy salt inputs.
    pub fn decode_related_attributes(
        &self,
        encoded: HashMap<String, serde_json::Value>,
    ) -> Result<PersistedAttributes, PersistedAttributesError> {
        PersistedAttributes::from_state_row_origin(encoded, &self.origin)
    }

    pub fn into_parts(self) -> (String, String, ResourceIdentity, PersistedAttributes) {
        (
            self.provider,
            self.resource_type,
            self.identity,
            self.attributes,
        )
    }
}

#[cfg(test)]
pub(crate) fn persisted_attributes_from_test_state_row(
    id: &ResourceId,
    attributes: HashMap<String, serde_json::Value>,
) -> Result<PersistedAttributes, PersistedAttributesError> {
    let encoded: EncodedStateRowAttributes = serde_json::from_value(serde_json::json!({
        "provider": id.provider,
        "resource_type": id.resource_type,
        "identity": id.identity_str().expect("test state row has a resolved identity"),
        "attributes": attributes,
    }))
    .expect("test state row uses the encoded state shape");
    let decoded = encoded.decode()?;
    let (_, _, _, attributes) = decoded.into_parts();
    Ok(attributes)
}

impl PersistedAttributes {
    /// Encode the typed placeholders captured in a saved-plan resource into
    /// their state-marker representation.
    pub fn from_saved_plan_attributes<'a>(
        attributes: impl IntoIterator<Item = (&'a String, &'a Value)>,
    ) -> Self {
        let encoded = attributes
            .into_iter()
            .filter_map(|(key, value)| {
                saved_plan_secret_hashes(value).map(|value| (key.clone(), value))
            })
            .collect();
        Self { encoded }
    }

    fn from_state_row_origin(
        mut encoded: HashMap<String, serde_json::Value>,
        origin: &StateRowLegacyOrigin,
    ) -> Result<Self, PersistedAttributesError> {
        for (key, value) in &mut encoded {
            upgrade_raw_legacy_markers_for_state_row(value, origin, key)?;
        }
        Ok(Self { encoded })
    }

    /// Construct persisted JSON at a context-free boundary such as a provider
    /// or saved plan. Raw pre-v12 legacy markers are rejected.
    pub fn from_encoded(
        encoded: HashMap<String, serde_json::Value>,
    ) -> Result<Self, PersistedAttributesError> {
        if let Some(attribute_key) = encoded
            .iter()
            .filter(|(_, value)| contains_raw_legacy_secret_marker_json(value))
            .map(|(key, _)| key)
            .min()
        {
            return Err(PersistedAttributesError::RawLegacyMarker {
                attribute_key: attribute_key.clone(),
            });
        }
        Ok(Self { encoded })
    }

    /// Decode one stored attribute with persisted secret-marker semantics.
    pub fn get(&self, key: &str) -> Option<Value> {
        let value = self.encoded.get(key)?;
        crate::value::persisted_json_to_dsl_value(value)
    }

    /// Decode every non-null stored attribute with persisted marker semantics.
    pub fn to_values(&self) -> HashMap<String, Value> {
        self.encoded
            .iter()
            .filter_map(|(key, value)| {
                crate::value::persisted_json_to_dsl_value(value).map(|value| (key.clone(), value))
            })
            .collect()
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
    pub fn encoded_value(&self, key: &str) -> Option<&serde_json::Value> {
        self.encoded.get(key)
    }

    /// Retain top-level persisted attributes selected by `keep`.
    pub fn retain_keys(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.encoded.retain(|key, _| keep(key));
    }

    /// Return the opaque hash for a top-level persisted secret attribute.
    pub fn redacted_secret_hash(&self, key: &str) -> Option<crate::resource::RedactedSecretHash> {
        match self.get(key)? {
            Value::Deferred(crate::resource::DeferredValue::RedactedSecret { hash }) => Some(hash),
            _ => None,
        }
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

    /// Encode provider/history values for state writeback, skipping values
    /// that cannot be represented and protecting all known secret shapes.
    pub fn encode_values_for_resource_and_schema(
        resource: &ResolvedResource,
        schema: Option<&ResourceSchema>,
        attributes: &HashMap<String, Value>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
    ) -> Self {
        let serialized = attributes
            .iter()
            .filter_map(|(key, value)| {
                persisted_state_value_to_json(value)
                    .ok()
                    .map(|value| (key.clone(), value))
            })
            .collect();
        Self::protect_serialized(
            resource,
            schema,
            serialized,
            previous_hash_authority,
            None,
            false,
        )
        .expect("lossy secret protection does not return serialization errors")
    }

    /// Apply desired/schema/previous-state secret protection to values already
    /// encoded in the state-file representation.
    pub fn protect_for_resource_and_schema(
        self,
        resource: &ResolvedResource,
        schema: Option<&ResourceSchema>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
    ) -> Self {
        let Self { encoded } = self;
        Self::protect_serialized(
            resource,
            schema,
            encoded,
            previous_hash_authority,
            None,
            false,
        )
        .expect("lossy secret protection does not return serialization errors")
    }

    /// Strict protection used after a provider read has already been encoded.
    /// `preferred` supplies same-path hashes committed by a saved plan.
    pub fn try_protect_for_resource_and_schema(
        self,
        resource: &ResolvedResource,
        schema: Option<&ResourceSchema>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
        preferred: Option<&PersistedAttributes>,
    ) -> Result<Self, crate::value::SerializationError> {
        let Self { encoded } = self;
        Self::protect_serialized(
            resource,
            schema,
            encoded,
            previous_hash_authority,
            preferred,
            true,
        )
    }

    /// Protect a provider read for an orphan, where no desired resource is
    /// available to supply secrets. Previously hashed values remain opaque and
    /// schema write-only plaintext is not persisted.
    pub fn protect_for_orphan_refresh(
        self,
        schema: Option<&ResourceSchema>,
        previous: &PersistedAttributes,
    ) -> Self {
        let mut serialized = self.encoded;
        let mut protected_secret_keys = std::collections::HashSet::new();
        protect_previously_hashed_secrets(
            |_| false,
            PreviousSecretHashAuthority::AllPreviouslyHashedKeys(previous),
            &mut serialized,
            &mut protected_secret_keys,
        );
        remove_unprotected_write_only_attributes(schema, &protected_secret_keys, &mut serialized);
        Self {
            encoded: serialized,
        }
    }

    fn protect_serialized(
        resource: &ResolvedResource,
        schema: Option<&ResourceSchema>,
        mut serialized: HashMap<String, serde_json::Value>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
        preferred: Option<&PersistedAttributes>,
        strict_desired: bool,
    ) -> Result<Self, crate::value::SerializationError> {
        let mut protected_secret_keys = std::collections::HashSet::new();

        for (key, desired_value) in &resource.attributes {
            if !crate::value::contains_unredacted_secret(desired_value) {
                continue;
            }
            let previous = preferred
                .and_then(|attributes| attributes.encoded().get(key))
                .or_else(|| {
                    previous_hash_authority
                        .attributes()
                        .and_then(|attributes| attributes.encoded().get(key))
                });
            let masked = if let Some(provider_json) = serialized.get(key).cloned() {
                crate::value::merge_secrets_into_provider_json(
                    desired_value,
                    &provider_json,
                    previous,
                )
            } else {
                crate::value::value_to_json_reusing_secret_hashes(desired_value, previous)
            };
            match masked {
                Ok(masked) => {
                    serialized.insert(key.clone(), masked);
                    protected_secret_keys.insert(key.clone());
                }
                Err(error) if strict_desired => return Err(error),
                Err(_) => {
                    serialized.remove(key);
                }
            }
        }

        protect_previously_hashed_secrets(
            |key| resource.attributes.contains_key(key),
            previous_hash_authority,
            &mut serialized,
            &mut protected_secret_keys,
        );
        remove_unprotected_write_only_attributes(schema, &protected_secret_keys, &mut serialized);

        Ok(Self {
            encoded: serialized,
        })
    }

    pub(crate) fn encoded(&self) -> &HashMap<String, serde_json::Value> {
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

fn contains_raw_legacy_secret_marker_json(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) => {
            crate::resource::RedactedSecretHash::is_raw_legacy_marker(value)
        }
        serde_json::Value::Array(values) => {
            values.iter().any(contains_raw_legacy_secret_marker_json)
        }
        serde_json::Value::Object(values) => {
            values.values().any(contains_raw_legacy_secret_marker_json)
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

fn upgrade_raw_legacy_markers_for_state_row(
    value: &mut serde_json::Value,
    origin: &StateRowLegacyOrigin,
    top_level_attribute_key: &str,
) -> Result<(), PersistedAttributesError> {
    match value {
        serde_json::Value::String(marker) => {
            match crate::resource::RedactedSecretHash::upgrade_raw_legacy_marker_for_state_row(
                marker,
                &origin.display_type,
                &origin.identity,
                top_level_attribute_key,
            ) {
                Ok(Some(hash)) => *marker = hash.persisted_marker(),
                Ok(None) => {}
                Err(error) => {
                    return Err(PersistedAttributesError::LegacySaltTooLong {
                        attribute_key: top_level_attribute_key.to_string(),
                        actual: error.actual,
                        maximum: error.maximum,
                    });
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                upgrade_raw_legacy_markers_for_state_row(value, origin, top_level_attribute_key)?;
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                upgrade_raw_legacy_markers_for_state_row(value, origin, top_level_attribute_key)?;
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
    Ok(())
}

/// Extract only redacted-secret leaves from a saved-plan value.
///
/// Maps omit unrelated siblings. Lists retain their indices with `null`
/// placeholders so a preferred hash still addresses the same nested path.
fn saved_plan_secret_hashes(value: &Value) -> Option<serde_json::Value> {
    match value {
        Value::Deferred(DeferredValue::RedactedSecret { hash }) => Some(serde_json::Value::String(
            crate::value::persisted_secret_marker(hash),
        )),
        Value::Concrete(ConcreteValue::Map(values)) => {
            let values: serde_json::Map<_, _> = values
                .iter()
                .filter_map(|(key, value)| {
                    saved_plan_secret_hashes(value).map(|value| (key.clone(), value))
                })
                .collect();
            (!values.is_empty()).then_some(serde_json::Value::Object(values))
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            let mut found = false;
            let values = values
                .iter()
                .map(|value| {
                    saved_plan_secret_hashes(value).map_or(serde_json::Value::Null, |value| {
                        found = true;
                        value
                    })
                })
                .collect();
            found.then_some(serde_json::Value::Array(values))
        }
        _ => None,
    }
}

fn persisted_state_value_to_json(
    value: &Value,
) -> Result<serde_json::Value, crate::value::SerializationError> {
    match value {
        Value::Deferred(DeferredValue::RedactedSecret { hash }) => Ok(serde_json::Value::String(
            crate::value::persisted_secret_marker(hash),
        )),
        Value::Concrete(ConcreteValue::List(values)) => values
            .iter()
            .map(persisted_state_value_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        Value::Concrete(ConcreteValue::Map(values)) => values
            .iter()
            .map(|(key, value)| {
                persisted_state_value_to_json(value).map(|value| (key.clone(), value))
            })
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(serde_json::Value::Object),
        _ => crate::value::value_to_json(value),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretHashShape {
    None,
    TopLevel,
    Nested,
}

fn secret_hash_shape(value: &serde_json::Value) -> SecretHashShape {
    match value {
        serde_json::Value::String(stored) if crate::value::is_persisted_secret(stored) => {
            SecretHashShape::TopLevel
        }
        serde_json::Value::Array(items) if items.iter().any(contains_secret_hash_json) => {
            SecretHashShape::Nested
        }
        serde_json::Value::Object(map) if map.values().any(contains_secret_hash_json) => {
            SecretHashShape::Nested
        }
        _ => SecretHashShape::None,
    }
}

fn contains_secret_hash_json(value: &serde_json::Value) -> bool {
    secret_hash_shape(value) != SecretHashShape::None
}

fn protect_previously_hashed_secrets(
    is_desired_key: impl Fn(&str) -> bool,
    authority: PreviousSecretHashAuthority<'_>,
    serialized: &mut HashMap<String, serde_json::Value>,
    protected_secret_keys: &mut std::collections::HashSet<String>,
) {
    let Some(existing_attributes) = authority.attributes() else {
        return;
    };
    for (key, previous_value) in existing_attributes.encoded() {
        if protected_secret_keys.contains(key) || !authority.protects_key(is_desired_key(key)) {
            continue;
        }
        let shape = secret_hash_shape(previous_value);
        if shape == SecretHashShape::None {
            continue;
        }
        let masked = match (shape, serialized.get(key)) {
            (SecretHashShape::TopLevel, Some(provider_json))
                if secret_hash_shape(provider_json) == SecretHashShape::TopLevel =>
            {
                provider_json.clone()
            }
            (SecretHashShape::TopLevel, Some(provider_json)) => {
                hash_provider_json_as_secret(provider_json, Some(previous_value))
                    .unwrap_or_else(|_| previous_value.clone())
            }
            (SecretHashShape::TopLevel, None) => previous_value.clone(),
            (SecretHashShape::Nested, provider_json) => {
                merge_previous_secret_hashes_into_provider_json(previous_value, provider_json)
            }
            (SecretHashShape::None, _) => unreachable!("None shape is filtered above"),
        };
        serialized.insert(key.clone(), masked);
        protected_secret_keys.insert(key.clone());
    }
}

fn remove_unprotected_write_only_attributes(
    schema: Option<&ResourceSchema>,
    protected_secret_keys: &std::collections::HashSet<String>,
    serialized: &mut HashMap<String, serde_json::Value>,
) {
    if let Some(schema) = schema {
        for (key, attribute) in &schema.attributes {
            if attribute.write_only && !protected_secret_keys.contains(key) {
                serialized.remove(key);
            }
        }
    }
}

fn merge_previous_secret_hashes_into_provider_json(
    previous_value: &serde_json::Value,
    provider_value: Option<&serde_json::Value>,
) -> serde_json::Value {
    match previous_value {
        serde_json::Value::String(stored) if crate::value::is_persisted_secret(stored) => {
            let Some(provider_value) = provider_value else {
                return previous_value.clone();
            };
            if secret_hash_shape(provider_value) == SecretHashShape::TopLevel {
                return provider_value.clone();
            }
            hash_provider_json_as_secret(provider_value, Some(previous_value))
                .unwrap_or_else(|_| previous_value.clone())
        }
        serde_json::Value::Object(previous_object) => {
            let Some(serde_json::Value::Object(provider_object)) = provider_value else {
                return previous_value.clone();
            };
            let mut merged = provider_object.clone();
            for (key, previous_child) in previous_object {
                if contains_secret_hash_json(previous_child) {
                    merged.insert(
                        key.clone(),
                        merge_previous_secret_hashes_into_provider_json(
                            previous_child,
                            provider_object.get(key),
                        ),
                    );
                }
            }
            serde_json::Value::Object(merged)
        }
        serde_json::Value::Array(previous_items) => {
            let Some(serde_json::Value::Array(provider_items)) = provider_value else {
                return previous_value.clone();
            };
            if !previous_secret_hash_array_alignment_is_trusted(previous_items, provider_items) {
                return hash_provider_json_as_secret(
                    &serde_json::Value::Array(provider_items.clone()),
                    Some(previous_value),
                )
                .unwrap_or_else(|_| previous_value.clone());
            }
            let mut merged = provider_items.clone();
            for (index, previous_child) in previous_items.iter().enumerate() {
                if contains_secret_hash_json(previous_child) {
                    merged[index] = merge_previous_secret_hashes_into_provider_json(
                        previous_child,
                        provider_items.get(index),
                    );
                }
            }
            serde_json::Value::Array(merged)
        }
        _ => provider_value
            .cloned()
            .unwrap_or_else(|| previous_value.clone()),
    }
}

fn previous_secret_hash_array_alignment_is_trusted(
    previous_items: &[serde_json::Value],
    provider_items: &[serde_json::Value],
) -> bool {
    previous_items.len() == provider_items.len()
        && previous_items
            .iter()
            .zip(provider_items)
            .filter(|(previous, _)| contains_secret_hash_json(previous))
            .all(|(previous, provider)| {
                let (
                    serde_json::Value::Object(previous_object),
                    serde_json::Value::Object(provider_object),
                ) = (previous, provider)
                else {
                    return false;
                };
                let anchors: Vec<_> = previous_object
                    .iter()
                    .filter(|(_, value)| !contains_secret_hash_json(value))
                    .collect();
                !anchors.is_empty()
                    && anchors
                        .into_iter()
                        .all(|(key, value)| provider_object.get(key) == Some(value))
            })
}

pub(crate) fn hash_provider_json_as_secret(
    provider_json: &serde_json::Value,
    previous: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    if provider_json.is_null() {
        return Err("cannot hash null provider secret".to_string());
    }
    crate::value::persist_secret_json(provider_json, previous).map_err(|error| error.to_string())
}

/// Saved attributes decoded from persisted state but not yet reconciled
/// with the current provider schemas for enum or structural value shapes.
///
/// This type cannot be passed to planning or refresh hydration. Consume it
/// with [`RawSavedAttrs::lift`] to obtain [`LiftedSavedAttrs`].
/// Arbitrary desired values cannot assert persisted provenance:
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::provider::RawSavedAttrs;
/// use carina_core::resource::{ResourceId, Value};
///
/// let desired: HashMap<ResourceId, HashMap<String, Value>> = HashMap::new();
/// let _saved = RawSavedAttrs::from_persisted(desired);
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawSavedAttrs(SavedAttrs);

impl RawSavedAttrs {
    /// Collect opaque attribute maps obtained from persisted state rows.
    pub fn from_persisted(saved_attrs: HashMap<ResourceId, PersistedAttributes>) -> Self {
        Self(SavedAttrs(
            saved_attrs
                .into_iter()
                .map(|(id, attributes)| {
                    let values = attributes.to_values();
                    (id, values)
                })
                .collect(),
        ))
    }

    /// Construct saved attributes directly for tests that exercise invalid
    /// or otherwise non-JSON-representable provider-boundary values.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn from_decoded_values_for_test(
        saved_attrs: HashMap<ResourceId, HashMap<String, Value>>,
    ) -> Self {
        Self(SavedAttrs(saved_attrs))
    }

    /// Lift enum leaves and structurally canonicalize every persisted entry
    /// using the schema selected by its resource ID.
    ///
    /// carina#3740: both rewrites live at this single raw-to-lifted seam so a
    /// saved map cannot reach hydration or the differ with only one applied.
    pub fn lift(mut self, registry: &SchemaRegistry) -> LiftedSavedAttrs {
        crate::utils::lift_saved_state_enum_leaves(&mut self.0.0, registry);
        LiftedSavedAttrs(self.0)
    }

    /// Read a raw resource entry without changing its typestate.
    pub fn get(&self, id: &ResourceId) -> Option<&HashMap<String, Value>> {
        self.0.get(id)
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
pub struct LiftedSavedAttrs(SavedAttrs);

impl LiftedSavedAttrs {
    /// Borrow the plugin-ABI representation at a provider hydration boundary.
    pub fn as_provider_saved_attrs(&self) -> &SavedAttrs {
        &self.0
    }

    /// Read saved attributes for one resource.
    pub fn get(&self, id: &ResourceId) -> Option<&HashMap<String, Value>> {
        self.0.get(id)
    }

    /// Iterate resource IDs without exposing mutable map access.
    pub fn resource_ids(&self) -> impl Iterator<Item = &ResourceId> {
        self.0.0.keys()
    }

    /// Transfer a resource's saved attributes to a new ID.
    pub fn remap_resource_id(&mut self, from: &ResourceId, to: ResourceId) {
        if let Some(attrs) = self.0.0.remove(from) {
            self.0.0.insert(to, attrs);
        }
    }
}

/// Result of a provider create operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CreateOutcome {
    /// Create completed and the returned state is complete.
    Success { state: State },
    /// Create completed but the provider could not fully observe state.
    PartialSuccess {
        state: State,
        diagnostic: PartialReadDiagnostic,
    },
}

/// Result of a provider update operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UpdateOutcome {
    /// Update completed and the returned state is complete.
    Success { state: State },
    /// Update completed but the provider could not fully observe state.
    PartialSuccess {
        state: State,
        diagnostic: PartialReadDiagnostic,
    },
}

/// Diagnostic details for a partial post-mutation read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartialReadDiagnostic {
    reason: String,
    missing_attributes: Vec<String>,
}

impl CreateOutcome {
    pub fn partial_success(state: State, reason: String, missing_attributes: Vec<String>) -> Self {
        match PartialReadDiagnostic::new(reason, missing_attributes) {
            Some(diagnostic) => CreateOutcome::PartialSuccess { state, diagnostic },
            None => CreateOutcome::Success { state },
        }
    }

    /// Consume the outcome and produce a State ready for writeback.
    pub fn into_state_for_writeback(self) -> State {
        match self {
            CreateOutcome::Success { state } => state,
            CreateOutcome::PartialSuccess { state, diagnostic } => {
                diagnostic.into_state_for_writeback(state)
            }
        }
    }

    pub fn diagnostic(&self) -> Option<&PartialReadDiagnostic> {
        match self {
            CreateOutcome::Success { .. } => None,
            CreateOutcome::PartialSuccess { diagnostic, .. } => Some(diagnostic),
        }
    }
}

impl UpdateOutcome {
    pub fn partial_success(state: State, reason: String, missing_attributes: Vec<String>) -> Self {
        match PartialReadDiagnostic::new(reason, missing_attributes) {
            Some(diagnostic) => UpdateOutcome::PartialSuccess { state, diagnostic },
            None => UpdateOutcome::Success { state },
        }
    }

    /// Consume the outcome and produce a State ready for writeback.
    pub fn into_state_for_writeback(self) -> State {
        match self {
            UpdateOutcome::Success { state } => state,
            UpdateOutcome::PartialSuccess { state, diagnostic } => {
                diagnostic.into_state_for_writeback(state)
            }
        }
    }

    pub fn diagnostic(&self) -> Option<&PartialReadDiagnostic> {
        match self {
            UpdateOutcome::Success { .. } => None,
            UpdateOutcome::PartialSuccess { diagnostic, .. } => Some(diagnostic),
        }
    }
}

impl PartialReadDiagnostic {
    pub fn new(reason: String, missing_attributes: Vec<String>) -> Option<Self> {
        if missing_attributes.is_empty() {
            None
        } else {
            Some(Self {
                reason,
                missing_attributes,
            })
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn missing_attributes(&self) -> &[String] {
        &self.missing_attributes
    }

    /// Merge another partial-read diagnostic into this one.
    ///
    /// The receiver's reason is kept because it represents the most recent
    /// operation that wrote the state. Missing attributes are unioned so the
    /// next plan can surface every unobserved attribute as Unknown.
    pub fn merge_in(&mut self, other: PartialReadDiagnostic) {
        for attr in other.missing_attributes {
            if !self.missing_attributes.contains(&attr) {
                self.missing_attributes.push(attr);
            }
        }
    }

    /// Merge two optional diagnostics.
    ///
    /// When both are present, `a` keeps its reason because it represents the
    /// more recent operation, while missing attributes are unioned with `b`.
    /// Use this wherever two operations on the same resource may each report
    /// partial; `.or()` silently drops the loser's missing attributes.
    pub fn merge_options(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(mut a), Some(b)) => {
                a.merge_in(b);
                Some(a)
            }
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    pub fn into_state_for_writeback(self, mut state: State) -> State {
        state.partial_read = Some(PartialReadMarker {
            detail: self.reason,
            missing_attributes: self.missing_attributes.into_iter().collect(),
        });
        state
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
pub trait Provider: Send + Sync {
    /// Name of this Provider (e.g., "aws")
    fn name(&self) -> &str;

    /// Read the current provider-side state of an existing resource.
    ///
    /// `identifier` is the cloud-side identifier (e.g. `vpc-0abc...`),
    /// or `None` when no prior identifier exists for this resource —
    /// typically because the saved state has no entry for it yet (a
    /// fresh component or a newly added resource). When `identifier`
    /// is `None` the provider MUST return [`State::not_found`]
    /// without contacting any external API. Encoding presence in the
    /// type lets the compiler enforce that contract on every
    /// implementation, replacing the earlier `&str` shape that used
    /// `""` as a sentinel and produced carina-rs/carina#2594.
    ///
    /// `request` carries no operationally meaningful fields today; it
    /// exists so future fields (e.g. a freshness hint or an attribute
    /// projection) can be added without breaking the signature.
    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>>;

    /// Read a data source resource.
    ///
    /// Unlike [`Provider::read`], this receives the full checked data source so
    /// the provider can see the user-supplied input attributes (e.g. the
    /// `identity_store_id` + `user_name` that `aws.identitystore.user`
    /// needs to resolve itself via the AWS SDK).
    ///
    /// Each provider must implement this explicitly. For zero-input data
    /// sources (e.g. `aws.sts.caller_identity`), the implementation can
    /// simply delegate to a state-only read against `resource.id`.
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
    ) -> BoxFuture<'_, ProviderResult<State>>;

    /// Create the resource described by `request.resource()` and return
    /// the resulting state (with `identifier` set to the cloud-side
    /// internal ID, e.g. `vpc-xxx`).
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<CreateOutcome>>;

    /// Update an existing resource by applying `request.patch()`.
    ///
    /// Each [`PatchOp`] corresponds to a key the user explicitly
    /// specified or removed in the desired state. Fields the user has
    /// never specified do not appear in the patch.
    ///
    /// **Providers MUST NOT modify any attribute that is not
    /// represented in `request.patch().ops`.** The patch is the sole
    /// source of truth for the update payload.
    ///
    /// `request.from()` is the current provider-side state and may be
    /// used for read-modify-write paths or for resolving
    /// server-assigned identifiers; it MUST NOT be used to derive
    /// additional fields to write back.
    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>>;

    /// Delete an existing resource. `identifier` is the cloud-side
    /// internal ID (e.g. `vpc-xxx`).
    ///
    /// `request.directives` carries the resource's Carina-side directives
    /// (force-delete, create-before-destroy, prevent-destroy).
    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>>;

    /// Permissions this provider needs to perform `op` on `id`.
    /// Empty vec means the provider declares no permissions for this resource/op pair.
    fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>>;

    /// Binding-name patterns for resources that can satisfy a wait on `target_id.attr_path`.
    /// Empty vec means the provider declares no satisfier hint for this target attribute.
    fn satisfier_hint(
        &self,
        _target_id: &ResourceId,
        _attr_path: &AttrPath,
    ) -> ProviderResult<Vec<BindingPattern>> {
        Ok(Vec::new())
    }
}

/// Convenience for a `ProviderNormalizer` method that does nothing.
///
/// Returns an immediately-ready future. A `BoxFuture`-returning trait
/// method cannot have an empty `{}` default body, so every "I don't
/// normalize this" implementation returns this explicitly — keeping the
/// no-op a deliberate, visible choice rather than a silent default
/// (the hazard that caused carina-rs/carina-provider-awscc#192).
pub fn ready_noop<'a>() -> BoxFuture<'a, ProviderResult<()>> {
    Box::pin(async { Ok(()) })
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
pub trait ProviderNormalizer: Send + Sync {
    /// Normalize desired resource state before diffing.
    ///
    /// For example, resolves bare enum identifiers like `advanced` or
    /// `Tier.advanced` into fully-qualified DSL format like
    /// `awscc.ec2_ipam.Tier.advanced` based on schema definitions.
    /// Providers without enum types return [`ready_noop`].
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [Resource],
    ) -> BoxFuture<'a, ProviderResult<()>>;

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
        current_states: &'a mut HashMap<ResourceId, State>,
    ) -> BoxFuture<'a, ProviderResult<()>>;

    /// Hydrate read state with saved attributes that APIs don't return.
    ///
    /// Some APIs (e.g., CloudControl) don't return certain properties in read
    /// responses (create-only properties, or normal properties like `description`
    /// on some resources). This method carries them forward from previously
    /// saved attribute values.
    /// Providers that don't hydrate return [`ready_noop`].
    fn hydrate_read_state<'a>(
        &'a self,
        current_states: &'a mut HashMap<ResourceId, State>,
        saved_attrs: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>>;
}

/// A no-op normalizer for providers that don't need plan-time normalization.
#[derive(Debug, Clone, Copy)]
pub struct NoopNormalizer;
impl ProviderNormalizer for NoopNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        _resources: &'a mut [Resource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        ready_noop()
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
        _saved_attrs: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        ready_noop()
    }
}

/// The routing identity shared by provider dispatch and host-owned default-tag
/// normalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderRouteKey {
    provider: String,
    provider_instance: Option<String>,
}

impl ProviderRouteKey {
    fn new(provider: impl Into<String>, provider_instance: Option<String>) -> Self {
        Self {
            provider: provider.into(),
            provider_instance,
        }
    }

    fn matches(&self, id: &ResourceId) -> bool {
        self.provider == id.provider && self.provider_instance == id.provider_instance
    }
}

impl From<&ProviderConfig> for ProviderRouteKey {
    fn from(config: &ProviderConfig) -> Self {
        Self::new(config.name.clone(), config.binding.clone())
    }
}

impl From<&ResourceId> for ProviderRouteKey {
    fn from(id: &ResourceId) -> Self {
        Self::new(id.provider.clone(), id.provider_instance.clone())
    }
}

/// Check a default-tag value before host-owned merging. Unresolved values are
/// skippable; non-finite floats at any depth and redacted secrets are fatal.
fn assert_default_tag_value_encodable(value: &Value) -> Result<(), SerializationError> {
    match value {
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                assert_default_tag_value_encodable(item)?;
            }
            Ok(())
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            for value in entries.values() {
                assert_default_tag_value_encodable(value)?;
            }
            Ok(())
        }
        // The former seal path replaced a whole secret with an opaque token
        // before encoding, so its plaintext was not inspected by the boundary.
        Value::Deferred(DeferredValue::Secret(_)) => Ok(()),
        Value::Concrete(ConcreteValue::Float(value)) if !value.is_finite() => {
            Err(SerializationError::NonFiniteFloat {
                value: *value,
                context: crate::value::SerializationContext::WasmBoundary,
            })
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(value)) => {
            Err(SerializationError::UnresolvedEnumIdentifier {
                value: value.to_string(),
                context: crate::value::SerializationContext::WasmBoundary,
            })
        }
        Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Concrete(ConcreteValue::Int(_))
        | Value::Concrete(ConcreteValue::Float(_))
        | Value::Concrete(ConcreteValue::Bool(_))
        | Value::Concrete(ConcreteValue::Duration(_))
        | Value::Concrete(ConcreteValue::StringList(_))
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
        | Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::Interpolation(_))
        | Value::Deferred(DeferredValue::FunctionCall { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => {
            crate::resource::assert_value_fully_resolved(value)
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
            self.key, self.provider_name, self.source
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
pub fn merge_default_tags(
    config: &ProviderConfig,
    resources: &mut [Resource],
    registry: &SchemaRegistry,
) -> ProviderResult<()> {
    if config.default_tags.is_empty() {
        return Ok(());
    }

    // Validate every tag before touching resources so a fatal encoding error
    // cannot leave a partially merged desired state behind.
    let mut default_tags = IndexMap::new();
    for (key, value) in &config.default_tags {
        match assert_default_tag_value_encodable(value) {
            Ok(()) => {
                default_tags.insert(key.clone(), value.clone());
            }
            Err(
                error @ (SerializationError::UnknownNotAllowed { .. }
                | SerializationError::UnresolvedResourceRef { .. }
                | SerializationError::UnresolvedInterpolation { .. }
                | SerializationError::UnresolvedFunctionCall { .. }
                | SerializationError::SecretNotAllowed { .. }
                | SerializationError::SavedPlanFieldSecretNotAllowed { .. }),
            ) => {
                log::error!("Skipping default_tag '{key}' with unresolvable value: {error}");
            }
            Err(
                source @ (SerializationError::NonFiniteFloat { .. }
                | SerializationError::UnresolvedEnumIdentifier { .. }
                | SerializationError::RedactedSecretNotAllowed { .. }),
            ) => {
                return Err(ProviderError::invalid_input_caused_by(
                    "failed to encode default tag",
                    DefaultTagEncodingError {
                        key: key.clone(),
                        provider_name: config.name.clone(),
                        source,
                    },
                )
                .for_provider(config.name.clone()));
            }
        }
    }
    if default_tags.is_empty() {
        return Ok(());
    }

    let route = ProviderRouteKey::from(config);
    for resource in resources.iter_mut() {
        if !route.matches(&resource.id) {
            continue;
        }

        // Only a map(string) attribute can receive provider default tags.
        // Some providers use the same attribute name for list/struct shapes;
        // presence alone is therefore not enough to opt a resource in.
        let has_tags = registry.get_for(resource).is_some_and(|schema| {
            let Some(tags) = schema.attributes.get("tags") else {
                return false;
            };
            let Shape::Map { key, value } = schema.shape_of(&tags.attr_type) else {
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
    providers: HashMap<ProviderRouteKey, Box<dyn Provider>>,
    normalizers: Vec<(String, Box<dyn ProviderNormalizer>)>,
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
            normalizers: Vec::new(),
        }
    }

    /// Register the kind's default instance (resources with
    /// `provider_instance = None` route here).
    pub fn add_provider(&mut self, kind: String, provider: Box<dyn Provider>) {
        self.providers
            .insert(ProviderRouteKey::new(kind, None), provider);
    }

    /// Register a provider under the same route derived from its config by
    /// host-owned normalization.
    pub fn add_provider_config(&mut self, config: &ProviderConfig, provider: Box<dyn Provider>) {
        self.providers
            .insert(ProviderRouteKey::from(config), provider);
    }

    pub fn add_normalizer(
        &mut self,
        provider_name: impl Into<String>,
        ext: Box<dyn ProviderNormalizer>,
    ) {
        self.normalizers.push((provider_name.into(), ext));
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    fn get_provider_or_error(&self, id: &ResourceId) -> ProviderResult<&dyn Provider> {
        let key = ProviderRouteKey::from(id);
        self.providers.get(&key).map(|p| p.as_ref()).ok_or_else(|| {
            ProviderError::internal(match &id.provider_instance {
                Some(binding) => format!(
                    "Unknown provider instance: {} (kind={})",
                    binding, id.provider
                ),
                None => format!("Unknown provider: {}", id.provider),
            })
        })
    }
}

impl Provider for ProviderRouter {
    fn name(&self) -> &str {
        "router"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        match self.get_provider_or_error(id) {
            Ok(provider) => provider.read(id, identifier, request),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        match self.get_provider_or_error(&resource.id) {
            Ok(provider) => provider.read_data_source(resource),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<CreateOutcome>> {
        match self.get_provider_or_error(id) {
            Ok(provider) => provider.create(id, request),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        match self.get_provider_or_error(id) {
            Ok(provider) => provider.update(id, identifier, request),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        match self.get_provider_or_error(id) {
            Ok(provider) => provider.delete(id, identifier, request),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
        self.get_provider_or_error(id)?
            .required_permissions(id, op)
            .map_err(|error| error.for_provider(id.provider.clone()))
    }

    fn satisfier_hint(
        &self,
        target_id: &ResourceId,
        attr_path: &AttrPath,
    ) -> ProviderResult<Vec<BindingPattern>> {
        self.get_provider_or_error(target_id)?
            .satisfier_hint(target_id, attr_path)
            .map_err(|error| error.for_provider(target_id.provider.clone()))
    }
}

impl ProviderNormalizer for ProviderRouter {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [Resource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            // Sequential, never concurrent: normalizers are not
            // commutative, and `resources` is re-borrowed per iteration
            // across the `.await`.
            for (provider_name, ext) in &self.normalizers {
                ext.normalize_desired(resources)
                    .await
                    .map_err(|error| error.for_provider(provider_name.clone()))?;
            }
            Ok(())
        })
    }

    fn normalize_state<'a>(
        &'a self,
        current_states: &'a mut HashMap<ResourceId, State>,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            for (provider_name, ext) in &self.normalizers {
                ext.normalize_state(current_states)
                    .await
                    .map_err(|error| error.for_provider(provider_name.clone()))?;
            }
            Ok(())
        })
    }

    fn hydrate_read_state<'a>(
        &'a self,
        current_states: &'a mut HashMap<ResourceId, State>,
        saved_attrs: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            for (provider_name, ext) in &self.normalizers {
                ext.hydrate_read_state(current_states, saved_attrs)
                    .await
                    .map_err(|error| error.for_provider(provider_name.clone()))?;
            }
            Ok(())
        })
    }
}

/// Provider-facing text whose DSL spelling has already been resolved by the
/// host.
///
/// The inner string is private and there is no public constructor, so an
/// implementation of [`ProviderFactory`] can inspect API spelling but a caller
/// outside `carina-core` cannot manufacture this capability from raw DSL text.
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
/// logic (region validation, region extraction, provider instantiation, schemas).
/// The CLI uses factories instead of hardcoded provider name matching.
pub trait ProviderFactory: Send + Sync {
    /// Provider name (e.g., "aws", "awscc")
    fn name(&self) -> &str;

    /// Display name for user-facing messages (e.g., "AWS provider", "AWS Cloud Control provider")
    fn display_name(&self) -> &str;

    /// Return the types of the provider block's configuration attributes
    /// (e.g., `region`).
    ///
    /// These are used by the host to validate provider config attributes
    /// against their declared types *before* calling `validate_config`.
    /// Keeping format validation (namespace structure, enum membership) on
    /// the host side means fixes to generic validation logic in
    /// `carina-core` take effect without rebuilding provider binaries.
    fn provider_config_attribute_types(&self) -> HashMap<String, crate::schema::AttributeType>;

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
    fn validate_config(&self, attributes: &CanonicalProviderConfigAttributes)
    -> Result<(), String>;

    /// Validate a value against a provider-defined custom type.
    /// Returns `Ok(())` if the value is valid or the type is unknown to this provider.
    /// Returns `Err(message)` if the value is invalid for the given type.
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
    ///         .validate_custom_type(&TypeIdentity::bare("Mode"), "raw.dsl.text")
    ///         .unwrap();
    /// }
    /// ```
    fn validate_custom_type(
        &self,
        _identity: &TypeIdentity,
        _value: &CanonicalCustomTypeValue,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Extract region from config in SDK format (e.g., "ap-northeast-1").
    /// Returns a default region if none is configured.
    fn extract_region(&self, attributes: &IndexMap<String, Value>) -> String;

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
    /// verbatim — it is the user-facing error text. The sealed
    /// [`ProviderReadyConfig`] proves that resolution, host-side schema
    /// validation, and [`ProviderFactory::validate_config`] already ran.
    fn create_provider(
        &self,
        binding: Option<&str>,
        config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>>;

    /// Create a normalizer instance from configuration attributes.
    ///
    /// `binding` semantics match [`create_provider`]: `Some(name)` for
    /// a named instance, `None` for the kind's default. Returns an explicit
    /// [`NoopNormalizer`] by default. Provider and boundary failures must be
    /// returned rather than replaced with a no-op implementation. The same
    /// checked configuration witness is required as for provider creation.
    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }

    /// Get all resource schemas for this provider.
    fn schemas(&self) -> Vec<crate::schema::ResourceSchema>;

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

/// Collect all resource schemas from the given factories into a [`SchemaRegistry`].
///
/// Each schema is inserted under the factory's `name()` plus `schema.kind`,
/// so a given `(provider, resource_type)` pair may have both a `Managed`
/// and a `DataSource` entry registered side by side.
pub fn collect_schemas(factories: &[Box<dyn ProviderFactory>]) -> SchemaRegistry {
    let mut registry = SchemaRegistry::new();
    for factory in factories {
        for schema in factory.schemas() {
            registry.insert(factory.name(), schema);
        }
    }
    registry
}

/// Extract custom type validators from a registry.
///
/// Walks all `AttributeType::Custom` types in every registered schema and
/// returns a map of (snake_case type name → validator function) suitable
/// for populating `ProviderContext.validators`.
pub fn collect_custom_type_validators(
    registry: &SchemaRegistry,
) -> HashMap<TypeIdentity, crate::parser::ValidatorFn> {
    let mut validators: HashMap<TypeIdentity, crate::parser::ValidatorFn> = HashMap::new();

    for (_provider, _resource_type, _kind, schema) in registry.iter() {
        for attr_schema in schema.attributes.values() {
            collect_validators_from_type(&attr_schema.attr_type, &mut validators);
        }
        // Visit cyclic-struct definitions directly so any
        // `Custom { identity, validate }` nested inside a `Ref` target
        // (e.g. WAFv2 `Statement -> AndStatement` carrying a custom
        // ARN type) gets its validator registered. The recursion in
        // `collect_validators_from_type` stops at `Ref` to avoid
        // infinite cycles; iterating `schema.defs.values()` here
        // closes the loop in one bounded pass (carina#3340).
        for def in schema.defs.values() {
            collect_validators_from_type(def, &mut validators);
        }
    }

    validators
}

/// Collect every identity-bearing schema type used by type-expression
/// validation.
///
/// The retained [`AttributeType`] is the authority needed to turn an authored
/// enum identifier into provider API spelling. Walking each definition once
/// mirrors [`collect_custom_type_validators`] and avoids recursively following
/// cyclic [`crate::schema::AttrTypeKind::Ref`] graphs.
pub fn collect_custom_type_attribute_types(
    registry: &SchemaRegistry,
) -> HashMap<TypeIdentity, crate::schema::AttributeType> {
    let mut types = HashMap::new();
    for (_provider, _resource_type, _kind, schema) in registry.iter() {
        for attr_schema in schema.attributes.values() {
            collect_identified_attribute_types_from_type(&attr_schema.attr_type, &mut types);
        }
        for def in schema.defs.values() {
            collect_identified_attribute_types_from_type(def, &mut types);
        }
    }
    types
}

fn collect_identified_attribute_types_from_type(
    attr_type: &crate::schema::AttributeType,
    types: &mut HashMap<TypeIdentity, crate::schema::AttributeType>,
) {
    use crate::schema::RawShape;

    match attr_type.raw_shape() {
        RawShape::String {
            identity: Some(identity),
            ..
        }
        | RawShape::Int {
            identity: Some(identity),
            ..
        }
        | RawShape::Float {
            identity: Some(identity),
            ..
        }
        | RawShape::Enum { identity, .. } => {
            types
                .entry(identity.clone())
                .or_insert_with(|| attr_type.clone());
        }
        RawShape::List { element_type, .. } => {
            collect_identified_attribute_types_from_type(element_type, types);
        }
        RawShape::Map { key, value } => {
            collect_identified_attribute_types_from_type(key, types);
            collect_identified_attribute_types_from_type(value, types);
        }
        RawShape::Struct { fields, .. } => {
            for field in fields {
                collect_identified_attribute_types_from_type(&field.field_type, types);
            }
        }
        RawShape::Union(members) => {
            for member in members {
                collect_identified_attribute_types_from_type(member, types);
            }
        }
        RawShape::Ref(_)
        | RawShape::String { identity: None, .. }
        | RawShape::Int { identity: None, .. }
        | RawShape::Float { identity: None, .. }
        | RawShape::Bool
        | RawShape::Duration => {}
    }
}

/// Collect custom type identities from a registry without allocating
/// validators.
///
/// Cheaper than `collect_custom_type_validators` when only the
/// identities are needed (e.g., for LSP completions).
pub fn collect_custom_type_names(registry: &SchemaRegistry) -> Vec<TypeIdentity> {
    let mut names = std::collections::HashSet::new();

    for (_provider, _resource_type, _kind, schema) in registry.iter() {
        for attr_schema in schema.attributes.values() {
            collect_type_names_from_type(&attr_schema.attr_type, &mut names);
        }
        // Walk cyclic-struct definitions for the same reason as
        // `collect_custom_type_validators` above (carina#3340).
        for def in schema.defs.values() {
            collect_type_names_from_type(def, &mut names);
        }
    }

    names.into_iter().collect()
}

/// Recursively extract Custom type validators from an AttributeType.
fn collect_validators_from_type(
    attr_type: &crate::schema::AttributeType,
    validators: &mut HashMap<TypeIdentity, crate::parser::ValidatorFn>,
) {
    use crate::schema::RawShape;

    match attr_type.raw_shape() {
        RawShape::String {
            identity: Some(id),
            validate,
            ..
        }
        | RawShape::Int {
            identity: Some(id),
            validate,
            ..
        }
        | RawShape::Float {
            identity: Some(id),
            validate,
            ..
        } => {
            validators.entry(id.clone()).or_insert_with(|| {
                let validate_fn = validate.clone();
                Box::new(move |s: &str| {
                    validate_fn(&crate::resource::Value::Concrete(
                        crate::resource::ConcreteValue::String(s.to_string().into()),
                    ))
                    .map_err(|e| e.to_string())
                })
            });
        }
        RawShape::String { identity: None, .. }
        | RawShape::Int { identity: None, .. }
        | RawShape::Float { identity: None, .. } => {}
        RawShape::List {
            element_type: inner,
            ..
        } => {
            collect_validators_from_type(inner, validators);
        }
        RawShape::Map { key, value: inner } => {
            collect_validators_from_type(key, validators);
            collect_validators_from_type(inner, validators);
        }
        RawShape::Struct { fields, .. } => {
            for field in fields {
                collect_validators_from_type(&field.field_type, validators);
            }
        }
        RawShape::Union(types) => {
            for t in types {
                collect_validators_from_type(t, validators);
            }
        }
        // `AttributeType::Ref`: do NOT recurse into the resolved target
        // here — that would loop forever on cyclic CFN schemas
        // (`Statement -> AndStatement -> List<Statement>`). The
        // callers (`collect_custom_type_validators` /
        // `collect_custom_type_names`) walk every entry of
        // `schema.defs` directly in a separate loop, which terminates
        // because each def is visited exactly once. (carina#3340.)
        RawShape::Ref(_) => {}
        // Primitives carry no nested Custom types and no Ref.
        RawShape::Bool | RawShape::Duration | RawShape::Enum { .. } => {}
    }
}

/// Recursively collect Custom type identities from an AttributeType
/// (identities only, no closures).
fn collect_type_names_from_type(
    attr_type: &crate::schema::AttributeType,
    names: &mut std::collections::HashSet<TypeIdentity>,
) {
    use crate::schema::RawShape;

    match attr_type.raw_shape() {
        RawShape::String {
            identity: Some(id), ..
        }
        | RawShape::Int {
            identity: Some(id), ..
        }
        | RawShape::Float {
            identity: Some(id), ..
        } => {
            names.insert(id.clone());
        }
        RawShape::String { identity: None, .. }
        | RawShape::Int { identity: None, .. }
        | RawShape::Float { identity: None, .. } => {}
        RawShape::List {
            element_type: inner,
            ..
        } => {
            collect_type_names_from_type(inner, names);
        }
        RawShape::Map { key, value: inner } => {
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
        // See `collect_validators_from_type` for the rationale: the
        // caller walks `schema.defs` separately to avoid infinite
        // recursion on cyclic schemas (carina#3340).
        RawShape::Ref(_) => {}
        RawShape::Bool | RawShape::Duration | RawShape::Enum { .. } => {}
    }
}

/// Provider implementation for Box<dyn Provider>
/// This enables dynamic dispatch for Providers
impl Provider for Box<dyn Provider> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
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
    ) -> BoxFuture<'_, ProviderResult<CreateOutcome>> {
        (**self).create(id, request)
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        (**self).update(id, identifier, request)
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        (**self).delete(id, identifier, request)
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
    use super::*;

    #[test]
    fn enum_canonicalization_error_renders_its_reason_once() {
        let mut resources = vec![
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

        let provider_error = ProviderError::from(error);

        assert!(matches!(provider_error, ProviderError::InvalidInput(_)));
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
    fn raw_legacy_persisted_attributes_require_a_state_row_constructor() {
        let marker = format!("{}{}", crate::value::LEGACY_SECRET_PREFIX, "ab".repeat(32));
        let encoded = serde_json::json!({ "token": marker });
        assert!(
            serde_json::from_value::<PersistedAttributes>(encoded.clone()).is_err(),
            "context-free serde must not construct a typed legacy digest",
        );
        let error = PersistedAttributes::from_encoded(
            serde_json::from_value(encoded.clone()).expect("attribute object"),
        )
        .expect_err("the checked context-free constructor must reject raw legacy markers");
        assert_eq!(error.attribute_key(), "token");

        let id = ResourceId::with_provider_identity("mock", "test.resource", "old", None);
        let persisted = persisted_attributes_from_test_state_row(
            &id,
            serde_json::from_value(encoded).expect("attribute object"),
        )
        .expect("state row decode");
        assert!(matches!(
            persisted.get("token"),
            Some(Value::Deferred(DeferredValue::RedactedSecret { hash }))
                if hash.persisted_marker().starts_with(
                    crate::value::EXPLICIT_LEGACY_SECRET_PREFIX
                )
        ));
    }

    #[test]
    fn state_loaded_legacy_attributes_round_trip_through_context_free_serde() {
        let marker = format!("{}{}", crate::value::LEGACY_SECRET_PREFIX, "ab".repeat(32));
        let id = ResourceId::with_provider_identity("mock", "test.resource", "old", None);
        let loaded = persisted_attributes_from_test_state_row(
            &id,
            HashMap::from([("token".to_string(), serde_json::json!(marker))]),
        )
        .expect("state row decode");

        let host_wire = serde_json::to_value(&loaded).expect("serialize host saved attributes");
        let guest: PersistedAttributes = serde_json::from_value(host_wire)
            .expect("guest must decode the host's self-contained legacy marker");
        let Some(Value::Deferred(DeferredValue::RedactedSecret { hash })) = guest.get("token")
        else {
            panic!("guest decode must preserve legacy secret semantics")
        };
        assert!(
            hash.persisted_marker()
                .starts_with("_secret:argon2-legacy:"),
            "host wire must carry the explicit legacy salt",
        );
    }

    #[test]
    fn state_loaded_fallback_salt_legacy_digest_is_self_contained_and_verifiable() {
        let material = br#""fallback-write-only""#;
        let mut output = [0_u8; 32];
        argon2::Argon2::default()
            .hash_password_into(material, b"carina-secret-v1", &mut output)
            .expect("hash pre-v12 fallback-salt fixture");
        let marker = format!(
            "{}{}",
            crate::value::LEGACY_SECRET_PREFIX,
            hex::encode(output)
        );
        let id = ResourceId::with_provider_identity("mock", "test.resource", "old", None);
        let loaded = persisted_attributes_from_test_state_row(
            &id,
            HashMap::from([("password".to_string(), serde_json::json!(marker))]),
        )
        .expect("state row decode");
        let round_trip: PersistedAttributes = serde_json::from_value(
            serde_json::to_value(loaded).expect("serialize upgraded legacy digest"),
        )
        .expect("decode upgraded legacy digest without row context");
        let hash = round_trip
            .redacted_secret_hash("password")
            .expect("upgraded marker remains typed");

        assert!(hash.verify(material));
        assert!(
            hash.persisted_marker()
                .starts_with(crate::value::EXPLICIT_LEGACY_SECRET_PREFIX)
        );
    }

    #[test]
    fn state_row_upgrade_rejects_legacy_salt_too_long_for_its_marker_format() {
        let marker = format!("{}{}", crate::value::LEGACY_SECRET_PREFIX, "ab".repeat(32));
        let encoded: EncodedStateRowAttributes = serde_json::from_value(serde_json::json!({
            "provider": "mock",
            "resource_type": "test.resource",
            "identity": "x".repeat(5000),
            "attributes": { "token": marker },
        }))
        .expect("decode oversized state row shape");

        let error = encoded
            .decode()
            .expect_err("an upgraded marker must never exceed the parser's salt bound");
        assert_eq!(error.attribute_key(), "token");
        assert!(matches!(
            error,
            PersistedAttributesError::LegacySaltTooLong { .. }
        ));
    }

    #[test]
    fn saved_plan_hash_inventory_ignores_non_placeholder_siblings() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("SAVED_PLAN_SECRET".into()),
        ))));
        let placeholder =
            crate::value::redact_secrets_in_value(&secret).expect("redact saved-plan secret");
        let attributes = HashMap::from([
            (
                "tags".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    ("S".to_string(), placeholder.clone()),
                    (
                        "Vpc".to_string(),
                        Value::resource_ref("vpc", "identifier", Vec::new()),
                    ),
                ]))),
            ),
            (
                "items".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::resource_ref("vpc", "identifier", Vec::new()),
                    placeholder,
                ])),
            ),
        ]);

        let hashes = PersistedAttributes::from_saved_plan_attributes(&attributes);
        let tags = hashes
            .encoded_value("tags")
            .and_then(serde_json::Value::as_object)
            .expect("sparse marker object");
        assert!(
            tags.get("S")
                .and_then(serde_json::Value::as_str)
                .is_some_and(crate::value::is_persisted_secret),
        );
        assert!(!tags.contains_key("Vpc"));
        let items = hashes
            .encoded_value("items")
            .and_then(serde_json::Value::as_array)
            .expect("sparse marker list");
        assert!(items[0].is_null(), "list indexes must remain stable");
        assert!(
            items[1]
                .as_str()
                .is_some_and(crate::value::is_persisted_secret),
        );
    }

    #[test]
    fn provider_secret_hash_drops_null_fields_before_hashing() {
        let provider_json = serde_json::json!({ "a": "x", "b": null });
        let marker = hash_provider_json_as_secret(&provider_json, None)
            .expect("hash canonical provider secret");
        let digest =
            crate::value::persisted_secret_hash(marker.as_str().expect("persisted marker string"))
                .expect("typed secret digest");
        let desired_material =
            serde_json::to_vec(&serde_json::json!({ "a": "x" })).expect("serialize desired secret");

        assert!(
            digest.verify(desired_material),
            "provider-only null fields must not create a phantom secret diff",
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
                    Value::resource_ref("management", "owner", Vec::new()),
                ),
            ]),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let mut resources = vec![Resource::with_provider(
            "aws",
            "service.Widget",
            "example",
            None,
        )];
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            crate::schema::ResourceSchema::new("service.Widget").attribute(
                crate::schema::AttributeSchema::new(
                    "tags",
                    crate::schema::AttributeType::map(crate::schema::AttributeType::string()),
                ),
            ),
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
        let mut resources = vec![
            Resource::with_provider("aws", "service.Widget", "example", None).with_attribute(
                "tags",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "Name".to_string(),
                    Value::Concrete(ConcreteValue::String("example".into())),
                )]))),
            ),
        ];
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            crate::schema::ResourceSchema::new("service.Widget").attribute(
                crate::schema::AttributeSchema::new(
                    "tags",
                    crate::schema::AttributeType::map(crate::schema::AttributeType::string()),
                ),
            ),
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
            Value::resource_ref("management", "owner", Vec::new()),
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
            crate::schema::ResourceSchema::new("service.Widget").attribute(
                crate::schema::AttributeSchema::new(
                    "status",
                    crate::schema::AttributeType::enum_(
                        crate::schema::enum_identity("Status", Some("aws.service.Widget")),
                        Some(vec!["Enabled".to_string()]),
                        vec![("Enabled".to_string(), "enabled".to_string())],
                        None,
                        None,
                    ),
                ),
            ),
        );
        registry
    }

    #[test]
    fn lift_covers_orphan_entries_absent_from_any_resource_slice() {
        let orphan = ResourceId::with_provider_identity("aws", "service.Widget", "orphan", None);
        let raw = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
            orphan.clone(),
            HashMap::from([(
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("Enabled".into())),
            )]),
        )]));

        let lifted = raw.lift(&widget_status_registry());

        assert!(matches!(
            &lifted.get(&orphan).unwrap()["status"],
            Value::Concrete(ConcreteValue::CanonicalEnum(value))
                if value.api_value() == "Enabled"
        ));
    }

    #[test]
    fn raw_saved_attrs_lift_is_map_driven_and_remappable() {
        let from = ResourceId::with_provider_identity("aws", "service.Widget", "old", None);
        let to = ResourceId::with_provider_identity("aws", "service.Widget", "new", None);
        let raw = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
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
            Value::Concrete(ConcreteValue::CanonicalEnum(value))
                if value.api_value() == "Enabled"
        ));

        lifted.remap_resource_id(&from, to.clone());
        assert!(lifted.get(&from).is_none());
        assert!(lifted.get(&to).is_some());
    }

    #[test]
    fn raw_saved_attrs_lift_structurally_canonicalizes_nested_values() {
        use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

        let statement = AttributeType::struct_(
            "Statement".to_string(),
            vec![StructField::new(
                "action",
                AttributeType::union(vec![
                    AttributeType::string(),
                    AttributeType::list(AttributeType::string()),
                ]),
            )],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            ResourceSchema::new("iam.policy")
                .attribute(AttributeSchema::new("statement", statement)),
        );

        let id = ResourceId::with_provider_identity("aws", "iam.policy", "p1", None);
        let raw = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
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
        let Value::Concrete(ConcreteValue::Map(statement)) = &lifted.get(&id).unwrap()["statement"]
        else {
            panic!("statement must remain a map");
        };
        assert_eq!(
            statement["action"],
            Value::Concrete(ConcreteValue::StringList(vec!["s3:GetObject".into()])),
            "carina#3740: RawSavedAttrs::lift must apply the shared structural canonicalizer"
        );
    }

    #[test]
    fn raw_saved_attrs_lift_preserves_attribute_absent_from_schema() {
        let id = ResourceId::with_provider_identity("aws", "service.Widget", "widget", None);
        let original = Value::Concrete(ConcreteValue::String("opaque".into()));
        let raw = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
            id.clone(),
            HashMap::from([("provider_extension".to_string(), original.clone())]),
        )]));

        let lifted = raw.lift(&widget_status_registry());

        assert_eq!(
            lifted.get(&id).unwrap()["provider_extension"],
            original,
            "carina#3740: a saved attribute absent from a known resource schema must pass through"
        );
    }

    #[test]
    fn raw_saved_attrs_lift_skips_resource_type_absent_from_registry() {
        let id = ResourceId::with_provider_identity("aws", "service.Unknown", "unknown", None);
        let original = Value::Concrete(ConcreteValue::String("Enabled".into()));
        let raw = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
            id.clone(),
            HashMap::from([("status".to_string(), original.clone())]),
        )]));

        let lifted = raw.lift(&widget_status_registry());

        assert_eq!(
            lifted.get(&id).unwrap()["status"],
            original,
            "carina#3740: a saved entry whose resource type is absent from the registry must be skipped"
        );
    }

    #[test]
    fn raw_saved_attrs_lift_is_idempotent() {
        use crate::schema::{AttributeSchema, ResourceSchema};

        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            ResourceSchema::new("service.Widget")
                .attribute(AttributeSchema::new(
                    "status",
                    crate::schema::AttributeType::enum_(
                        crate::schema::enum_identity("Status", Some("aws.service.Widget")),
                        Some(vec!["Enabled".to_string()]),
                        vec![("Enabled".to_string(), "enabled".to_string())],
                        None,
                        None,
                    ),
                ))
                .attribute(AttributeSchema::new(
                    "subjects",
                    crate::schema::AttributeType::union(vec![
                        crate::schema::AttributeType::string(),
                        crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
                    ]),
                )),
        );
        let id = ResourceId::with_provider_identity("aws", "service.Widget", "widget", None);
        let raw = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
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
        let twice = RawSavedAttrs::from_decoded_values_for_test(once.0.0.clone()).lift(&registry);
        let sorted_pairs = |attrs: &HashMap<String, Value>| {
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
        use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

        let config = AttributeType::struct_(
            "Config".to_string(),
            vec![StructField::new(
                "password",
                AttributeType::union(vec![
                    AttributeType::string(),
                    AttributeType::list(AttributeType::string()),
                ]),
            )],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            ResourceSchema::new("service.Widget").attribute(AttributeSchema::new("config", config)),
        );

        let id = ResourceId::with_provider_identity("test", "service.Widget", "widget", None);
        let secret_hash = Value::Deferred(DeferredValue::RedactedSecret {
            hash: RedactedSecretHash::generate(b"saved-attrs-test"),
        });
        let raw = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
            id.clone(),
            HashMap::from([(
                "config".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "password".to_string(),
                    secret_hash.clone(),
                )]))),
            )]),
        )]));

        let lifted = raw.lift(&registry);
        let Value::Concrete(ConcreteValue::Map(config)) = &lifted.get(&id).unwrap()["config"]
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
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, StructField, enum_identity,
        };

        // Persisted JSON strings currently decode as ConcreteValue::String, so
        // EnumIdentifier is a seam-only phase probe rather than a reachable
        // state-file shape. StateText case-matches `foo` to API member `FOO`;
        // RawDsl would treat it as the alias for the different member `API_A`.
        // The Union wrapper ensures the separate enum-lift walker does not
        // perform the conversion first.
        let mode = AttributeType::enum_(
            enum_identity("Mode", Some("awscc.example.Widget")),
            Some(vec!["API_A".to_string(), "FOO".to_string()]),
            vec![("API_A".to_string(), "foo".to_string())],
            None,
            None,
        );
        let settings = AttributeType::struct_(
            "Settings".to_string(),
            vec![StructField::new(
                "mode",
                AttributeType::union(vec![mode.clone(), AttributeType::list(mode)]),
            )],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "awscc",
            ResourceSchema::new("example.Widget")
                .attribute(AttributeSchema::new("settings", settings)),
        );

        let id = ResourceId::with_provider_identity("awscc", "example.Widget", "widget", None);
        let raw = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
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
        let Value::Concrete(ConcreteValue::Map(settings)) = &lifted.get(&id).unwrap()["settings"]
        else {
            panic!("settings must remain a map");
        };
        let Value::Concrete(ConcreteValue::CanonicalEnum(value)) = &settings["mode"] else {
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
        let gate = crate::executor::ModuleConstraintGate::new(&[]);
        let schemas = crate::schema::SchemaRegistry::new();
        let context = crate::executor::ProviderPreparationContext::new(
            &bindings,
            &gate,
            &[],
            &NoopNormalizer,
            &[],
            &schemas,
        );
        futures::executor::block_on(crate::executor::prepare_provider_ready_resource(
            resource, &context,
        ))
        .expect("test resource should pass provider preparation")
    }

    fn ready_data_source_for_test(resource: DataSource) -> ProviderReadyDataSource {
        let bindings = crate::binding_index::ResolvedBindings::default();
        let gate = crate::executor::ModuleConstraintGate::new(&[]);
        let schemas = crate::schema::SchemaRegistry::new();
        let context = crate::executor::ProviderPreparationContext::new(
            &bindings,
            &gate,
            &[],
            &NoopNormalizer,
            &[],
            &schemas,
        );
        crate::executor::prepare_provider_ready_data_source(resource, &context)
            .expect("test data source should pass provider preparation")
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
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
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
        ) -> BoxFuture<'_, ProviderResult<CreateOutcome>> {
            let id = id.clone();
            let attrs = request.resource().as_resource().attributes.clone();
            Box::pin(async move {
                let state = State::existing(id, crate::resource::attrs_to_hashmap(&attrs))
                    .with_identifier("mock-id-123");
                Ok(CreateOutcome::Success { state })
            })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            let id = id.clone();
            // Apply the patch on top of `from` so the test sees the
            // user-specified changes round-tripped into State.
            let mut attrs = request.from().attributes.clone();
            for op in &request.patch().ops {
                match op.kind {
                    PatchOpKind::Add | PatchOpKind::Replace => {
                        if let Some(v) = &op.value {
                            attrs.insert(op.key.clone(), v.clone());
                        }
                    }
                    PatchOpKind::Remove => {
                        attrs.remove(&op.key);
                    }
                }
            }
            Box::pin(async move {
                Ok(UpdateOutcome::Success {
                    state: State::existing(id, attrs),
                })
            })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
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
        let state = provider.read(&id, None, ReadRequest).await.unwrap();
        assert!(!state.exists);
    }

    #[test]
    fn create_outcome_exposes_state_and_diagnostic() {
        let id = ResourceId::with_identity("test", "example");
        let state = State::existing(id, HashMap::new()).with_identifier("mock-id");
        let diagnostic = PartialReadDiagnostic::new(
            "mock partial create".to_string(),
            vec!["dns_name".to_string()],
        )
        .expect("missing attributes are non-empty");

        let success = CreateOutcome::Success {
            state: state.clone(),
        };
        assert_eq!(success.diagnostic(), None);
        assert_eq!(success.into_state_for_writeback(), state.clone());

        let partial = CreateOutcome::partial_success(
            state.clone(),
            "mock partial create".to_string(),
            vec!["dns_name".to_string()],
        );

        assert_eq!(partial.diagnostic(), Some(&diagnostic));
        let state = partial.into_state_for_writeback();
        assert_eq!(state.partial_read.unwrap().missing_attributes.len(), 1);
    }

    #[test]
    fn create_outcome_empty_partial_diagnostic_becomes_success() {
        let id = ResourceId::with_identity("test", "example");
        let state = State::existing(id, HashMap::new()).with_identifier("mock-id");

        let outcome = CreateOutcome::partial_success(
            state.clone(),
            "mock partial create".to_string(),
            vec![],
        );

        assert_eq!(outcome.diagnostic(), None);
        assert_eq!(outcome.into_state_for_writeback(), state);
    }

    #[test]
    fn partial_read_diagnostic_merge_options_covers_all_cases() {
        let a = PartialReadDiagnostic::new(
            "rename partial".to_string(),
            vec!["rename_attr".to_string(), "shared".to_string()],
        )
        .expect("missing attributes are non-empty");
        let b = PartialReadDiagnostic::new(
            "create partial".to_string(),
            vec!["create_attr".to_string(), "shared".to_string()],
        )
        .expect("missing attributes are non-empty");

        assert_eq!(PartialReadDiagnostic::merge_options(None, None), None);
        assert_eq!(
            PartialReadDiagnostic::merge_options(Some(a.clone()), None),
            Some(a.clone())
        );
        assert_eq!(
            PartialReadDiagnostic::merge_options(None, Some(b.clone())),
            Some(b.clone())
        );

        let merged = PartialReadDiagnostic::merge_options(Some(a), Some(b))
            .expect("both diagnostics should merge into one");
        assert_eq!(merged.reason(), "rename partial");
        assert_eq!(
            merged.missing_attributes(),
            &[
                "rename_attr".to_string(),
                "shared".to_string(),
                "create_attr".to_string(),
            ]
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
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
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
        ) -> BoxFuture<'_, ProviderResult<CreateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
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
    async fn read_data_source_delegates_to_read_for_zero_input() {
        // MockProvider's read_data_source delegates to read(&resource.id, None).
        // For MockProvider this returns not_found.
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
        router.add_provider("input-aware".to_string(), Box::new(InputAwareProvider));

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
        let id = resource.id.clone();
        let state = provider
            .create(&id, CreateRequest::checked(resolved_for_test(resource)))
            .await
            .unwrap()
            .into_state_for_writeback();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("mock-id-123".to_string()));
    }

    #[tokio::test]
    async fn provider_router_dispatches_read_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider_identity("mock", "test", "example", None);
        let state = router.read(&id, None, ReadRequest).await.unwrap();
        assert!(!state.exists);
    }

    #[tokio::test]
    async fn provider_router_attaches_provider_context_to_normalizer_errors() {
        struct FailingNormalizer;

        impl ProviderNormalizer for FailingNormalizer {
            fn normalize_desired<'a>(
                &'a self,
                _resources: &'a mut [Resource],
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }

            fn normalize_state<'a>(
                &'a self,
                _current_states: &'a mut HashMap<ResourceId, State>,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                Box::pin(async { Err(ProviderError::internal("normalization failed")) })
            }

            fn hydrate_read_state<'a>(
                &'a self,
                _current_states: &'a mut HashMap<ResourceId, State>,
                _saved_attrs: &'a SavedAttrs,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }
        }

        let mut router = ProviderRouter::new();
        router.add_normalizer("awscc", Box::new(FailingNormalizer));

        let error = router
            .normalize_state(&mut HashMap::new())
            .await
            .expect_err("normalizer failure must propagate");

        assert_eq!(error.detail().provider_name.as_deref(), Some("awscc"));
    }

    #[tokio::test]
    async fn provider_router_dispatches_create_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let resource = Resource::with_provider("mock", "test", "example", None);
        let id = resource.id.clone();
        let state = router
            .create(&id, CreateRequest::checked(resolved_for_test(resource)))
            .await
            .unwrap()
            .into_state_for_writeback();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("mock-id-123".to_string()));
    }

    #[test]
    fn provider_error_is_terminal_but_keeps_typed_cause() {
        use std::error::Error;
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::api_error_caused_by("Failed to create resource", cause);
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
        let err = ProviderError::api_error_caused_by("Failed to create resource", cause);
        let display = format!("{}", err);
        assert!(
            display.contains("connection refused"),
            "Display should include cause message, got: {}",
            display
        );
    }

    #[test]
    fn provider_error_display_without_cause() {
        let err = ProviderError::internal("simple error");
        let display = format!("{}", err);
        assert_eq!(display, "simple error");
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
        );

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
        let error = ProviderError::internal_caused_by("verification failed", cause);
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
        struct ProviderBoundaryError(ProviderError);

        impl std::fmt::Display for ProviderBoundaryError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "provider boundary failed: {}", self.0)
            }
        }
        impl std::error::Error for ProviderBoundaryError {}

        let inner = ProviderError::api_error_caused_by(
            "provider request failed",
            std::io::Error::other("leaf service reason"),
        );
        let outer = ProviderError::internal_caused_by(
            "outer provider operation failed",
            ProviderBoundaryError(inner),
        );

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
        let err = ProviderError::api_error_caused_by("Failed to read", cause).for_resource(id);
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
        let err = ProviderError::api_error("Failed").with_operation("iam.ListRoles");
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
        let err =
            ProviderError::api_error_caused_by("Failed", outer).with_operation("iam.ListRoles");
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
        let err = ProviderError::api_error_caused_by("Failed to create resource", cause);
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
        let inv = ProviderError::invalid_input("bad");
        assert!(matches!(inv, ProviderError::InvalidInput(_)));
        assert_eq!(inv.to_string(), "bad");

        let api = ProviderError::api_error("rejected");
        assert!(matches!(api, ProviderError::ApiError(_)));

        let nf = ProviderError::not_found("missing");
        assert!(matches!(nf, ProviderError::NotFound(_)));

        let to = ProviderError::timeout("slow");
        assert!(matches!(to, ProviderError::Timeout(_)));

        let intl = ProviderError::internal("bug");
        assert!(matches!(intl, ProviderError::Internal(_)));
    }

    #[test]
    fn build_update_patch_classifies_ops() {
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
        let changed = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let patch = build_update_patch(&changed, &to, &from);
        assert_eq!(patch.ops.len(), 3);

        let by_key: HashMap<&str, &PatchOp> =
            patch.ops.iter().map(|op| (op.key.as_str(), op)).collect();
        let a = by_key["a"];
        assert_eq!(a.kind, PatchOpKind::Replace);
        assert_eq!(
            a.value,
            Some(Value::Concrete(ConcreteValue::String("new".into())))
        );
        let b = by_key["b"];
        assert_eq!(b.kind, PatchOpKind::Add);
        assert_eq!(
            b.value,
            Some(Value::Concrete(ConcreteValue::String("added".into())))
        );
        let c = by_key["c"];
        assert_eq!(c.kind, PatchOpKind::Remove);
        assert_eq!(c.value, None);
    }

    #[tokio::test]
    async fn provider_router_dispatches_update_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider_identity("mock", "test", "example", None);
        let from = State::existing(id.clone(), HashMap::new());
        let ready = resolved_for_test(Resource::with_provider("mock", "test", "example", None));
        let request = UpdateRequest::checked(from, &[], &ready);
        let state = router
            .update(&id, "mock-id-123", request)
            .await
            .unwrap()
            .into_state_for_writeback();
        assert!(state.exists);
    }

    #[tokio::test]
    async fn provider_router_dispatches_delete_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider_identity("mock", "test", "example", None);
        let request = DeleteRequest::default();
        let result = router.delete(&id, "mock-id-123", request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn provider_router_returns_error_for_unknown_provider() {
        let router = ProviderRouter::new();
        let id = ResourceId::with_provider_identity("nonexistent", "test", "example", None);
        let result = router.read(&id, None, ReadRequest).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ProviderError::Internal(_)));
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
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let mut attrs = HashMap::new();
            attrs.insert(
                "tag".to_string(),
                Value::Concrete(ConcreteValue::String(self.tag.to_string().into())),
            );
            let state = State::existing(id.clone(), attrs);
            Box::pin(async move { Ok(state) })
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
        ) -> BoxFuture<'_, ProviderResult<CreateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }
        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }
        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
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
            Box::new(TaggedProvider { tag: "default" }),
        );
        let named_config = ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: Some("us".to_string()),
        };
        router.add_provider_config(&named_config, Box::new(TaggedProvider { tag: "us" }));

        let default_id = ResourceId::with_provider_identity("mock", "test", "a", None);
        let state = router.read(&default_id, None, ReadRequest).await.unwrap();
        assert_eq!(
            state.attributes.get("tag"),
            Some(&Value::Concrete(ConcreteValue::String("default".into()))),
            "resources without provider_instance must route to the kind's default instance"
        );

        let us_id = ResourceId::with_provider_identity("mock", "test", "b", Some("us".to_string()));
        let state = router.read(&us_id, None, ReadRequest).await.unwrap();
        assert_eq!(
            state.attributes.get("tag"),
            Some(&Value::Concrete(ConcreteValue::String("us".into()))),
            "resources tagged with binding=Some('us') must route to that named instance"
        );
    }

    #[tokio::test]
    async fn provider_router_unknown_named_instance_errors_with_binding() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id =
            ResourceId::with_provider_identity("mock", "test", "x", Some("missing".to_string()));
        let err = router.read(&id, None, ReadRequest).await.unwrap_err();
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
        use crate::schema::ResourceSchema;

        struct FailingFactory;

        impl ProviderFactory for FailingFactory {
            fn name(&self) -> &str {
                "failing"
            }
            fn display_name(&self) -> &str {
                "Failing provider"
            }
            fn provider_config_attribute_types(
                &self,
            ) -> HashMap<String, crate::schema::AttributeType> {
                HashMap::new()
            }
            fn validate_config(
                &self,
                _attrs: &CanonicalProviderConfigAttributes,
            ) -> Result<(), String> {
                Ok(())
            }
            fn extract_region(&self, _attrs: &IndexMap<String, Value>) -> String {
                "us-east-1".to_string()
            }
            fn create_provider(
                &self,
                _binding: Option<&str>,
                _config: &ProviderReadyConfig,
            ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
                Box::pin(async {
                    Err(ProviderError::invalid_input(
                        "AWS account ID '019115212452' is not in the provider's \
                         allowed_account_ids [\"151116838382\"]. Refusing to operate \
                         against this account. Check the AWS credentials in your environment.",
                    ))
                })
            }
            fn schemas(&self) -> Vec<ResourceSchema> {
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
        let result = factory.create_provider(None, &config).await;
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

        use crate::schema::ResourceSchema;

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
            fn provider_config_attribute_types(
                &self,
            ) -> HashMap<String, crate::schema::AttributeType> {
                HashMap::new()
            }
            fn validate_config(
                &self,
                _attrs: &CanonicalProviderConfigAttributes,
            ) -> Result<(), String> {
                Ok(())
            }
            fn extract_region(&self, _attrs: &IndexMap<String, Value>) -> String {
                "ap-northeast-1".to_string()
            }
            fn create_provider(
                &self,
                binding: Option<&str>,
                _config: &ProviderReadyConfig,
            ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(binding.map(|s| s.to_string()));
                Box::pin(async { Ok(Box::new(MockProvider) as Box<dyn Provider>) })
            }
            fn schemas(&self) -> Vec<ResourceSchema> {
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
        let _ = factory.create_provider(None, &config).await.unwrap();
        let _ = factory.create_provider(Some("us"), &config).await.unwrap();
        let _ = factory
            .create_provider(Some("tokyo"), &config)
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
                resources: &'a mut [Resource],
            ) -> BoxFuture<'a, ProviderResult<()>> {
                Box::pin(async move {
                    // Prefix all string attribute values with "normalized:"
                    for resource in resources.iter_mut() {
                        for value in resource.attributes.values_mut() {
                            if let Value::Concrete(ConcreteValue::String(s)) = value {
                                *s = format!("normalized:{}", s.expose_plaintext()).into();
                            }
                        }
                    }
                    Ok(())
                })
            }

            fn normalize_state<'a>(
                &'a self,
                _current_states: &'a mut HashMap<ResourceId, State>,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }

            fn hydrate_read_state<'a>(
                &'a self,
                states: &'a mut HashMap<ResourceId, State>,
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
        let mut resources = vec![Resource::new("test", "example").with_attribute(
            "key",
            Value::Concrete(ConcreteValue::String("value".into())),
        )];
        ext.normalize_desired(&mut resources).await.unwrap();
        assert_eq!(
            resources[0].get_attr("key"),
            Some(&Value::Concrete(ConcreteValue::String(
                "normalized:value".into()
            )))
        );

        // Test hydrate_read_state
        let id = ResourceId::with_identity("test", "example");
        let mut states = HashMap::new();
        states.insert(id.clone(), State::existing(id.clone(), HashMap::new()));
        let saved = RawSavedAttrs::from_decoded_values_for_test(HashMap::from([(
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
            Some(&Value::Concrete(ConcreteValue::String("data".into())))
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
                _identifier: Option<&str>,
                _request: ReadRequest,
            ) -> BoxFuture<'_, ProviderResult<State>> {
                let id = id.clone();
                Box::pin(async move { Ok(State::not_found(id)) })
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
            ) -> BoxFuture<'_, ProviderResult<CreateOutcome>> {
                let id = id.clone();
                Box::pin(async move {
                    Ok(CreateOutcome::Success {
                        state: State::not_found(id),
                    })
                })
            }

            fn update(
                &self,
                id: &ResourceId,
                _identifier: &str,
                _request: UpdateRequest,
            ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
                let id = id.clone();
                Box::pin(async move {
                    Ok(UpdateOutcome::Success {
                        state: State::not_found(id),
                    })
                })
            }

            fn delete(
                &self,
                _id: &ResourceId,
                _identifier: &str,
                _request: DeleteRequest,
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
                resources: &'a mut [Resource],
            ) -> BoxFuture<'a, ProviderResult<()>> {
                Box::pin(async move {
                    for resource in resources.iter_mut() {
                        if resource.id.provider == "normalizing" {
                            assert!(matches!(
                                resource.get_attr("peer"),
                                Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
                            ));
                            assert!(matches!(
                                resource.get_attr("pending"),
                                Some(Value::Deferred(DeferredValue::Unknown(_)))
                            ));
                            for value in resource.attributes.values_mut() {
                                if let Value::Concrete(ConcreteValue::String(s)) = value {
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
                _current_states: &'a mut HashMap<ResourceId, State>,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }

            fn hydrate_read_state<'a>(
                &'a self,
                _current_states: &'a mut HashMap<ResourceId, State>,
                _saved_attrs: &'a SavedAttrs,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                ready_noop()
            }
        }

        let mut router = ProviderRouter::new();
        router.add_provider("normalizing".to_string(), Box::new(NormalizingProvider));
        router.add_normalizer("normalizing", Box::new(TestNormalizer));

        let peer_ref = Value::resource_ref("peer", "identifier", Vec::new());
        let pending = Value::Deferred(DeferredValue::Unknown(
            crate::resource::UnknownReason::ForValue,
        ));
        let mut resources = vec![
            Resource::with_provider("normalizing", "test", "example", None)
                .with_attribute("key", Value::Concrete(ConcreteValue::String("val".into())))
                .with_attribute("peer", peer_ref.clone())
                .with_attribute("pending", pending.clone()),
        ];
        router.normalize_desired(&mut resources).await.unwrap();
        assert_eq!(
            resources[0].get_attr("key"),
            Some(&Value::Concrete(ConcreteValue::String("norm:val".into())))
        );
        assert_eq!(resources[0].get_attr("peer"), Some(&peer_ref));
        assert!(matches!(
            resources[0].get_attr("pending"),
            Some(Value::Deferred(DeferredValue::Unknown(
                crate::resource::UnknownReason::ForValue
            )))
        ));
    }
}
