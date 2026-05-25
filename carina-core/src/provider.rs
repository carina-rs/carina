//! Provider - Trait abstracting resource operations
//!
//! A Provider defines operations for a specific infrastructure (AWS, GCP, etc.).
//! It is responsible for converting Effects into actual API calls.

use std::collections::HashMap;

use indexmap::IndexMap;
use std::future::Future;
use std::pin::Pin;

use crate::resource::{
    ConcreteValue, DataSource, Directives, ManagedResource, ResourceId, State, Value,
};
use crate::schema::{SchemaRegistry, TypeIdentity};

/// Contextual metadata attached to every [`ProviderError`] variant.
///
/// Mirrors `error-detail` in `wit/types.wit`. The `cause` chain stays
/// host-side (boxed `dyn Error`) and is flattened to a string when the
/// error crosses the WIT boundary.
#[derive(Debug, Default)]
pub struct ErrorDetail {
    /// Human-readable message.
    pub message: String,
    /// Resource the error pertains to, if known.
    pub resource_id: Option<Box<ResourceId>>,
    /// Underlying cause chain. Lost when the error crosses the WIT
    /// boundary — flattened to a string there.
    pub cause: Option<Box<dyn std::error::Error + Send + Sync>>,
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
        // between the structured multi-line render and the legacy
        // chain-walk render so non-AWS errors keep the same shape.
        if let Some(ref id) = detail.resource_id {
            write!(f, "[{}.{}] {}", id.resource_type, id.name, detail.message)?;
        } else {
            write!(f, "{}", detail.message)?;
        }

        // carina#3242: when any of the structured cloud-API fields are
        // populated, render the multi-line labeled shape and skip the
        // legacy chain walk (the structured fields carry the same
        // information without the SDK-internal scaffolding). When all
        // four are `None`, fall through to the legacy chain walk so
        // unmigrated provider call sites and non-cloud errors render
        // exactly as they did before.
        if detail.has_structured_cloud_fields() {
            return render_structured_cloud_fields(f, detail);
        }

        // carina#2603: walk the entire `source()` chain rather than
        // printing only the first level. AWS SDK errors typically carry
        // the actionable detail (error code, raw response message,
        // request id) two or three levels deep; a single `: {cause}`
        // would still hide e.g. "AccessDenied" behind a generic
        // "service error" wrapper.
        if let Some(ref cause) = detail.cause {
            let mut current: Option<&(dyn std::error::Error + 'static)> = Some(cause.as_ref());
            while let Some(c) = current {
                write!(f, ": {}", c)?;
                current = c.source();
            }
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
/// timeout) carry their diagnostic only in the cause chain — there was
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
    if let Some(ref cause) = detail.cause {
        // Walk the source chain (same shape as the legacy fallback)
        // and join with `: ` on the same `cause:` line. Keeps
        // transport-level diagnostics visible without bringing back
        // the SDK-scaffolding multi-line wall that motivated #3241.
        write!(f, "\n  cause: ")?;
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(cause.as_ref());
        let mut first = true;
        while let Some(c) = current {
            if first {
                write!(f, "{c}")?;
                first = false;
            } else {
                write!(f, ": {c}")?;
            }
            current = c.source();
        }
    }
    Ok(())
}

impl std::error::Error for ProviderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.detail()
            .cause
            .as_ref()
            .map(|e| e.as_ref() as &dyn std::error::Error)
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

    /// Convenience accessor for the human-readable message.
    pub fn message(&self) -> &str {
        &self.detail().message
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

    /// Attach a resource id to the inner detail.
    pub fn for_resource(mut self, id: ResourceId) -> Self {
        self.detail_mut().resource_id = Some(Box::new(id));
        self
    }

    /// Attach a cause chain to the inner detail.
    pub fn with_cause(mut self, cause: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.detail_mut().cause = Some(Box::new(cause));
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

    /// Returns `true` when at least one of the cloud-API metadata
    /// fields (`operation`, `status`, `code`, `request_id`) is set.
    ///
    /// `Display` uses this as the gate for the multi-line labeled
    /// render (carina#3242): any of the four flips the renderer into
    /// the structured shape. When none are set the error keeps the
    /// existing chain-walking single-line render — that's the
    /// fallback path for unmigrated provider call sites and for
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

pub type ProviderResult<T> = Result<T, ProviderError>;

/// Per-operation request record for [`Provider::create`].
///
/// Mirrors `create-request` in `wit/types.wit`.
#[derive(Debug, Clone)]
pub struct CreateRequest {
    /// Full desired state for the new resource.
    pub resource: ManagedResource,
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
/// provider should write — there is no separate `to: ManagedResource`
/// because exposing the full desired resource invites providers to
/// touch fields the user never specified (the root cause of
/// `carina-rs/carina#2559`).
#[derive(Debug, Clone)]
pub struct UpdateRequest {
    /// Current provider-side state. May be used for read-modify-write
    /// paths or for resolving server-assigned identifiers; MUST NOT be
    /// used to derive additional fields to write back.
    pub from: State,
    /// Structured description of the user's intended change.
    pub patch: UpdatePatch,
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
/// `ops`.
#[derive(Debug, Clone, Default)]
pub struct UpdatePatch {
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
pub fn build_update_patch(
    changed_attributes: &[String],
    to: &ManagedResource,
    from: &State,
) -> UpdatePatch {
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
/// Used by `ProviderNormalizer::hydrate_read_state` to carry forward
/// attributes that APIs don't return in read responses.
pub type SavedAttrs = HashMap<ResourceId, HashMap<String, Value>>;

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
    /// Unlike [`Provider::read`], this receives the full [`DataSource`] so the
    /// provider can see the user-supplied input attributes (e.g. the
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
    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>>;

    /// Create the resource described by `request.resource` and return
    /// the resulting state (with `identifier` set to the cloud-side
    /// internal ID, e.g. `vpc-xxx`).
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>>;

    /// Update an existing resource by applying `request.patch`.
    ///
    /// Each [`PatchOp`] corresponds to a key the user explicitly
    /// specified or removed in the desired state. Fields the user has
    /// never specified do not appear in the patch.
    ///
    /// **Providers MUST NOT modify any attribute that is not
    /// represented in `request.patch.ops`.** The patch is the sole
    /// source of truth for the update payload.
    ///
    /// `request.from` is the current provider-side state and may be
    /// used for read-modify-write paths or for resolving
    /// server-assigned identifiers; it MUST NOT be used to derive
    /// additional fields to write back.
    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>>;

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
}

/// Convenience for a `ProviderNormalizer` method that does nothing.
///
/// Returns an immediately-ready future. A `BoxFuture`-returning trait
/// method cannot have an empty `{}` default body, so every "I don't
/// normalize this" implementation returns this explicitly — keeping the
/// no-op a deliberate, visible choice rather than a silent default
/// (the hazard that caused carina-rs/carina-provider-awscc#192).
pub fn ready_noop<'a>() -> BoxFuture<'a, ()> {
    Box::pin(async {})
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
/// carina#3112). Each method mutates its arguments in place and returns
/// nothing; the returned future borrows the arguments for `'a`, so
/// callers must `.await` it before the borrow ends (they always do —
/// the futures are never run concurrently).
pub trait ProviderNormalizer: Send + Sync {
    /// Normalize desired resource state before diffing.
    ///
    /// For example, resolves bare enum identifiers like `advanced` or
    /// `Tier.advanced` into fully-qualified DSL format like
    /// `awscc.ec2_ipam.Tier.advanced` based on schema definitions.
    /// Providers without enum types return [`ready_noop`].
    fn normalize_desired<'a>(&'a self, resources: &'a mut [ManagedResource]) -> BoxFuture<'a, ()>;

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
    ) -> BoxFuture<'a, ()>;

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
    ) -> BoxFuture<'a, ()>;

    /// Merge default tags from provider configuration into resources that support tags.
    ///
    /// For each resource whose schema includes a `tags` attribute:
    /// - If the resource has no `tags`, set it to `default_tags`
    /// - If the resource has `tags`, merge default_tags (resource-level tags win on conflict)
    ///
    /// Records which tag keys came from defaults in the `_default_tag_keys` internal
    /// metadata attribute.
    ///
    /// No default body: an implicit no-op silently swallowed
    /// `WasmProviderNormalizer`'s missing dispatch in
    /// carina-rs/carina-provider-awscc#192. Every implementation now picks
    /// explicitly between [`merge_default_tags_for_provider`], a custom
    /// merge, or [`ready_noop`]'s deliberate no-op.
    fn merge_default_tags<'a>(
        &'a self,
        resources: &'a mut [ManagedResource],
        default_tags: &'a IndexMap<String, Value>,
        registry: &'a SchemaRegistry,
    ) -> BoxFuture<'a, ()>;
}

/// A no-op normalizer for providers that don't need plan-time normalization.
#[derive(Debug, Clone, Copy)]
pub struct NoopNormalizer;
impl ProviderNormalizer for NoopNormalizer {
    fn normalize_desired<'a>(&'a self, _resources: &'a mut [ManagedResource]) -> BoxFuture<'a, ()> {
        ready_noop()
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
    ) -> BoxFuture<'a, ()> {
        ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
        _saved_attrs: &'a SavedAttrs,
    ) -> BoxFuture<'a, ()> {
        ready_noop()
    }

    fn merge_default_tags<'a>(
        &'a self,
        _resources: &'a mut [ManagedResource],
        _default_tags: &'a IndexMap<String, Value>,
        _registry: &'a SchemaRegistry,
    ) -> BoxFuture<'a, ()> {
        ready_noop()
    }
}

/// Shared implementation for merging default tags into resources.
///
/// For each resource matching `provider_name` whose schema includes a `tags` attribute:
/// - If the resource has no `tags`, set it to `default_tags`
/// - If the resource has `tags`, merge default_tags (resource-level tags win on conflict)
///
/// Records which tag keys came from defaults in the `_default_tag_keys` internal
/// metadata attribute.
pub fn merge_default_tags_for_provider(
    provider_name: &str,
    resources: &mut [ManagedResource],
    default_tags: &IndexMap<String, Value>,
    registry: &SchemaRegistry,
) {
    if default_tags.is_empty() {
        return;
    }

    for resource in resources.iter_mut() {
        if resource.id.provider != provider_name {
            continue;
        }

        // Check if the resource schema has a `tags` attribute
        let has_tags = registry
            .get_for(resource)
            .is_some_and(|s| s.attributes.contains_key("tags"));

        if !has_tags {
            continue;
        }

        // Merge default_tags into the resource's tags
        let mut default_tag_keys: Vec<String> = Vec::new();
        match resource.get_attr_mut("tags") {
            Some(Value::Concrete(ConcreteValue::Map(existing_tags))) => {
                for (key, value) in default_tags {
                    if !existing_tags.contains_key(key) {
                        existing_tags.insert(key.clone(), value.clone());
                        default_tag_keys.push(key.clone());
                    }
                }
            }
            None => {
                default_tag_keys = default_tags.keys().cloned().collect();
                resource.set_attr(
                    "tags".to_string(),
                    Value::Concrete(ConcreteValue::Map(default_tags.clone())),
                );
            }
            _ => {
                continue;
            }
        }

        if !default_tag_keys.is_empty() {
            default_tag_keys.sort();
            resource.set_attr(
                "_default_tag_keys".to_string(),
                Value::Concrete(ConcreteValue::List(
                    default_tag_keys
                        .into_iter()
                        .map(|s| Value::Concrete(ConcreteValue::String(s)))
                        .collect(),
                )),
            );
        }
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
    providers: HashMap<(String, Option<String>), Box<dyn Provider>>,
    normalizers: Vec<Box<dyn ProviderNormalizer>>,
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
        self.providers.insert((kind, None), provider);
    }

    /// Register a provider instance. `binding = None` registers the
    /// kind's default instance; `binding = Some(name)` registers a
    /// named instance.
    pub fn add_provider_instance(
        &mut self,
        kind: String,
        binding: Option<String>,
        provider: Box<dyn Provider>,
    ) {
        self.providers.insert((kind, binding), provider);
    }

    pub fn add_normalizer(&mut self, ext: Box<dyn ProviderNormalizer>) {
        self.normalizers.push(ext);
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    fn get_provider_or_error(&self, id: &ResourceId) -> ProviderResult<&dyn Provider> {
        let key = (id.provider.clone(), id.provider_instance.clone());
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

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        match self.get_provider_or_error(&resource.id) {
            Ok(provider) => provider.read_data_source(resource),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
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
    ) -> BoxFuture<'_, ProviderResult<State>> {
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
}

impl ProviderNormalizer for ProviderRouter {
    fn normalize_desired<'a>(&'a self, resources: &'a mut [ManagedResource]) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // Sequential, never concurrent: normalizers are not
            // commutative, and `resources` is re-borrowed per iteration
            // across the `.await`.
            for ext in &self.normalizers {
                ext.normalize_desired(resources).await;
            }
        })
    }

    fn normalize_state<'a>(
        &'a self,
        current_states: &'a mut HashMap<ResourceId, State>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for ext in &self.normalizers {
                ext.normalize_state(current_states).await;
            }
        })
    }

    fn hydrate_read_state<'a>(
        &'a self,
        current_states: &'a mut HashMap<ResourceId, State>,
        saved_attrs: &'a SavedAttrs,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for ext in &self.normalizers {
                ext.hydrate_read_state(current_states, saved_attrs).await;
            }
        })
    }

    fn merge_default_tags<'a>(
        &'a self,
        resources: &'a mut [ManagedResource],
        default_tags: &'a IndexMap<String, Value>,
        registry: &'a SchemaRegistry,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for ext in &self.normalizers {
                ext.merge_default_tags(resources, default_tags, registry)
                    .await;
            }
        })
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
    fn validate_config(&self, attributes: &IndexMap<String, Value>) -> Result<(), String>;

    /// Validate a value against a provider-defined custom type.
    /// Returns `Ok(())` if the value is valid or the type is unknown to this provider.
    /// Returns `Err(message)` if the value is invalid for the given type.
    ///
    /// `identity` is the structured [`TypeIdentity`] of the type, so a
    /// provider resolves the exact provider-scoped type instead of
    /// splitting a flat name string.
    fn validate_custom_type(&self, _identity: &TypeIdentity, _value: &str) -> Result<(), String> {
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
    /// verbatim — it is the user-facing error text.
    fn create_provider(
        &self,
        binding: Option<&str>,
        attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>>;

    /// Create a normalizer instance from configuration attributes.
    ///
    /// `binding` semantics match [`create_provider`]: `Some(name)` for
    /// a named instance, `None` for the kind's default. Returns a
    /// [`NoopNormalizer`] by default. Providers that need plan-time
    /// normalization or state hydration should override this.
    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
        Box::pin(async { Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer> })
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

    /// Maps a DSL alias value back to the canonical AWS value.
    ///
    /// For example, `("ec2.security_group_ingress", "ip_protocol", "all")` returns
    /// `Some("-1")` because `"all"` is a DSL alias for the AWS value `"-1"`.
    ///
    /// Returns `None` if no alias mapping exists (the value is already canonical).
    fn get_enum_alias_reverse(
        &self,
        _resource_type: &str,
        _attr_name: &str,
        _value: &str,
    ) -> Option<String> {
        None
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
    }

    validators
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
    }

    names.into_iter().collect()
}

/// Recursively extract Custom type validators from an AttributeType.
fn collect_validators_from_type(
    attr_type: &crate::schema::AttributeType,
    validators: &mut HashMap<TypeIdentity, crate::parser::ValidatorFn>,
) {
    use crate::schema::AttributeType;

    match attr_type {
        AttributeType::Custom {
            identity: Some(id),
            validate,
            ..
        } => {
            validators.entry(id.clone()).or_insert_with(|| {
                let validate_fn = validate.clone();
                Box::new(move |s: &str| {
                    validate_fn(&crate::resource::Value::Concrete(
                        crate::resource::ConcreteValue::String(s.to_string()),
                    ))
                    .map_err(|e| e.to_string())
                })
            });
        }
        AttributeType::Custom { identity: None, .. } => {}
        AttributeType::List { inner, .. } => {
            collect_validators_from_type(inner, validators);
        }
        AttributeType::Map { key, value: inner } => {
            collect_validators_from_type(key, validators);
            collect_validators_from_type(inner, validators);
        }
        AttributeType::Struct { fields, .. } => {
            for field in fields {
                collect_validators_from_type(&field.field_type, validators);
            }
        }
        AttributeType::Union(types) => {
            for t in types {
                collect_validators_from_type(t, validators);
            }
        }
        _ => {}
    }
}

/// Recursively collect Custom type identities from an AttributeType
/// (identities only, no closures).
fn collect_type_names_from_type(
    attr_type: &crate::schema::AttributeType,
    names: &mut std::collections::HashSet<TypeIdentity>,
) {
    use crate::schema::AttributeType;

    match attr_type {
        AttributeType::Custom {
            identity: Some(id), ..
        } => {
            names.insert(id.clone());
        }
        AttributeType::Custom { identity: None, .. } => {}
        AttributeType::List { inner, .. } => {
            collect_type_names_from_type(inner, names);
        }
        AttributeType::Map { key, value: inner } => {
            collect_type_names_from_type(key, names);
            collect_type_names_from_type(inner, names);
        }
        AttributeType::Struct { fields, .. } => {
            for field in fields {
                collect_type_names_from_type(&field.field_type, names);
            }
        }
        AttributeType::Union(types) => {
            for t in types {
                collect_type_names_from_type(t, names);
            }
        }
        _ => {}
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

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).read_data_source(resource)
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        (**self).create(id, request)
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(
            &self,
            id: &ResourceId,
            request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            let attrs = request.resource.attributes.clone();
            Box::pin(async move {
                Ok(
                    State::existing(id, crate::resource::attrs_to_hashmap(&attrs))
                        .with_identifier("mock-id-123"),
                )
            })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            // Apply the patch on top of `from` so the test sees the
            // user-specified changes round-tripped into State.
            let mut attrs = request.from.attributes.clone();
            for op in request.patch.ops {
                match op.kind {
                    PatchOpKind::Add | PatchOpKind::Replace => {
                        if let Some(v) = op.value {
                            attrs.insert(op.key, v);
                        }
                    }
                    PatchOpKind::Remove => {
                        attrs.remove(&op.key);
                    }
                }
            }
            Box::pin(async move { Ok(State::existing(id, attrs)) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn mock_provider_read_returns_not_found() {
        let provider = MockProvider;
        let id = ResourceId::new("test", "example");
        let state = provider.read(&id, None, ReadRequest).await.unwrap();
        assert!(!state.exists);
    }

    /// A provider that only accepts a data source when the full `ManagedResource`
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
            // the full ManagedResource — the regular `read` route has no access
            // to input attributes.
            Box::pin(async move {
                Err(ProviderError::internal("read cannot see inputs").for_resource(id))
            })
        }

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
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
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
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
    }

    #[tokio::test]
    async fn read_data_source_delegates_to_read_for_zero_input() {
        // MockProvider's read_data_source delegates to read(&resource.id, None).
        // For MockProvider this returns not_found.
        let provider = MockProvider;
        let resource = DataSource::new("test", "example");
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
            Value::Concrete(ConcreteValue::String("d-9567916d09".to_string())),
        );
        resource.set_attr(
            "user_name".to_string(),
            Value::Concrete(ConcreteValue::String("gosukenator@gmail.com".to_string())),
        );

        let state = provider.read_data_source(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(
            state.attributes.get("identity_store_id"),
            Some(&Value::Concrete(ConcreteValue::String(
                "d-9567916d09".to_string()
            )))
        );
        assert_eq!(
            state.attributes.get("user_name"),
            Some(&Value::Concrete(ConcreteValue::String(
                "gosukenator@gmail.com".to_string()
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
            Value::Concrete(ConcreteValue::String("x".to_string())),
        );
        let state = provider.read_data_source(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(
            state.attributes.get("user_name"),
            Some(&Value::Concrete(ConcreteValue::String("x".to_string())))
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
            Value::Concrete(ConcreteValue::String("x".to_string())),
        );
        let state = router.read_data_source(&resource).await.unwrap();
        assert!(state.exists);
        assert_eq!(
            state.attributes.get("user_name"),
            Some(&Value::Concrete(ConcreteValue::String("x".to_string())))
        );
    }

    #[tokio::test]
    async fn mock_provider_create_returns_existing() {
        let provider = MockProvider;
        let resource = ManagedResource::new("test", "example");
        let id = resource.id.clone();
        let state = provider
            .create(&id, CreateRequest { resource })
            .await
            .unwrap();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("mock-id-123".to_string()));
    }

    #[tokio::test]
    async fn provider_router_dispatches_read_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider("mock", "test", "example", None);
        let state = router.read(&id, None, ReadRequest).await.unwrap();
        assert!(!state.exists);
    }

    #[tokio::test]
    async fn provider_router_dispatches_create_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let resource = ManagedResource::with_provider("mock", "test", "example", None);
        let id = resource.id.clone();
        let state = router
            .create(&id, CreateRequest { resource })
            .await
            .unwrap();
        assert!(state.exists);
        assert_eq!(state.identifier, Some("mock-id-123".to_string()));
    }

    #[test]
    fn provider_error_source_returns_cause() {
        use std::error::Error;
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::api_error("Failed to create resource").with_cause(cause);
        let source = err.source().expect("source should be Some");
        assert_eq!(source.to_string(), "connection refused");
    }

    #[test]
    fn provider_error_display_includes_cause() {
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::api_error("Failed to create resource").with_cause(cause);
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

    /// carina-rs/carina#2603: AWS SDK errors typically carry the
    /// actionable detail (error code, message, request id) wrapped
    /// two or three levels deep behind `source()`. A `Display` impl
    /// that prints only the first level hides the part the user
    /// actually needs. Walk the whole chain.
    #[test]
    fn provider_error_display_walks_entire_source_chain() {
        #[derive(Debug)]
        struct ChainErr {
            msg: &'static str,
            source: Option<Box<dyn std::error::Error + Send + Sync>>,
        }
        impl std::fmt::Display for ChainErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.msg)
            }
        }
        impl std::error::Error for ChainErr {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.source
                    .as_deref()
                    .map(|e| e as &(dyn std::error::Error + 'static))
            }
        }

        let inner = ChainErr {
            msg: "AccessDenied: User is not authorized to perform: s3:HeadBucket",
            source: None,
        };
        let outer = ChainErr {
            msg: "service error",
            source: Some(Box::new(inner)),
        };
        let err = ProviderError::api_error("AWS error").with_cause(outer);

        let rendered = format!("{}", err);
        assert!(
            rendered.contains("service error"),
            "outer cause must surface, got: {}",
            rendered
        );
        assert!(
            rendered.contains("AccessDenied"),
            "deeper source-chain text (the part the user actually needs) \
             must surface — single-level Display is the carina#2603 bug. \
             Got: {}",
            rendered
        );
    }

    #[test]
    fn provider_error_display_with_resource_id_and_cause() {
        let cause = std::io::Error::other("timeout");
        let id = ResourceId::new("s3.Bucket", "my-bucket");
        let err = ProviderError::api_error("Failed to read")
            .with_cause(cause)
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
        let id = ResourceId::new("iam.Roles", "admin_access_roles");
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
    /// diagnostic in the cause chain — there was no HTTP response, so
    /// `status`/`code`/`request_id` are `None` but `operation` may
    /// still be set. Without preserving cause, the operator loses
    /// visibility into the underlying error. The labeled-line shape
    /// keeps the new multi-line aesthetic while still surfacing the
    /// diagnostic.
    #[test]
    fn provider_error_display_multi_line_appends_cause_as_labeled_line() {
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::api_error("Failed to list IAM roles")
            .with_cause(cause)
            .with_operation("iam.ListRoles");
        let rendered = format!("{}", err);
        let lines: Vec<&str> = rendered.lines().collect();

        // Header line stays clean.
        assert_eq!(
            lines.first().copied(),
            Some("Failed to list IAM roles"),
            "header line must not be extended by the chain-walk, got: {}",
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
        let id = ResourceId::new("iam.Roles", "admin_access_roles");
        let err = ProviderError::api_error("Failed to list IAM roles")
            .with_cause(cause)
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

    /// carina#3242: when the cause itself carries a source chain, the
    /// `cause:` line joins the levels with `: ` exactly like the
    /// legacy fallback (carina#2603) — same shape, same content,
    /// just on a labeled line instead of appended to the header.
    #[test]
    fn provider_error_display_multi_line_cause_walks_source_chain() {
        #[derive(Debug)]
        struct ChainErr {
            msg: &'static str,
            source: Option<Box<dyn std::error::Error + Send + Sync>>,
        }
        impl std::fmt::Display for ChainErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.msg)
            }
        }
        impl std::error::Error for ChainErr {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.source
                    .as_deref()
                    .map(|e| e as &(dyn std::error::Error + 'static))
            }
        }

        let inner = ChainErr {
            msg: "dns lookup failed",
            source: None,
        };
        let outer = ChainErr {
            msg: "transport error",
            source: Some(Box::new(inner)),
        };
        let err = ProviderError::api_error("Failed")
            .with_cause(outer)
            .with_operation("iam.ListRoles");
        let rendered = format!("{}", err);
        assert!(
            rendered.contains("\n  cause: transport error: dns lookup failed"),
            "cause line must walk the full source chain, got: {}",
            rendered
        );
    }

    /// carina#3242: when **none** of the structured fields are
    /// populated, `Display` must fall back to the existing
    /// chain-walking single-line form. This is the path that
    /// unmigrated provider call sites still hit during the
    /// migration period.
    #[test]
    fn provider_error_display_falls_back_to_chain_walk_when_unstructured() {
        let cause = std::io::Error::other("connection refused");
        let err = ProviderError::api_error("Failed to create resource").with_cause(cause);
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
        assert_eq!(inv.message(), "bad");

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
        let id = ResourceId::new("test", "example");
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

        let mut to = ManagedResource::new("test", "example");
        to.set_attr(
            "a".to_string(),
            Value::Concrete(ConcreteValue::String("new".into())),
        );
        to.set_attr(
            "b".to_string(),
            Value::Concrete(ConcreteValue::String("added".into())),
        );

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

        let id = ResourceId::with_provider("mock", "test", "example", None);
        let from = State::existing(id.clone(), HashMap::new());
        let request = UpdateRequest {
            from,
            patch: UpdatePatch::default(),
        };
        let state = router.update(&id, "mock-id-123", request).await.unwrap();
        assert!(state.exists);
    }

    #[tokio::test]
    async fn provider_router_dispatches_delete_by_provider_name() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider("mock", "test", "example", None);
        let request = DeleteRequest::default();
        let result = router.delete(&id, "mock-id-123", request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn provider_router_returns_error_for_unknown_provider() {
        let router = ProviderRouter::new();
        let id = ResourceId::with_provider("nonexistent", "test", "example", None);
        let result = router.read(&id, None, ReadRequest).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ProviderError::Internal(_)));
        assert!(err.message().contains("Unknown provider: nonexistent"));
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
                Value::Concrete(ConcreteValue::String(self.tag.to_string())),
            );
            let state = State::existing(id.clone(), attrs);
            Box::pin(async move { Ok(state) })
        }
        fn read_data_source(&self, _resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }
        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("not supported")) })
        }
        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
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
    }

    #[tokio::test]
    async fn provider_router_dispatches_to_named_instance() {
        // Two instances of the same kind ("mock") routed by binding.
        let mut router = ProviderRouter::new();
        router.add_provider_instance(
            "mock".to_string(),
            None,
            Box::new(TaggedProvider { tag: "default" }),
        );
        router.add_provider_instance(
            "mock".to_string(),
            Some("us".to_string()),
            Box::new(TaggedProvider { tag: "us" }),
        );

        let default_id = ResourceId::with_provider("mock", "test", "a", None);
        let state = router.read(&default_id, None, ReadRequest).await.unwrap();
        assert_eq!(
            state.attributes.get("tag"),
            Some(&Value::Concrete(ConcreteValue::String(
                "default".to_string()
            ))),
            "resources without provider_instance must route to the kind's default instance"
        );

        let us_id = ResourceId::with_provider("mock", "test", "b", Some("us".to_string()));
        let state = router.read(&us_id, None, ReadRequest).await.unwrap();
        assert_eq!(
            state.attributes.get("tag"),
            Some(&Value::Concrete(ConcreteValue::String("us".to_string()))),
            "resources tagged with binding=Some('us') must route to that named instance"
        );
    }

    #[tokio::test]
    async fn provider_router_unknown_named_instance_errors_with_binding() {
        let mut router = ProviderRouter::new();
        router.add_provider("mock".to_string(), Box::new(MockProvider));

        let id = ResourceId::with_provider("mock", "test", "x", Some("missing".to_string()));
        let err = router.read(&id, None, ReadRequest).await.unwrap_err();
        let msg = err.message();
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
            fn validate_config(&self, _attrs: &IndexMap<String, Value>) -> Result<(), String> {
                Ok(())
            }
            fn extract_region(&self, _attrs: &IndexMap<String, Value>) -> String {
                "us-east-1".to_string()
            }
            fn create_provider(
                &self,
                _binding: Option<&str>,
                _attrs: &IndexMap<String, Value>,
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
        let result = factory.create_provider(None, &IndexMap::new()).await;
        let err = match result {
            Ok(_) => panic!("create_provider must surface the init error"),
            Err(e) => e,
        };
        // Inner message must be preserved verbatim; no implementation-detail wrapper.
        let msg = err.message().to_string();
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
            fn validate_config(&self, _attrs: &IndexMap<String, Value>) -> Result<(), String> {
                Ok(())
            }
            fn extract_region(&self, _attrs: &IndexMap<String, Value>) -> String {
                "ap-northeast-1".to_string()
            }
            fn create_provider(
                &self,
                binding: Option<&str>,
                _attrs: &IndexMap<String, Value>,
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
        let _ = factory
            .create_provider(None, &IndexMap::new())
            .await
            .unwrap();
        let _ = factory
            .create_provider(Some("us"), &IndexMap::new())
            .await
            .unwrap();
        let _ = factory
            .create_provider(Some("tokyo"), &IndexMap::new())
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
                resources: &'a mut [ManagedResource],
            ) -> BoxFuture<'a, ()> {
                Box::pin(async move {
                    // Prefix all string attribute values with "normalized:"
                    for resource in resources.iter_mut() {
                        for value in resource.attributes.values_mut() {
                            if let Value::Concrete(ConcreteValue::String(s)) = value {
                                *s = format!("normalized:{}", s);
                            }
                        }
                    }
                })
            }

            fn normalize_state<'a>(
                &'a self,
                _current_states: &'a mut HashMap<ResourceId, State>,
            ) -> BoxFuture<'a, ()> {
                ready_noop()
            }

            fn hydrate_read_state<'a>(
                &'a self,
                states: &'a mut HashMap<ResourceId, State>,
                saved: &'a SavedAttrs,
            ) -> BoxFuture<'a, ()> {
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
                })
            }

            fn merge_default_tags<'a>(
                &'a self,
                _resources: &'a mut [ManagedResource],
                _default_tags: &'a IndexMap<String, Value>,
                _registry: &'a SchemaRegistry,
            ) -> BoxFuture<'a, ()> {
                ready_noop()
            }
        }

        // Test normalize_desired
        let ext = SchemaOnlyProvider;
        let mut resources = vec![ManagedResource::new("test", "example").with_attribute(
            "key",
            Value::Concrete(ConcreteValue::String("value".to_string())),
        )];
        ext.normalize_desired(&mut resources).await;
        assert_eq!(
            resources[0].get_attr("key"),
            Some(&Value::Concrete(ConcreteValue::String(
                "normalized:value".to_string()
            )))
        );

        // Test hydrate_read_state
        let id = ResourceId::new("test", "example");
        let mut states = HashMap::new();
        states.insert(id.clone(), State::existing(id.clone(), HashMap::new()));
        let mut saved: SavedAttrs = HashMap::new();
        saved.insert(
            id.clone(),
            HashMap::from([(
                "restored".to_string(),
                Value::Concrete(ConcreteValue::String("data".to_string())),
            )]),
        );
        ext.hydrate_read_state(&mut states, &saved).await;
        assert_eq!(
            states.get(&id).unwrap().attributes.get("restored"),
            Some(&Value::Concrete(ConcreteValue::String("data".to_string())))
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
                resource: &DataSource,
            ) -> BoxFuture<'_, ProviderResult<State>> {
                let id = resource.id.clone();
                Box::pin(async move { Ok(State::not_found(id)) })
            }

            fn create(
                &self,
                id: &ResourceId,
                _request: CreateRequest,
            ) -> BoxFuture<'_, ProviderResult<State>> {
                let id = id.clone();
                Box::pin(async move { Ok(State::not_found(id)) })
            }

            fn update(
                &self,
                id: &ResourceId,
                _identifier: &str,
                _request: UpdateRequest,
            ) -> BoxFuture<'_, ProviderResult<State>> {
                let id = id.clone();
                Box::pin(async move { Ok(State::not_found(id)) })
            }

            fn delete(
                &self,
                _id: &ResourceId,
                _identifier: &str,
                _request: DeleteRequest,
            ) -> BoxFuture<'_, ProviderResult<()>> {
                Box::pin(async { Ok(()) })
            }
        }

        // Separate schema ext struct for the router
        struct TestNormalizer;
        impl ProviderNormalizer for TestNormalizer {
            fn normalize_desired<'a>(
                &'a self,
                resources: &'a mut [ManagedResource],
            ) -> BoxFuture<'a, ()> {
                Box::pin(async move {
                    for resource in resources.iter_mut() {
                        if resource.id.provider == "normalizing" {
                            for value in resource.attributes.values_mut() {
                                if let Value::Concrete(ConcreteValue::String(s)) = value {
                                    *s = format!("norm:{}", s);
                                }
                            }
                        }
                    }
                })
            }

            fn normalize_state<'a>(
                &'a self,
                _current_states: &'a mut HashMap<ResourceId, State>,
            ) -> BoxFuture<'a, ()> {
                ready_noop()
            }

            fn hydrate_read_state<'a>(
                &'a self,
                _current_states: &'a mut HashMap<ResourceId, State>,
                _saved_attrs: &'a SavedAttrs,
            ) -> BoxFuture<'a, ()> {
                ready_noop()
            }

            fn merge_default_tags<'a>(
                &'a self,
                _resources: &'a mut [ManagedResource],
                _default_tags: &'a IndexMap<String, Value>,
                _registry: &'a SchemaRegistry,
            ) -> BoxFuture<'a, ()> {
                ready_noop()
            }
        }

        let mut router = ProviderRouter::new();
        router.add_provider("normalizing".to_string(), Box::new(NormalizingProvider));
        router.add_normalizer(Box::new(TestNormalizer));

        let mut resources = vec![
            ManagedResource::with_provider("normalizing", "test", "example", None).with_attribute(
                "key",
                Value::Concrete(ConcreteValue::String("val".to_string())),
            ),
        ];
        router.normalize_desired(&mut resources).await;
        assert_eq!(
            resources[0].get_attr("key"),
            Some(&Value::Concrete(ConcreteValue::String(
                "norm:val".to_string()
            )))
        );
    }
}
