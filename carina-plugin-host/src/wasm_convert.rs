//! Conversions between carina-core types and Wasmtime-generated WIT types.

use std::collections::HashMap;
use std::fmt;

use carina_core::effect::PlanOp as CorePlanOp;
use carina_core::provider::{
    CreateRequest as CoreCreateRequest, DeleteRequest as CoreDeleteRequest,
    ErrorDetail as CoreErrorDetail, PatchOp as CorePatchOp, PatchOpKind as CorePatchOpKind,
    ProviderError as CoreProviderError, ReadRequest as CoreReadRequest,
    UpdatePatch as CoreUpdatePatch, UpdateRequest as CoreUpdateRequest,
};
use carina_core::resource::{
    ConcreteValue, DataSource as CoreDataSource, DeferredValue, Directives,
    Resource as CoreResource, ResourceId as CoreResourceId, State as CoreState, Value as CoreValue,
};
use carina_core::schema::{
    AttributeSchema as CoreAttributeSchema, AttributeType as CoreAttributeType,
    ResourceSchema as CoreResourceSchema, StructField as CoreStructField, legacy_validator,
};
use carina_core::value::{SerializationContext, SerializationError};
use carina_core::wait::BindingPattern as CoreBindingPattern;
use carina_core::wait::predicate::{AttrPath as CoreAttrPath, AttrPathError};

use carina_provider_protocol::types as proto;

use crate::wasm_bindings::carina::provider::types as wit;
use crate::wasm_bindings::exports::carina::provider::provider as wit_provider;

/// Error raised when provider-emitted schema wire data cannot be decoded.
///
/// External provider metadata is version-gated but still untrusted at this
/// boundary; carina#3459 makes unsupported wire shapes explicit errors instead
/// of panics so callers can attach provider context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDecodeError {
    detail: String,
}

impl SchemaDecodeError {
    fn unsupported_custom_base(enclosing_custom_name: &str, base: &proto::AttributeType) -> Self {
        Self {
            detail: format!(
                "legacy Custom wire type '{enclosing_custom_name}' has unsupported base {base:?}"
            ),
        }
    }
}

impl fmt::Display for SchemaDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for SchemaDecodeError {}

// -- Value --

/// Convert a core Value to a WIT Value.
///
/// List and Map values are serialized to JSON strings because WIT does
/// not support recursive types.
///
/// Returns `Err(SerializationError)` for `Value::Deferred(DeferredValue::Unknown)`,
/// `Value::Deferred(DeferredValue::ResourceRef)`, `Value::Deferred(DeferredValue::Interpolation)`, and
/// `Value::Deferred(DeferredValue::FunctionCall)`. The plan-time pipeline strips every attribute
/// that recursively contains one of these via
/// `PlanPreprocessor::prepare`'s strip-and-restore pass, so reaching
/// any of these arms is a producer-side bug — the diagnostic identifies
/// which variant leaked. See RFC #2371 stage 2 (#2378) for `Unknown`
/// and #2387 for `ResourceRef` / `Interpolation` / `FunctionCall`.
///
/// `Value::Deferred(DeferredValue::Secret(inner))` is encoded as the `secret-val` WIT variant
/// (#2390): the inner is JSON-encoded the same way `list-val` / `map-val`
/// encode their contents, and a typed signal crosses the boundary so the
/// WASM provider can distinguish a secret from a plain string. The
/// previous `format!("{v:?}")` debug-format fallback would send
/// `"Secret(String(\"…\"))"` literally to the provider — a contract leak
/// equivalent to the pre-#2387 `ResourceRef` debug-string.
pub fn core_to_wit_value(v: &CoreValue) -> Result<wit::Value, SerializationError> {
    match v {
        CoreValue::Concrete(ConcreteValue::String(s)) => {
            // Enum identifiers lower to the WIT boundary as plain
            // strings — the WASM plugin sees the same wire shape as
            // any other string value. See carina#2986.
            Ok(wit::Value::StrVal(s.clone()))
        }
        CoreValue::Concrete(ConcreteValue::EnumIdentifier(s)) => {
            Ok(wit::Value::StrVal(s.to_string()))
        }
        CoreValue::Concrete(ConcreteValue::CanonicalEnum(c)) => {
            Ok(wit::Value::StrVal(c.api_value().to_string()))
        }
        CoreValue::Concrete(ConcreteValue::Int(i)) => Ok(wit::Value::IntVal(*i)),
        CoreValue::Concrete(ConcreteValue::Float(f)) => Ok(wit::Value::FloatVal(*f)),
        CoreValue::Concrete(ConcreteValue::Bool(b)) => Ok(wit::Value::BoolVal(*b)),
        // Duration crosses the WIT boundary as integer seconds — see
        // `notes/specs/2026-05-10-duration-design.md` for the rationale
        // (no `duration-val` variant; existing `aws`/`awscc` plugins
        // need no rebuild). The inbound `wit_to_core_value` path is
        // intentionally one-way for now: every `IntVal` reads back as
        // `Value::Concrete(ConcreteValue::Int)`, regardless of whether the destination schema
        // attribute is typed `Duration`. Re-typing on the inbound side
        // (so a Duration-typed schema attribute reconstructs as
        // `Value::Concrete(ConcreteValue::Duration)`) is a deferred follow-up — without it, a
        // post-apply state diff for a Duration attribute will surface
        // as `Duration(75min) → Int(4500)` until the schema-aware
        // re-typing lands. For MVP every consumer of Duration is
        // host-side (`wait { timeout = ... }`), so the asymmetry is
        // contained to the not-yet-existent provider-side use case.
        CoreValue::Concrete(ConcreteValue::Duration(d)) => {
            Ok(wit::Value::IntVal(d.as_secs() as i64))
        }
        CoreValue::Concrete(ConcreteValue::List(items)) => {
            let json_items: Result<Vec<serde_json::Value>, _> =
                items.iter().map(core_value_to_json).collect();
            let json_str = serde_json::to_string(&json_items?)
                .expect("serde_json::Value -> String is infallible");
            Ok(wit::Value::ListVal(json_str))
        }
        CoreValue::Concrete(ConcreteValue::StringList(items)) => {
            // Cross the WASM boundary as a plain JSON array of strings —
            // the provider sees the same shape as `Value::Concrete(ConcreteValue::List([String]))`,
            // matching AWS's wire-format expectation for the
            // `string_or_list_of_strings` IAM shape.
            let json_arr: Vec<serde_json::Value> = items
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect();
            let json_str = serde_json::to_string(&json_arr)
                .expect("serde_json::Value -> String is infallible");
            Ok(wit::Value::ListVal(json_str))
        }
        CoreValue::Concrete(ConcreteValue::Map(map)) => {
            let json_map: Result<serde_json::Map<String, serde_json::Value>, _> = map
                .iter()
                .map(|(k, v)| core_value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect();
            let json_str = serde_json::to_string(&json_map?)
                .expect("serde_json::Value -> String is infallible");
            Ok(wit::Value::MapVal(json_str))
        }
        CoreValue::Deferred(DeferredValue::Unknown(reason)) => {
            Err(SerializationError::UnknownNotAllowed {
                reason: reason.clone(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::ResourceRef { path }) => {
            Err(SerializationError::UnresolvedResourceRef {
                path: path.to_dot_string(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::BindingRef { binding }) => {
            Err(SerializationError::UnresolvedResourceRef {
                path: binding.clone(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::Interpolation(_)) => {
            Err(SerializationError::UnresolvedInterpolation {
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::FunctionCall { name, .. }) => {
            Err(SerializationError::UnresolvedFunctionCall {
                name: name.clone(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::Secret(inner)) => {
            let json = core_value_to_json(inner)?;
            let json_str =
                serde_json::to_string(&json).expect("serde_json::Value -> String is infallible");
            Ok(wit::Value::SecretVal(json_str))
        }
    }
}

/// Convert a WIT Value to a core Value.
pub fn wit_to_core_value(v: &wit::Value) -> CoreValue {
    match v {
        wit::Value::StrVal(s) => CoreValue::Concrete(ConcreteValue::String(s.clone())),
        wit::Value::IntVal(i) => CoreValue::Concrete(ConcreteValue::Int(*i)),
        wit::Value::FloatVal(f) => CoreValue::Concrete(ConcreteValue::Float(*f)),
        wit::Value::BoolVal(b) => CoreValue::Concrete(ConcreteValue::Bool(*b)),
        wit::Value::ListVal(json) => {
            let items: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
            CoreValue::Concrete(ConcreteValue::List(
                items.iter().map(json_to_core_value).collect(),
            ))
        }
        wit::Value::MapVal(json) => {
            let map: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(json).unwrap_or_default();
            CoreValue::Concrete(ConcreteValue::Map(
                map.iter()
                    .map(|(k, v)| (k.clone(), json_to_core_value(v)))
                    .collect(),
            ))
        }
        wit::Value::SecretVal(json) => {
            // Decode the JSON-encoded inner value the same way `ListVal` /
            // `MapVal` decode theirs, then re-wrap in `Value::Deferred(DeferredValue::Secret)` so the
            // host's secret-tracking machinery (state hashing, plan
            // redaction) keeps working. A malformed encoding falls back to
            // an empty string secret rather than panicking — the WASM
            // provider produced this, so we treat it as untrusted input.
            let inner_json: serde_json::Value =
                serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
            let inner = json_to_core_value(&inner_json);
            CoreValue::Deferred(DeferredValue::Secret(Box::new(inner)))
        }
    }
}

pub fn core_to_wit_plan_op(op: CorePlanOp) -> wit_provider::PlanOp {
    match op {
        CorePlanOp::Create => wit_provider::PlanOp::Create,
        CorePlanOp::Read => wit_provider::PlanOp::Read,
        CorePlanOp::Update => wit_provider::PlanOp::Update,
        CorePlanOp::Delete => wit_provider::PlanOp::Delete,
    }
}

pub fn core_to_wit_binding_pattern(p: &CoreBindingPattern) -> wit::BindingPattern {
    match p {
        CoreBindingPattern::Exact(name) => wit::BindingPattern::Exact(name.clone()),
        CoreBindingPattern::ForLoopChildren { base } => {
            wit::BindingPattern::ForLoopChildren(base.clone())
        }
        CoreBindingPattern::AttributeMatch {
            resource_type,
            attr,
            from,
        } => wit::BindingPattern::AttributeMatch(wit::AttributeMatchPattern {
            resource_type: resource_type.clone(),
            attr: attr.segments().to_vec(),
            from: from.segments().to_vec(),
        }),
    }
}

pub fn wit_to_core_binding_pattern(
    p: wit::BindingPattern,
) -> Result<CoreBindingPattern, AttrPathError> {
    let pattern = match p {
        wit::BindingPattern::Exact(name) => CoreBindingPattern::Exact(name),
        wit::BindingPattern::ForLoopChildren(base) => CoreBindingPattern::ForLoopChildren { base },
        wit::BindingPattern::AttributeMatch(pattern) => CoreBindingPattern::AttributeMatch {
            resource_type: pattern.resource_type,
            attr: CoreAttrPath::try_new(pattern.attr)?,
            from: CoreAttrPath::try_new(pattern.from)?,
        },
    };
    Ok(pattern)
}

/// Helper: convert a core Value to a serde_json::Value for JSON
/// encoding inside the WIT-string fallback for List/Map. Returns
/// `Err` for the same set of variants as `core_to_wit_value` — see
/// that function's doc for the rationale and the strip-and-restore
/// pass that keeps these arms unreachable in legitimate flows.
fn core_value_to_json(v: &CoreValue) -> Result<serde_json::Value, SerializationError> {
    match v {
        CoreValue::Concrete(ConcreteValue::String(s)) => Ok(serde_json::Value::String(s.clone())),
        CoreValue::Concrete(ConcreteValue::EnumIdentifier(s)) => {
            Ok(serde_json::Value::String(s.to_string()))
        }
        CoreValue::Concrete(ConcreteValue::CanonicalEnum(c)) => {
            Ok(serde_json::Value::String(c.api_value().to_string()))
        }
        CoreValue::Concrete(ConcreteValue::Int(i)) => Ok(serde_json::Value::Number((*i).into())),
        CoreValue::Concrete(ConcreteValue::Float(f)) => Ok(serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)),
        CoreValue::Concrete(ConcreteValue::Bool(b)) => Ok(serde_json::Value::Bool(*b)),
        CoreValue::Concrete(ConcreteValue::Duration(d)) => {
            Ok(serde_json::Value::Number((d.as_secs() as i64).into()))
        }
        CoreValue::Concrete(ConcreteValue::List(items)) => {
            let arr: Result<Vec<_>, _> = items.iter().map(core_value_to_json).collect();
            Ok(serde_json::Value::Array(arr?))
        }
        CoreValue::Concrete(ConcreteValue::StringList(items)) => Ok(serde_json::Value::Array(
            items
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        )),
        CoreValue::Concrete(ConcreteValue::Map(map)) => {
            let obj: Result<serde_json::Map<String, serde_json::Value>, _> = map
                .iter()
                .map(|(k, v)| core_value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect();
            Ok(serde_json::Value::Object(obj?))
        }
        CoreValue::Deferred(DeferredValue::Unknown(reason)) => {
            Err(SerializationError::UnknownNotAllowed {
                reason: reason.clone(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::ResourceRef { path }) => {
            Err(SerializationError::UnresolvedResourceRef {
                path: path.to_dot_string(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::BindingRef { binding }) => {
            Err(SerializationError::UnresolvedResourceRef {
                path: binding.clone(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::Interpolation(_)) => {
            Err(SerializationError::UnresolvedInterpolation {
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::FunctionCall { name, .. }) => {
            Err(SerializationError::UnresolvedFunctionCall {
                name: name.clone(),
                context: SerializationContext::WasmBoundary,
            })
        }
        // Within this helper a `Secret` would only appear if the caller
        // wrapped one inside a `List` / `Map` payload going to a WIT
        // `list-val` / `map-val`. Render it as the JSON-encoded inner so
        // the byte stream the provider receives is identical to what
        // `core_to_wit_value`'s `secret-val` arm produces. Provider plugins
        // that decode `list-val` / `map-val` see the inner JSON shape;
        // `secret-val` is the channel used to mark the *attribute* itself
        // as a secret.
        CoreValue::Deferred(DeferredValue::Secret(inner)) => core_value_to_json(inner),
    }
}

/// Helper: convert a serde_json::Value to a core Value.
fn json_to_core_value(v: &serde_json::Value) -> CoreValue {
    match v {
        serde_json::Value::String(s) => CoreValue::Concrete(ConcreteValue::String(s.clone())),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CoreValue::Concrete(ConcreteValue::Int(i))
            } else if let Some(f) = n.as_f64() {
                CoreValue::Concrete(ConcreteValue::Float(f))
            } else {
                CoreValue::Concrete(ConcreteValue::String(n.to_string()))
            }
        }
        serde_json::Value::Bool(b) => CoreValue::Concrete(ConcreteValue::Bool(*b)),
        serde_json::Value::Array(items) => CoreValue::Concrete(ConcreteValue::List(
            items.iter().map(json_to_core_value).collect(),
        )),
        serde_json::Value::Object(map) => CoreValue::Concrete(ConcreteValue::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_core_value(v)))
                .collect(),
        )),
        serde_json::Value::Null => CoreValue::Concrete(ConcreteValue::String(String::new())),
    }
}

// -- Value map helpers --

pub fn core_to_wit_value_map<'a, M>(map: M) -> Result<Vec<(String, wit::Value)>, SerializationError>
where
    M: IntoIterator<Item = (&'a String, &'a CoreValue)>,
{
    map.into_iter()
        .map(|(k, v)| core_to_wit_value(v).map(|wv| (k.clone(), wv)))
        .collect()
}

pub fn wit_to_core_value_map(entries: &[(String, wit::Value)]) -> HashMap<String, CoreValue> {
    entries
        .iter()
        .map(|(k, v)| (k.clone(), wit_to_core_value(v)))
        .collect()
}

// -- ResourceId --

pub fn core_to_wit_resource_id(id: &CoreResourceId) -> wit::ResourceId {
    wit::ResourceId {
        provider: id.provider.clone(),
        resource_type: id.resource_type.clone(),
        name: id.name_str().to_string(),
    }
}

pub fn wit_to_core_resource_id(id: &wit::ResourceId) -> CoreResourceId {
    // The WIT `resource-id` record has no `provider-instance` field
    // yet, so this boundary cannot round-trip a named instance.
    // Tracked as a follow-up to extend the WIT contract; until then,
    // callers that need routing must thread it through alongside the
    // converted id.
    CoreResourceId::with_provider(&id.provider, &id.resource_type, &id.name, None)
}

// -- State --

pub fn core_to_wit_state(state: &CoreState) -> Result<wit::State, SerializationError> {
    Ok(wit::State {
        identifier: state.identifier.clone(),
        attributes: core_to_wit_value_map(&state.attributes)?,
        exists: state.exists,
    })
}

pub fn wit_to_core_state(state: &wit::State, id: &CoreResourceId) -> CoreState {
    if !state.exists {
        return CoreState::not_found(id.clone());
    }
    let attributes = wit_to_core_value_map(&state.attributes);
    let mut core_state = CoreState::existing(id.clone(), attributes);
    if let Some(ref ident) = state.identifier {
        core_state = core_state.with_identifier(ident);
    }
    core_state
}

// -- Resource --

pub fn core_to_wit_resource(
    resource: &CoreResource,
) -> Result<wit::ResourceDef, SerializationError> {
    Ok(wit::ResourceDef {
        id: core_to_wit_resource_id(&resource.id),
        attributes: core_to_wit_value_map(&resource.resolved_attributes())?,
    })
}

/// Convert a [`CoreDataSource`] to the WIT `ResourceDef` carried over the
/// plugin boundary. The WIT contract has a single `ResourceDef` record,
/// so a data source maps to the same `{ id, attributes }` shape as a
/// managed resource (carina#3181).
pub fn core_data_source_to_wit_resource(
    data_source: &CoreDataSource,
) -> Result<wit::ResourceDef, SerializationError> {
    Ok(wit::ResourceDef {
        id: core_to_wit_resource_id(&data_source.id),
        attributes: core_to_wit_value_map(&carina_core::resource::attrs_to_hashmap(
            &data_source.attributes,
        ))?,
    })
}

pub fn wit_to_core_resource(resource: &wit::ResourceDef) -> CoreResource {
    let id = wit_to_core_resource_id(&resource.id);
    // `id` came from `wit_to_core_resource_id`, which has no
    // `provider_instance` to forward (WIT contract limitation); pass
    // `None` explicitly to match.
    let mut core_resource =
        CoreResource::with_provider(&id.provider, &id.resource_type, id.name_str(), None);
    core_resource.attributes = resource
        .attributes
        .iter()
        .map(|(k, v)| (k.clone(), wit_to_core_value(v)))
        .collect();
    core_resource
}

// -- JSON passthrough functions for provider-specific types --

/// Serialize Directives to JSON string for the WIT boundary.
pub fn directives_to_json(directives: &Directives) -> String {
    serde_json::to_string(directives).unwrap_or_else(|_| "{}".to_string())
}

// -- ProviderError --

/// Convert a host-side core [`CoreProviderError`] into the WIT
/// `provider-error` variant. The boxed `cause` chain is flattened to a
/// string because WIT cannot represent `dyn std::error::Error`.
pub fn core_to_wit_provider_error(err: &CoreProviderError) -> wit::ProviderError {
    let detail = err.detail();
    let wit_detail = wit::ErrorDetail {
        message: detail.message.clone(),
        resource_id: detail
            .resource_id
            .as_ref()
            .map(|id| core_to_wit_resource_id(id)),
        cause: detail.cause.as_ref().map(|c| c.to_string()),
        provider_name: detail.provider_name.clone(),
        operation: detail.operation.clone(),
        status: detail.status,
        code: detail.code.clone(),
        request_id: detail.request_id.clone(),
    };
    match err {
        CoreProviderError::InvalidInput(_) => wit::ProviderError::InvalidInput(wit_detail),
        CoreProviderError::ApiError(_) => wit::ProviderError::ApiError(wit_detail),
        CoreProviderError::NotFound(_) => wit::ProviderError::NotFound(wit_detail),
        CoreProviderError::Timeout(_) => wit::ProviderError::Timeout(wit_detail),
        CoreProviderError::Internal(_) => wit::ProviderError::Internal(wit_detail),
    }
}

/// Convert a WIT [`wit::ProviderError`] into the host-side core
/// [`CoreProviderError`]. The variant is preserved exactly; the
/// `cause` string is rehydrated as an `Option<String>` inside
/// [`CoreErrorDetail`].
pub fn wit_to_core_provider_error(err: wit::ProviderError) -> CoreProviderError {
    let (detail, ctor): (
        wit::ErrorDetail,
        fn(Box<CoreErrorDetail>) -> CoreProviderError,
    ) = match err {
        wit::ProviderError::InvalidInput(d) => (d, CoreProviderError::InvalidInput),
        wit::ProviderError::ApiError(d) => (d, CoreProviderError::ApiError),
        wit::ProviderError::NotFound(d) => (d, CoreProviderError::NotFound),
        wit::ProviderError::Timeout(d) => (d, CoreProviderError::Timeout),
        wit::ProviderError::Internal(d) => (d, CoreProviderError::Internal),
    };
    let core_detail = CoreErrorDetail {
        message: detail.message,
        resource_id: detail
            .resource_id
            .map(|id| Box::new(wit_to_core_resource_id(&id))),
        cause: detail
            .cause
            .map(|s| Box::new(FlattenedCause(s)) as Box<dyn std::error::Error + Send + Sync>),
        provider_name: detail.provider_name,
        operation: detail.operation,
        status: detail.status,
        code: detail.code,
        request_id: detail.request_id,
    };
    ctor(Box::new(core_detail))
}

/// Synthetic `Error` wrapping a flattened-cause string from the WIT
/// boundary. We can't rehydrate a real cause chain across WIT
/// (`dyn Error` doesn't fit through a record), so we wrap the string
/// in a minimal `Error` shape so callers reading
/// `ProviderError::source()` still see a non-empty message.
#[derive(Debug)]
struct FlattenedCause(String);

impl std::fmt::Display for FlattenedCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FlattenedCause {}

// -- Update request and patch --

/// Build a [`wit::UpdateRequest`] from the host-side core
/// [`CoreUpdateRequest`]. Patch op order is preserved.
pub fn core_to_wit_update_request(
    request: &CoreUpdateRequest,
) -> Result<wit::UpdateRequest, SerializationError> {
    Ok(wit::UpdateRequest {
        current: core_to_wit_state(&request.from)?,
        patch: core_to_wit_update_patch(&request.patch)?,
    })
}

/// Build a [`wit::CreateRequest`] from the host-side core
/// [`CoreCreateRequest`].
pub fn core_to_wit_create_request(
    request: &CoreCreateRequest,
) -> Result<wit::CreateRequest, SerializationError> {
    Ok(wit::CreateRequest {
        res: core_to_wit_resource(request.resource.as_resource())?,
    })
}

/// Build a [`wit::ReadRequest`].
///
/// `ReadRequest` carries no operationally meaningful fields; the
/// `reserved` placeholder exists because the wasm component model
/// rejects records with zero fields.
pub fn core_to_wit_read_request(_request: &CoreReadRequest) -> wit::ReadRequest {
    wit::ReadRequest { reserved: false }
}

/// Build a [`wit::DeleteRequest`] from the host-side core
/// [`CoreDeleteRequest`].
pub fn core_to_wit_delete_request(request: &CoreDeleteRequest) -> wit::DeleteRequest {
    wit::DeleteRequest {
        directives: core_to_wit_directives(&request.directives),
    }
}

/// Convert a [`Directives`] to a [`wit::Directives`].
pub fn core_to_wit_directives(directives: &Directives) -> wit::Directives {
    wit::Directives {
        force_delete: directives.force_delete,
        create_before_destroy: directives.create_before_destroy,
        prevent_destroy: directives.prevent_destroy,
    }
}

/// Convert a host-side [`CoreUpdatePatch`] to a [`wit::UpdatePatch`].
/// Op order is preserved.
pub fn core_to_wit_update_patch(
    patch: &CoreUpdatePatch,
) -> Result<wit::UpdatePatch, SerializationError> {
    let ops = patch
        .ops
        .iter()
        .map(core_to_wit_patch_op)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(wit::UpdatePatch { ops })
}

/// Convert a host-side [`CorePatchOp`] to a [`wit::PatchOp`].
pub fn core_to_wit_patch_op(op: &CorePatchOp) -> Result<wit::PatchOp, SerializationError> {
    let value = match &op.value {
        Some(v) => Some(core_to_wit_value(v)?),
        None => None,
    };
    Ok(wit::PatchOp {
        kind: match op.kind {
            CorePatchOpKind::Add => wit::PatchOpKind::Add,
            CorePatchOpKind::Replace => wit::PatchOpKind::Replace,
            CorePatchOpKind::Remove => wit::PatchOpKind::Remove,
        },
        key: op.key.clone(),
        value,
    })
}

/// Convert a [`wit::UpdatePatch`] back to a host-side
/// [`CoreUpdatePatch`]. Used by tests and round-trip verification.
pub fn wit_to_core_update_patch(patch: &wit::UpdatePatch) -> CoreUpdatePatch {
    CoreUpdatePatch {
        ops: patch.ops.iter().map(wit_to_core_patch_op).collect(),
    }
}

/// Convert a [`wit::PatchOp`] to a host-side [`CorePatchOp`].
pub fn wit_to_core_patch_op(op: &wit::PatchOp) -> CorePatchOp {
    CorePatchOp {
        kind: match op.kind {
            wit::PatchOpKind::Add => CorePatchOpKind::Add,
            wit::PatchOpKind::Replace => CorePatchOpKind::Replace,
            wit::PatchOpKind::Remove => CorePatchOpKind::Remove,
        },
        key: op.key.clone(),
        value: op.value.as_ref().map(wit_to_core_value),
    }
}

/// Convert a host-side [`carina_core::schema::TypeIdentity`] to the
/// WIT [`wit::TypeIdentity`] record.
///
/// The WIT record has no `option` provider field; an absent provider
/// axis is encoded as an empty string (see the `type-identity` record
/// doc in `provider.wit`).
pub fn core_type_identity_to_wit(
    identity: &carina_core::schema::TypeIdentity,
) -> wit::TypeIdentity {
    wit::TypeIdentity {
        provider: identity.provider.clone().unwrap_or_default(),
        segments: identity.segments.clone(),
        kind: identity.kind.clone(),
    }
}

/// Deserialize JSON to (name, display_name, version) tuple from ProviderInfo.
pub fn json_to_provider_info(json: &str) -> (String, String, String) {
    if let Ok(info) = serde_json::from_str::<proto::ProviderInfo>(json) {
        (info.name, info.display_name, info.version)
    } else {
        (
            "unknown".to_string(),
            "Unknown Provider".to_string(),
            "0.0.0".to_string(),
        )
    }
}

/// Reject a provider whose protocol is older than this host's
/// carina_provider_protocol::PROTOCOL_VERSION. The SDK stamps the
/// version it was compiled against into the info() envelope, so this is a
/// compile-time fact of the provider's protocol crate, not a value the
/// author sets. A provider predating the envelope omits the field ->
/// deserializes to 0 -> below any host minimum (the carina#3364 class).
/// carina#3365.
pub fn check_protocol_version(info_json: &str) -> Result<(), String> {
    let envelope: proto::ProviderInfoEnvelope =
        serde_json::from_str(info_json).map_err(|e| format!("invalid provider info: {e}"))?;
    let host = carina_provider_protocol::PROTOCOL_VERSION;
    if envelope.protocol_version < host {
        return Err(format!(
            "provider '{}' was built against protocol version {} but this host \
             requires version {}; rebuild the provider against the current carina protocol",
            envelope.info.name, envelope.protocol_version, host
        ));
    }
    Ok(())
}

/// Deserialize JSON to a Vec of core ResourceSchemas.
pub fn json_to_schemas(json: &str) -> Result<Vec<CoreResourceSchema>, SchemaDecodeError> {
    let proto_schemas: Vec<proto::ResourceSchema> = serde_json::from_str(json).unwrap_or_default();
    proto_schemas.iter().map(proto_schema_to_core).collect()
}

/// Deserialize a JSON-encoded `HashMap<String, AttributeType>` from a WASM
/// guest and convert it to core `AttributeType` values.
pub fn json_to_attribute_types(
    json: &str,
) -> Result<HashMap<String, CoreAttributeType>, SchemaDecodeError> {
    let proto_types: HashMap<String, proto::AttributeType> =
        serde_json::from_str(json).unwrap_or_default();
    proto_types
        .into_iter()
        .map(|(k, v)| proto_attr_type_to_core(&v).map(|attr_type| (k, attr_type)))
        .collect()
}

// -- Protocol schema to core schema conversion --

fn proto_schema_to_core(
    s: &proto::ResourceSchema,
) -> Result<CoreResourceSchema, SchemaDecodeError> {
    Ok(CoreResourceSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .iter()
            .map(|(name, a)| proto_attr_schema_to_core(a).map(|schema| (name.clone(), schema)))
            .collect::<Result<_, _>>()?,
        description: s.description.clone(),
        validator: build_validator_from_types(&s.validators),
        kind: match s.kind {
            proto::SchemaKind::Managed => carina_core::schema::SchemaKind::Resource,
            proto::SchemaKind::DataSource => carina_core::schema::SchemaKind::DataSource,
        },
        name_attribute: s.name_attribute.clone(),
        operation_config: s.operation_config.as_ref().map(|c| {
            carina_core::schema::OperationConfig {
                delete_timeout_secs: c.delete_timeout_secs,
                delete_max_retries: c.delete_max_retries,
                create_timeout_secs: c.create_timeout_secs,
                create_max_retries: c.create_max_retries,
            }
        }),
        exclusive_required: s.exclusive_required.clone(),
        // Wait defaults are not (yet) carried across the WASM plugin
        // boundary — providers fall back to the carina-core constants
        // (`WAIT_DEFAULT_TIMEOUT` / `WAIT_DEFAULT_INTERVAL`) until the
        // protocol gains explicit fields. See carina-rs/carina#2825.
        default_wait_timeout: None,
        default_wait_interval: None,
        // Cyclic CFN struct definitions reachable via
        // `AttributeType::Ref`. Mirrors the wire `defs` map onto the
        // core schema so walk-sites that traverse a `Ref` can resolve
        // it against the resource's own def table (carina#3340).
        defs: s
            .defs
            .iter()
            .map(|(k, v)| proto_attr_type_to_core(v).map(|attr_type| (k.clone(), attr_type)))
            .collect::<Result<_, _>>()?,
    })
}

/// Reconstruct a validator function from serializable `ValidatorType` declarations.
fn build_validator_from_types(
    types: &[proto::ValidatorType],
) -> Option<carina_core::schema::ResourceValidator> {
    if types.is_empty() {
        return None;
    }
    // Currently only TagsKeyValueCheck exists. When more variants are added,
    // compose validators by collecting checks and running all of them.
    if types.contains(&proto::ValidatorType::TagsKeyValueCheck) {
        Some(validate_tags_key_value)
    } else {
        None
    }
}

fn validate_tags_key_value(
    attrs: &HashMap<String, CoreValue>,
) -> Result<(), Vec<carina_core::schema::TypeError>> {
    if let Some(CoreValue::Concrete(ConcreteValue::Map(map))) = attrs.get("tags") {
        let (mut has_key, mut has_value) = (false, false);
        for k in map.keys() {
            if k.eq_ignore_ascii_case("key") {
                has_key = true;
            } else if k.eq_ignore_ascii_case("value") {
                has_value = true;
            }
            if has_key && has_value {
                return Err(vec![carina_core::schema::TypeError::ResourceValidationFailed {
                    message: "tags map contains both 'key' and 'value' as keys, which looks like a Key/Value pair list. Use flat map syntax instead: tags = { Name = '...' }".to_string(),
                    attribute: Some("tags".to_string()),
                }]);
            }
        }
    }
    Ok(())
}

fn proto_attr_schema_to_core(
    a: &proto::AttributeSchema,
) -> Result<CoreAttributeSchema, SchemaDecodeError> {
    Ok(CoreAttributeSchema {
        name: a.name.clone(),
        attr_type: proto_attr_type_to_core(&a.attr_type)?,
        required: a.required,
        default: None,
        description: a.description.clone(),
        completions: None,
        provider_name: a.provider_name.clone(),
        create_only: a.create_only,
        read_only: a.read_only,
        removable: a.removable,
        block_name: a.block_name.clone(),
        write_only: a.write_only,
        identity: a.identity,
        // The WIT contract does not transmit `deferred_populate` —
        // the annotation lives entirely in the host-side schema; see
        // `proto_struct_field_to_core` for the rationale.
        deferred_populate: false,
    })
}

fn proto_attr_type_to_core(
    t: &proto::AttributeType,
) -> Result<CoreAttributeType, SchemaDecodeError> {
    Ok(match t {
        proto::AttributeType::String {
            pattern,
            length,
            to_dsl,
            identity,
            ..
        } => CoreAttributeType::refined_string(
            identity
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(carina_core::schema::TypeIdentity::from_dotted),
            pattern.clone(),
            *length,
            to_dsl.clone(),
        ),
        proto::AttributeType::Int { range, identity } => CoreAttributeType::refined_int(
            identity
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(carina_core::schema::TypeIdentity::from_dotted),
            *range,
        ),
        proto::AttributeType::Float { range, identity } => CoreAttributeType::refined_float(
            identity
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(carina_core::schema::TypeIdentity::from_dotted),
            *range,
        ),
        proto::AttributeType::Bool => CoreAttributeType::bool(),
        proto::AttributeType::Duration => CoreAttributeType::duration(),
        proto::AttributeType::StringEnum {
            values,
            name,
            namespace,
            dsl_aliases,
        } => CoreAttributeType::enum_(
            // Lift the wire-form flat dotted prefix into the
            // structured `TypeIdentity` the core schema now carries
            // (carina#3222). The pre-#3222 core form mirrored the
            // wire form one-to-one; with the split, the wire form
            // remains flat while the core form is structural — the
            // boundary cost lives here.
            namespace
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|ns| carina_core::schema::enum_identity(name, Some(ns)))
                .unwrap_or_else(|| carina_core::schema::TypeIdentity::bare(name)),
            Some(values.clone()),
            dsl_aliases.clone(),
            None,
            None,
        ),
        proto::AttributeType::List {
            element_type,
            ordered,
            length,
            ..
        } => CoreAttributeType::refined_list(
            proto_attr_type_to_core(element_type)?,
            *ordered,
            *length,
            legacy_validator(|_| Ok(())),
        ),
        proto::AttributeType::Map { inner, key } => CoreAttributeType::map_with_key(
            proto_attr_type_to_core(key)?,
            proto_attr_type_to_core(inner)?,
        ),
        proto::AttributeType::Struct { name, fields } => CoreAttributeType::struct_(
            name.clone(),
            fields
                .iter()
                .map(proto_struct_field_to_core)
                .collect::<Result<_, _>>()?,
        ),
        proto::AttributeType::Union { members } => CoreAttributeType::union(
            members
                .iter()
                .map(proto_attr_type_to_core)
                .collect::<Result<_, _>>()?,
        ),
        proto::AttributeType::Custom {
            name,
            base,
            pattern,
            length,
            to_dsl,
            ..
        } => proto_legacy_custom_to_core(name, base, pattern.clone(), *length, to_dsl.clone())?,
        proto::AttributeType::CustomEnum {
            name,
            base,
            namespace,
            dsl_transform,
        } => CoreAttributeType::enum_with_base(
            // Enum requires a populated identity (the
            // shorthand expansion needs the dotted prefix);
            // `enum_identity` is the inverse of
            // `TypeIdentity::dotted_prefix` and recovers the
            // structured form from the wire's flat `namespace + name`
            // shape.
            carina_core::schema::enum_identity(name, Some(namespace.as_str())),
            proto_attr_type_to_core(base)?,
            None,
            vec![],
            None, // Validation is handled via ProviderContext.validators
            dsl_transform.clone(),
        ),
        // Cyclic CFN struct reference (carina#3340). The host's
        // structural counterpart is `AttributeType::Ref`; the matching
        // `ResourceSchema.defs` map is converted alongside in
        // `proto_schema_to_core` so resolution at walk-sites succeeds.
        proto::AttributeType::Ref { name } => CoreAttributeType::ref_(name.clone()),
    })
}

fn proto_legacy_custom_to_core(
    enclosing_name: &str,
    base: &proto::AttributeType,
    pattern: Option<String>,
    length: Option<(Option<u64>, Option<u64>)>,
    to_dsl: Option<proto::DslTransform>,
) -> Result<CoreAttributeType, SchemaDecodeError> {
    if let proto::AttributeType::Custom {
        base,
        pattern: inner_pattern,
        length: inner_length,
        to_dsl: inner_to_dsl,
        ..
    } = base
    {
        // carina#3459: pre-namespace-adoption providers can encode resource
        // IDs as Custom -> Custom -> primitive refinement chains. The outer
        // name remains the only identity, with no inner-name fallback even if
        // empty; pattern/length/to_dsl use outer-first fallback.
        return proto_legacy_custom_to_core(
            enclosing_name,
            base,
            pattern.or_else(|| inner_pattern.clone()),
            length.or(*inner_length),
            to_dsl.or_else(|| inner_to_dsl.clone()),
        );
    }

    let identity = if enclosing_name.is_empty() {
        None
    } else {
        Some(carina_core::schema::TypeIdentity::from_dotted(
            enclosing_name,
        ))
    };
    let validate = legacy_validator(|_| Ok(())); // Validation is handled via ProviderContext.validators

    match base {
        proto::AttributeType::String { .. } => {
            Ok(CoreAttributeType::refined_string_with_validator(
                identity, pattern, length, validate, to_dsl,
            ))
        }
        proto::AttributeType::Int { .. } => Ok(CoreAttributeType::refined_int_with_validator(
            identity,
            length.map(|(min, max)| (min.map(|v| v as i64), max.map(|v| v as i64))),
            validate,
        )),
        proto::AttributeType::Float { .. } => Ok(CoreAttributeType::refined_float_with_validator(
            identity, None, validate,
        )),
        proto::AttributeType::List {
            element_type,
            ordered,
            ..
        } => Ok(CoreAttributeType::refined_list(
            proto_attr_type_to_core(element_type)?,
            *ordered,
            length,
            validate,
        )),
        other => Err(SchemaDecodeError::unsupported_custom_base(
            enclosing_name,
            other,
        )),
    }
}

fn proto_struct_field_to_core(
    f: &proto::StructField,
) -> Result<CoreStructField, SchemaDecodeError> {
    Ok(CoreStructField {
        name: f.name.clone(),
        field_type: proto_attr_type_to_core(&f.field_type)?,
        required: f.required,
        description: f.description.clone(),
        provider_name: f.provider_name.clone(),
        block_name: f.block_name.clone(),
        // The WIT contract does not transmit `deferred_populate` —
        // the annotation lives entirely in the host-side schema (set
        // by the provider's codegen output in
        // `carina-provider-{aws,awscc}/.../schemas/generated/`),
        // which is loaded directly via the SchemaRegistry rather
        // than crossing the WASM boundary. carina#3034.
        deferred_populate: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proto_string() -> proto::AttributeType {
        proto::AttributeType::String {
            pattern: None,
            length: None,
            validate: None,
            to_dsl: None,
            identity: None,
        }
    }

    fn proto_custom(
        name: &str,
        base: proto::AttributeType,
        pattern: Option<&str>,
        length: Option<(Option<u64>, Option<u64>)>,
    ) -> proto::AttributeType {
        proto::AttributeType::Custom {
            name: name.to_string(),
            base: Box::new(base),
            pattern: pattern.map(str::to_string),
            length,
            validate: None,
            to_dsl: None,
        }
    }

    #[test]
    fn binding_pattern_round_trips_through_wit() {
        let patterns = vec![
            CoreBindingPattern::Exact("validation_record".to_string()),
            CoreBindingPattern::ForLoopChildren {
                base: "validation_records".to_string(),
            },
            CoreBindingPattern::AttributeMatch {
                resource_type: "route53.RecordSet".to_string(),
                attr: CoreAttrPath::single("name"),
                from: CoreAttrPath::try_new(vec![
                    "domain_validation_options".to_string(),
                    "resource_record".to_string(),
                    "name".to_string(),
                ])
                .unwrap(),
            },
        ];

        for pattern in patterns {
            let wit = core_to_wit_binding_pattern(&pattern);
            assert_eq!(wit_to_core_binding_pattern(wit).unwrap(), pattern);
        }
    }

    #[test]
    fn wit_to_core_binding_pattern_returns_err_for_empty_attr_path() {
        let wit = wit::BindingPattern::AttributeMatch(wit::AttributeMatchPattern {
            resource_type: "route53.RecordSet".to_string(),
            attr: vec![],
            from: vec!["resource_record_name".to_string()],
        });

        assert!(matches!(
            wit_to_core_binding_pattern(wit),
            Err(AttrPathError::Empty)
        ));
    }

    #[test]
    fn wit_to_core_binding_pattern_returns_err_for_empty_from_path() {
        let wit = wit::BindingPattern::AttributeMatch(wit::AttributeMatchPattern {
            resource_type: "route53.RecordSet".to_string(),
            attr: vec!["name".to_string()],
            from: vec![],
        });

        assert!(matches!(
            wit_to_core_binding_pattern(wit),
            Err(AttrPathError::Empty)
        ));
    }

    #[test]
    fn wit_to_core_binding_pattern_ok_for_non_empty_paths() {
        let wit = wit::BindingPattern::AttributeMatch(wit::AttributeMatchPattern {
            resource_type: "route53.RecordSet".to_string(),
            attr: vec!["name".to_string()],
            from: vec!["resource_record_name".to_string()],
        });

        assert_eq!(
            wit_to_core_binding_pattern(wit).unwrap(),
            CoreBindingPattern::AttributeMatch {
                resource_type: "route53.RecordSet".to_string(),
                attr: CoreAttrPath::single("name"),
                from: CoreAttrPath::single("resource_record_name"),
            }
        );
    }

    #[test]
    fn wit_to_core_binding_pattern_ok_for_exact() {
        let wit = wit::BindingPattern::Exact("validation_record".to_string());

        assert_eq!(
            wit_to_core_binding_pattern(wit).unwrap(),
            CoreBindingPattern::Exact("validation_record".to_string())
        );
    }

    /// RFC #2371 stage 4 contract pin: `Value::Deferred(DeferredValue::Unknown)` reaching either
    /// WASM-boundary converter is a stage-2 invariant violation
    /// (`PlanPreprocessor::strip_unknown_attributes` must remove it
    /// first). The converters now return `Err(SerializationError::
    /// UnknownNotAllowed)` so a silent `format!("{v:?}")` fallback
    /// would re-introduce the v1 corruption bug (#2375); these tests
    /// pin the contract by asserting the variant + context survives.
    #[test]
    fn unknown_returns_err_in_core_to_wit_value() {
        use carina_core::resource::{AccessPath, UnknownReason};
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let v = CoreValue::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        }));
        let err = core_to_wit_value(&v).unwrap_err();
        match err {
            SerializationError::UnknownNotAllowed {
                reason: UnknownReason::UpstreamRef { path: p },
                context: SerializationContext::WasmBoundary,
            } => assert_eq!(p, path),
            other => panic!("expected UnknownNotAllowed/UpstreamRef/WasmBoundary, got: {other:?}"),
        }
    }

    #[test]
    fn unknown_returns_err_in_core_value_to_json() {
        use carina_core::resource::{AccessPath, UnknownReason};
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let v = CoreValue::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        }));
        let err = core_value_to_json(&v).unwrap_err();
        assert!(matches!(
            err,
            SerializationError::UnknownNotAllowed {
                reason: UnknownReason::UpstreamRef { .. },
                context: SerializationContext::WasmBoundary,
            }
        ));
    }

    // -- Value roundtrips --

    #[test]
    fn test_scalar_bool_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::Bool(true));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_int_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::Int(42));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_float_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::Float(2.78));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_string_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::String("hello".into()));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_list_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::List(vec![
            CoreValue::Concrete(ConcreteValue::String("a".into())),
            CoreValue::Concrete(ConcreteValue::Int(1)),
            CoreValue::Concrete(ConcreteValue::Bool(false)),
        ]));
        let wit = core_to_wit_value(&core).unwrap();
        assert!(matches!(wit, wit::Value::ListVal(_)));
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_map_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::Map(
            vec![
                (
                    "key1".to_string(),
                    CoreValue::Concrete(ConcreteValue::String("val1".into())),
                ),
                (
                    "key2".to_string(),
                    CoreValue::Concrete(ConcreteValue::Int(99)),
                ),
            ]
            .into_iter()
            .collect(),
        ));
        let wit = core_to_wit_value(&core).unwrap();
        assert!(matches!(wit, wit::Value::MapVal(_)));
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_nested_list_of_maps_roundtrip() {
        let inner_map = CoreValue::Concrete(ConcreteValue::Map(
            vec![
                (
                    "name".to_string(),
                    CoreValue::Concrete(ConcreteValue::String("test".into())),
                ),
                (
                    "count".to_string(),
                    CoreValue::Concrete(ConcreteValue::Int(5)),
                ),
            ]
            .into_iter()
            .collect(),
        ));
        let core = CoreValue::Concrete(ConcreteValue::List(vec![inner_map.clone(), inner_map]));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_nested_map_of_lists_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::Map(
            vec![
                (
                    "tags".to_string(),
                    CoreValue::Concrete(ConcreteValue::List(vec![
                        CoreValue::Concrete(ConcreteValue::String("a".into())),
                        CoreValue::Concrete(ConcreteValue::String("b".into())),
                    ])),
                ),
                (
                    "counts".to_string(),
                    CoreValue::Concrete(ConcreteValue::List(vec![
                        CoreValue::Concrete(ConcreteValue::Int(1)),
                        CoreValue::Concrete(ConcreteValue::Int(2)),
                    ])),
                ),
            ]
            .into_iter()
            .collect(),
        ));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    /// #2387: `Value::Deferred(DeferredValue::ResourceRef)` reaching `core_to_wit_value` is a
    /// stage-2 invariant violation now — the strip-and-restore pass in
    /// `PlanPreprocessor::prepare` removes any attribute that
    /// recursively contains a `ResourceRef` before this boundary. The
    /// converter returns `Err(UnresolvedResourceRef)` so a regression
    /// surfaces as a typed error at the call site instead of a
    /// debug-format string silently flowing into the provider.
    #[test]
    fn resource_ref_returns_err_in_core_to_wit_value() {
        use carina_core::resource::AccessPath;
        let path = AccessPath::new("sso", "identity_store_id");
        let v = CoreValue::Deferred(DeferredValue::ResourceRef { path: path.clone() });
        let err = core_to_wit_value(&v).unwrap_err();
        match err {
            SerializationError::UnresolvedResourceRef {
                path: p,
                context: SerializationContext::WasmBoundary,
            } => assert_eq!(p, path.to_dot_string()),
            other => panic!("expected UnresolvedResourceRef/WasmBoundary, got: {other:?}"),
        }
    }

    #[test]
    fn interpolation_returns_err_in_core_to_wit_value() {
        use carina_core::resource::{AccessPath, InterpolationPart};
        let v = CoreValue::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(CoreValue::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("vpc", "id"),
            })),
        ]));
        let err = core_to_wit_value(&v).unwrap_err();
        assert!(matches!(
            err,
            SerializationError::UnresolvedInterpolation {
                context: SerializationContext::WasmBoundary,
            }
        ));
    }

    #[test]
    fn function_call_returns_err_in_core_to_wit_value() {
        use carina_core::resource::AccessPath;
        let v = CoreValue::Deferred(DeferredValue::FunctionCall {
            name: "join".into(),
            args: vec![
                CoreValue::Concrete(ConcreteValue::String("-".into())),
                CoreValue::Deferred(DeferredValue::ResourceRef {
                    path: AccessPath::new("vpc", "id"),
                }),
            ],
        });
        let err = core_to_wit_value(&v).unwrap_err();
        match err {
            SerializationError::UnresolvedFunctionCall {
                name,
                context: SerializationContext::WasmBoundary,
            } => assert_eq!(name, "join"),
            other => panic!("expected UnresolvedFunctionCall/WasmBoundary, got: {other:?}"),
        }
    }

    #[test]
    fn resource_ref_returns_err_in_core_value_to_json() {
        use carina_core::resource::AccessPath;
        let path = AccessPath::new("sso", "identity_store_id");
        let v = CoreValue::Deferred(DeferredValue::ResourceRef { path: path.clone() });
        let err = core_value_to_json(&v).unwrap_err();
        assert!(matches!(
            err,
            SerializationError::UnresolvedResourceRef {
                context: SerializationContext::WasmBoundary,
                ..
            }
        ));
    }

    /// #2390: `Value::Deferred(DeferredValue::Secret)` crosses the boundary as the `secret-val`
    /// WIT variant carrying the JSON-encoded inner — never as a
    /// `format!("{v:?}")` debug string.
    #[test]
    fn secret_string_emits_secret_val_variant() {
        let v = CoreValue::Deferred(DeferredValue::Secret(Box::new(CoreValue::Concrete(
            ConcreteValue::String("password".into()),
        ))));
        let wit_v = core_to_wit_value(&v).unwrap();
        match wit_v {
            wit::Value::SecretVal(json) => assert_eq!(json, "\"password\""),
            other => panic!("expected SecretVal, got: {other:?}"),
        }
    }

    #[test]
    fn secret_int_emits_secret_val_variant() {
        // Audit (carina#2390 step 12) confirmed `secret()` accepts any
        // scalar; pin the wire format for non-string inner values too.
        let v = CoreValue::Deferred(DeferredValue::Secret(Box::new(CoreValue::Concrete(
            ConcreteValue::Int(42),
        ))));
        let wit_v = core_to_wit_value(&v).unwrap();
        match wit_v {
            wit::Value::SecretVal(json) => assert_eq!(json, "42"),
            other => panic!("expected SecretVal, got: {other:?}"),
        }
    }

    #[test]
    fn secret_round_trip_preserves_inner_value() {
        // Round-tripping through the WIT boundary preserves the
        // `Secret` wrapper so host-side machinery (state hashing, plan
        // redaction) keeps recognising it after a guest call returns.
        let original = CoreValue::Deferred(DeferredValue::Secret(Box::new(CoreValue::Concrete(
            ConcreteValue::String("hunter2".into()),
        ))));
        let wit_v = core_to_wit_value(&original).unwrap();
        let back = wit_to_core_value(&wit_v);
        match back {
            CoreValue::Deferred(DeferredValue::Secret(inner)) => match *inner {
                CoreValue::Concrete(ConcreteValue::String(s)) => assert_eq!(s, "hunter2"),
                other => panic!("expected inner String, got: {other:?}"),
            },
            other => panic!("expected Secret, got: {other:?}"),
        }
    }

    #[test]
    fn secret_in_list_serialises_inner_to_json() {
        // Within `core_value_to_json` (the helper that flattens nested
        // structures into the `list-val` / `map-val` JSON payload) a
        // `Secret` is serialised as its inner JSON shape — providers
        // decoding the list see the operational value. The
        // *attribute*-level secret signal goes through `secret-val` via
        // `core_to_wit_value`, not this path.
        let v = CoreValue::Concrete(ConcreteValue::List(vec![CoreValue::Deferred(
            DeferredValue::Secret(Box::new(CoreValue::Concrete(ConcreteValue::String(
                "p".into(),
            )))),
        )]));
        let wit_v = core_to_wit_value(&v).unwrap();
        match wit_v {
            wit::Value::ListVal(json) => assert_eq!(json, "[\"p\"]"),
            other => panic!("expected ListVal, got: {other:?}"),
        }
    }

    // -- ResourceId roundtrip --

    #[test]
    fn test_resource_id_roundtrip() {
        let core = CoreResourceId::with_provider("aws", "s3.Bucket", "my-bucket", None);
        let wit = core_to_wit_resource_id(&core);
        assert_eq!(wit.provider, "aws");
        assert_eq!(wit.resource_type, "s3.Bucket");
        assert_eq!(wit.name, "my-bucket");
        let back = wit_to_core_resource_id(&wit);
        assert_eq!(core, back);
    }

    // -- State roundtrip --

    #[test]
    fn test_state_roundtrip() {
        let id = CoreResourceId::with_provider("aws", "s3.Bucket", "my-bucket", None);
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".into(),
            CoreValue::Concrete(ConcreteValue::String("my-bucket".into())),
        );
        attrs.insert(
            "region".into(),
            CoreValue::Concrete(ConcreteValue::String("us-east-1".into())),
        );
        let core = CoreState::existing(id.clone(), attrs);

        let wit = core_to_wit_state(&core).unwrap();
        let back = wit_to_core_state(&wit, &id);

        assert_eq!(back.id, core.id);
        assert_eq!(back.attributes, core.attributes);
        assert!(back.exists);
    }

    #[test]
    fn test_state_with_identifier_roundtrip() {
        let id = CoreResourceId::with_provider("aws", "ec2.Vpc", "main", None);
        let attrs = HashMap::from([(
            "cidr".into(),
            CoreValue::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        )]);
        let core = CoreState::existing(id.clone(), attrs).with_identifier("vpc-12345");

        let wit = core_to_wit_state(&core).unwrap();
        assert_eq!(wit.identifier, Some("vpc-12345".into()));
        let back = wit_to_core_state(&wit, &id);
        assert_eq!(back.identifier, Some("vpc-12345".into()));
    }

    // -- Value map helpers --

    #[test]
    fn test_value_map_roundtrip() {
        let map: HashMap<String, CoreValue> = vec![
            (
                "a".into(),
                CoreValue::Concrete(ConcreteValue::String("hello".into())),
            ),
            ("b".into(), CoreValue::Concrete(ConcreteValue::Int(42))),
        ]
        .into_iter()
        .collect();

        let wit = core_to_wit_value_map(&map).unwrap();
        let back = wit_to_core_value_map(&wit);
        assert_eq!(map, back);
    }

    // -- Resource roundtrip --

    #[test]
    fn test_resource_roundtrip() {
        let mut resource = CoreResource::with_provider("aws", "s3.Bucket", "my-bucket", None);
        resource.attributes = indexmap::IndexMap::from([
            (
                "name".into(),
                CoreValue::Concrete(ConcreteValue::String("my-bucket".into())),
            ),
            (
                "region".into(),
                CoreValue::Concrete(ConcreteValue::String("us-east-1".into())),
            ),
        ]);

        let wit = core_to_wit_resource(&resource).unwrap();
        assert_eq!(wit.id.provider, "aws");
        assert_eq!(wit.id.resource_type, "s3.Bucket");
        assert_eq!(wit.id.name, "my-bucket");

        let back = wit_to_core_resource(&wit);
        assert_eq!(back.id, resource.id);
        // Compare resolved attributes
        assert_eq!(back.resolved_attributes(), resource.resolved_attributes());
    }

    // -- JSON passthrough tests --

    #[test]
    fn test_directives_to_json() {
        let directives = Directives {
            force_delete: true,
            create_before_destroy: false,
            prevent_destroy: false,
            depends_on: Vec::new(),
            ..Directives::default()
        };
        let json = directives_to_json(&directives);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["force_delete"], true);
        assert_eq!(parsed["create_before_destroy"], false);
        assert_eq!(parsed["prevent_destroy"], false);
    }

    #[test]
    fn test_provider_error_variant_round_trip_through_wit() {
        // Build host-side core errors of every variant, convert them
        // to the WIT shape, then back. The variant tag must be
        // preserved exactly so host code matching on the variant
        // still works after a round trip across the WIT boundary.
        type VariantCheck = fn(&CoreProviderError) -> bool;
        let cases: Vec<(CoreProviderError, VariantCheck)> = vec![
            (CoreProviderError::invalid_input("bad input"), |e| {
                matches!(e, CoreProviderError::InvalidInput(_))
            }),
            (CoreProviderError::api_error("rejected"), |e| {
                matches!(e, CoreProviderError::ApiError(_))
            }),
            (CoreProviderError::not_found("missing"), |e| {
                matches!(e, CoreProviderError::NotFound(_))
            }),
            (CoreProviderError::timeout("slow"), |e| {
                matches!(e, CoreProviderError::Timeout(_))
            }),
            (CoreProviderError::internal("bug"), |e| {
                matches!(e, CoreProviderError::Internal(_))
            }),
        ];

        for (err, is_variant) in cases {
            let wit_err = core_to_wit_provider_error(&err);
            let back = wit_to_core_provider_error(wit_err);
            assert!(is_variant(&back), "variant lost for {}", err.variant_name());
            assert_eq!(back.message(), err.message(), "message lost");
        }
    }

    #[test]
    fn test_provider_error_detail_fields_round_trip() {
        // Resource id, cause string, and provider name must all
        // survive a host -> WIT -> host round trip. cause is
        // flattened to a string at the boundary because WIT cannot
        // carry `dyn std::error::Error`.
        let cause = std::io::Error::other("inner io error");
        let id = CoreResourceId::with_provider("aws", "s3.Bucket", "my-bucket", None);
        let err = CoreProviderError::api_error("Failed to read")
            .with_cause(cause)
            .for_resource(id.clone())
            .for_provider("aws");

        let wit_err = core_to_wit_provider_error(&err);
        let back = wit_to_core_provider_error(wit_err);
        assert!(matches!(back, CoreProviderError::ApiError(_)));
        let detail = back.detail();
        assert_eq!(detail.message, "Failed to read");
        assert_eq!(detail.provider_name.as_deref(), Some("aws"));
        let rid = detail.resource_id.as_ref().expect("resource_id preserved");
        assert_eq!(rid.provider, "aws");
        assert_eq!(rid.resource_type, "s3.Bucket");
        assert_eq!(rid.name_str(), "my-bucket");
        let cause_str = detail
            .cause
            .as_ref()
            .map(|c| c.to_string())
            .expect("cause preserved as string");
        assert_eq!(cause_str, "inner io error");
    }

    /// carina#3242: the new structured cloud-API metadata fields
    /// (operation, status, code, request_id) must survive the
    /// host → WIT → host round trip. Without this, provider-aws would
    /// populate the fields, the host renderer would never see them,
    /// and the operator would still get the legacy single-line dump.
    #[test]
    fn test_provider_error_structured_cloud_fields_round_trip() {
        let err = CoreProviderError::api_error("Failed to list IAM roles")
            .with_operation("iam.ListRoles")
            .with_status(403)
            .with_code("AccessDenied")
            .with_request_id("997aa923-2aa4-4d2b-8d16-44fd21c81368");

        let wit_err = core_to_wit_provider_error(&err);
        let back = wit_to_core_provider_error(wit_err);
        let detail = back.detail();
        assert_eq!(detail.operation.as_deref(), Some("iam.ListRoles"));
        assert_eq!(detail.status, Some(403));
        assert_eq!(detail.code.as_deref(), Some("AccessDenied"));
        assert_eq!(
            detail.request_id.as_deref(),
            Some("997aa923-2aa4-4d2b-8d16-44fd21c81368"),
        );
    }

    #[test]
    fn test_update_patch_round_trip_preserves_op_order_and_kinds() {
        use carina_core::resource::Value as CV;
        let patch = CoreUpdatePatch {
            ops: vec![
                CorePatchOp {
                    kind: CorePatchOpKind::Add,
                    key: "a".to_string(),
                    value: Some(CV::Concrete(ConcreteValue::String("alpha".into()))),
                },
                CorePatchOp {
                    kind: CorePatchOpKind::Replace,
                    key: "b".to_string(),
                    value: Some(CV::Concrete(ConcreteValue::Int(42))),
                },
                CorePatchOp {
                    kind: CorePatchOpKind::Remove,
                    key: "c".to_string(),
                    value: None,
                },
            ],
        };
        let wit_patch = core_to_wit_update_patch(&patch).unwrap();
        let back = wit_to_core_update_patch(&wit_patch);
        assert_eq!(back.ops.len(), 3);
        assert_eq!(back.ops[0].kind, CorePatchOpKind::Add);
        assert_eq!(back.ops[0].key, "a");
        assert_eq!(back.ops[1].kind, CorePatchOpKind::Replace);
        assert_eq!(back.ops[1].key, "b");
        assert_eq!(back.ops[2].kind, CorePatchOpKind::Remove);
        assert_eq!(back.ops[2].key, "c");
        assert!(back.ops[2].value.is_none(), "Remove must carry None");
    }

    #[test]
    fn test_json_to_provider_info_valid() {
        let json = r#"{"name":"aws","display_name":"AWS Provider","version":"1.0.0"}"#;
        let (name, display, version) = json_to_provider_info(json);
        assert_eq!(name, "aws");
        assert_eq!(display, "AWS Provider");
        assert_eq!(version, "1.0.0");
    }

    #[test]
    fn test_json_to_provider_info_missing_version_falls_back() {
        // When version is missing entirely (complete parse failure), fall back to "0.0.0"
        let json = r#"{"name":"old","display_name":"Old Provider"}"#;
        let (name, display, version) = json_to_provider_info(json);
        assert_eq!(name, "unknown");
        assert_eq!(display, "Unknown Provider");
        assert_eq!(version, "0.0.0");
    }

    #[test]
    fn test_check_protocol_version_accepts_host_version() {
        let json = format!(
            r#"{{"name":"aws","display_name":"AWS Provider","version":"1.0.0","protocol_version":{}}}"#,
            carina_provider_protocol::PROTOCOL_VERSION
        );

        assert!(check_protocol_version(&json).is_ok());
    }

    #[test]
    fn test_check_protocol_version_rejects_lower_version() {
        let err = check_protocol_version(
            r#"{"name":"aws","display_name":"AWS Provider","version":"1.0.0","protocol_version":0}"#,
        )
        .unwrap_err();

        assert!(err.contains("provider 'aws'"));
        assert!(err.contains("protocol version 0"));
        assert!(err.contains(&format!(
            "requires version {}",
            carina_provider_protocol::PROTOCOL_VERSION
        )));
    }

    #[test]
    fn test_check_protocol_version_rejects_old_style_info_without_protocol_version() {
        let err = check_protocol_version(
            r#"{"name":"old","display_name":"Old Provider","version":"1.0.0"}"#,
        )
        .unwrap_err();

        assert!(err.contains("provider 'old'"));
        assert!(err.contains("protocol version 0"));
        assert!(err.contains(&format!(
            "requires version {}",
            carina_provider_protocol::PROTOCOL_VERSION
        )));
    }

    #[test]
    fn test_json_to_schemas_empty() {
        let schemas = json_to_schemas("[]").unwrap();
        assert!(schemas.is_empty());
    }

    #[test]
    fn test_json_to_schemas_with_complex_attributes() {
        let json = r#"[
          {
            "resource_type": "ec2.SecurityGroup",
            "description": "EC2 Security Group",
            "name_attribute": "group_name",
            "attributes": {
              "ingress": {
                "name": "ingress",
                "attr_type": {
                  "type": "list",
                  "inner": {
                    "type": "union",
                    "members": [
                      {
                        "type": "struct",
                        "name": "IngressRule",
                        "fields": [
                          {
                            "name": "from_port",
                            "field_type": { "type": "Int" },
                            "required": true,
                            "description": "Start of port range",
                            "block_name": "from_port_block",
                            "provider_name": "FromPort"
                          },
                          {
                            "name": "protocol",
                            "field_type": { "type": "String" },
                            "required": true,
                            "description": null,
                            "block_name": null,
                            "provider_name": null
                          }
                        ]
                      },
                      { "type": "String" }
                    ]
                  },
                  "ordered": false
                },
                "required": false,
                "description": "Ingress rules",
                "create_only": false,
                "read_only": false,
                "write_only": false,
                "block_name": "ingress_block",
                "provider_name": "IpPermissions",
                "removable": false
              },
              "description": {
                "name": "description",
                "attr_type": { "type": "String" },
                "required": true,
                "description": "Group description",
                "create_only": false,
                "read_only": false,
                "write_only": false
              },
              "enabled": {
                "name": "enabled",
                "attr_type": { "type": "Bool" },
                "required": false,
                "description": null,
                "create_only": false,
                "read_only": false,
                "write_only": false
              },
              "priority": {
                "name": "priority",
                "attr_type": { "type": "Int" },
                "required": false,
                "description": null,
                "create_only": false,
                "read_only": false,
                "write_only": false
              }
            }
          }
        ]"#;

        let schemas = json_to_schemas(json).unwrap();
        assert_eq!(schemas.len(), 1);

        let schema = &schemas[0];
        assert_eq!(schema.resource_type, "ec2.SecurityGroup");
        assert_eq!(schema.description.as_deref(), Some("EC2 Security Group"));
        assert!(!schema.is_data_source());
        assert_eq!(schema.name_attribute.as_deref(), Some("group_name"));

        // Basic attribute types
        let desc_attr = schema
            .attributes
            .get("description")
            .expect("description attribute");
        assert_eq!(desc_attr.name, "description");
        assert!(matches!(
            desc_attr
                .attr_type
                .shape_ref_free()
                .expect("test schema is Ref-free"),
            carina_core::schema::Shape::String { .. }
        ));
        assert!(desc_attr.required);

        let enabled_attr = schema.attributes.get("enabled").expect("enabled attribute");
        assert!(matches!(
            enabled_attr
                .attr_type
                .shape_ref_free()
                .expect("test schema is Ref-free"),
            carina_core::schema::Shape::Bool
        ));

        let priority_attr = schema
            .attributes
            .get("priority")
            .expect("priority attribute");
        assert!(matches!(
            priority_attr
                .attr_type
                .shape_ref_free()
                .expect("test schema is Ref-free"),
            carina_core::schema::Shape::Int { .. }
        ));

        // Ingress attribute: list with ordered=false, provider_name, block_name, removable
        let ingress_attr = schema.attributes.get("ingress").expect("ingress attribute");
        assert_eq!(ingress_attr.provider_name.as_deref(), Some("IpPermissions"));
        assert_eq!(ingress_attr.block_name.as_deref(), Some("ingress_block"));
        assert_eq!(ingress_attr.removable, Some(false));

        // List with ordered: false
        match ingress_attr
            .attr_type
            .shape_ref_free()
            .expect("test schema is Ref-free")
        {
            carina_core::schema::Shape::List {
                element_type: inner,
                ordered,
                ..
            } => {
                assert!(!ordered, "list should be unordered");

                // Union inside list
                match inner.raw_shape() {
                    carina_core::schema::RawShape::Union(members) => {
                        assert_eq!(members.len(), 2);

                        // First member: struct with block_name and provider_name on fields
                        match members[0].raw_shape() {
                            carina_core::schema::RawShape::Struct { name, fields } => {
                                assert_eq!(name, "IngressRule");
                                assert_eq!(fields.len(), 2);

                                let from_port = &fields[0];
                                assert_eq!(from_port.name, "from_port");
                                assert!(matches!(
                                    from_port
                                        .field_type
                                        .shape_ref_free()
                                        .expect("test schema is Ref-free"),
                                    carina_core::schema::Shape::Int { .. }
                                ));
                                assert!(from_port.required);
                                assert_eq!(
                                    from_port.description.as_deref(),
                                    Some("Start of port range")
                                );
                                assert_eq!(
                                    from_port.block_name.as_deref(),
                                    Some("from_port_block")
                                );
                                assert_eq!(from_port.provider_name.as_deref(), Some("FromPort"));

                                let protocol = &fields[1];
                                assert_eq!(protocol.name, "protocol");
                                assert!(matches!(
                                    protocol
                                        .field_type
                                        .shape_ref_free()
                                        .expect("test schema is Ref-free"),
                                    carina_core::schema::Shape::String { .. }
                                ));
                                assert!(protocol.block_name.is_none());
                                assert!(protocol.provider_name.is_none());
                            }
                            other => panic!("expected Struct, got {:?}", other),
                        }

                        // Second member: String
                        assert!(matches!(
                            members[1]
                                .shape_ref_free()
                                .expect("test schema is Ref-free"),
                            carina_core::schema::Shape::String { .. }
                        ));
                    }
                    other => panic!("expected Union inside list, got {:?}", other),
                }
            }
            other => panic!("expected List, got {:?}", other),
        }
    }

    #[test]
    fn json_to_attribute_types_decodes_duration() {
        // Acceptance for carina#3166: providers that declare a
        // Duration-typed schema attribute via `provider_config_attribute_types`
        // (e.g. assume_role.duration in aws#342 / awscc#260) must
        // round-trip through `{"type":"Duration"}` to
        // `CoreAttributeType::duration()` so the host's type checker
        // accepts `duration = 30min` against that declaration.
        let json = r#"{"timeout":{"type":"Duration"}}"#;
        let types = json_to_attribute_types(json).unwrap();
        assert!(matches!(
            types
                .get("timeout")
                .map(|t| t.shape_ref_free().expect("test schema is Ref-free")),
            Some(carina_core::schema::Shape::Duration)
        ));
    }

    #[test]
    fn test_deeply_nested_list_map_roundtrip() {
        let policy_document = CoreValue::Concrete(ConcreteValue::Map(
            vec![
                (
                    "version".to_string(),
                    CoreValue::Concrete(ConcreteValue::String("2012-10-17".into())),
                ),
                (
                    "statement".to_string(),
                    CoreValue::Concrete(ConcreteValue::List(vec![CoreValue::Concrete(
                        ConcreteValue::Map(
                            vec![
                                (
                                    "effect".to_string(),
                                    CoreValue::Concrete(ConcreteValue::String("Allow".into())),
                                ),
                                (
                                    "action".to_string(),
                                    CoreValue::Concrete(ConcreteValue::String(
                                        "logs:CreateLogGroup".into(),
                                    )),
                                ),
                                (
                                    "resource".to_string(),
                                    CoreValue::Concrete(ConcreteValue::String("*".into())),
                                ),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    )])),
                ),
            ]
            .into_iter()
            .collect(),
        ));
        let policies = CoreValue::Concrete(ConcreteValue::List(vec![CoreValue::Concrete(
            ConcreteValue::Map(
                vec![
                    (
                        "policy_name".to_string(),
                        CoreValue::Concrete(ConcreteValue::String("test-policy".into())),
                    ),
                    ("policy_document".to_string(), policy_document),
                ]
                .into_iter()
                .collect(),
            ),
        )]));

        let wit = core_to_wit_value(&policies).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(policies, back);
    }

    #[test]
    fn test_proto_schema_with_tags_validator_reconstructed() {
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.s3.Bucket".to_string(),
            attributes: HashMap::new(),
            description: None,
            kind: proto::SchemaKind::Managed,
            name_attribute: None,
            operation_config: None,
            validators: vec![proto::ValidatorType::TagsKeyValueCheck],
            exclusive_required: vec![],
            defs: Default::default(),
        };
        let core_schema = proto_schema_to_core(&proto_schema).unwrap();
        assert!(core_schema.validator.is_some());
    }

    #[test]
    fn test_proto_schema_without_validators_has_no_validator() {
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.s3.Bucket".to_string(),
            attributes: HashMap::new(),
            description: None,
            kind: proto::SchemaKind::Managed,
            name_attribute: None,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![],
            defs: Default::default(),
        };
        let core_schema = proto_schema_to_core(&proto_schema).unwrap();
        assert!(core_schema.validator.is_none());
    }

    #[test]
    fn test_exclusive_required_roundtrips_through_proto() {
        // Declarative exclusive_required must survive the proto boundary so
        // WASM providers can express `oneOf` constraints as data.
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.ec2.Vpc".to_string(),
            attributes: HashMap::new(),
            description: None,
            kind: proto::SchemaKind::Managed,
            name_attribute: None,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![vec![
                "cidr_block".to_string(),
                "ipv4_ipam_pool_id".to_string(),
            ]],
            defs: Default::default(),
        };
        let core_schema = proto_schema_to_core(&proto_schema).unwrap();
        assert_eq!(
            core_schema.exclusive_required,
            vec![vec![
                "cidr_block".to_string(),
                "ipv4_ipam_pool_id".to_string(),
            ]]
        );

        // And the resulting core schema rejects empty attributes.
        let err = core_schema.validate(&HashMap::new()).unwrap_err();
        assert!(
            err.iter().any(|e| e
                .to_string()
                .contains("Exactly one of [cidr_block, ipv4_ipam_pool_id]")),
            "expected missing-group error, got: {:?}",
            err
        );
    }

    #[test]
    fn test_exclusive_required_survives_json_roundtrip() {
        // The plugin host receives schemas as JSON. Confirm the new field
        // is preserved through full JSON round-trip (guest -> host).
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.ec2.Vpc".to_string(),
            attributes: HashMap::new(),
            description: None,
            kind: proto::SchemaKind::Managed,
            name_attribute: None,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![vec!["a".to_string(), "b".to_string()]],
            defs: Default::default(),
        };
        let json = serde_json::to_string(&vec![proto_schema]).unwrap();
        let schemas = json_to_schemas(&json).unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(
            schemas[0].exclusive_required,
            vec![vec!["a".to_string(), "b".to_string()]]
        );
    }

    /// carina#2831: a proto closed enum that carries `dsl_aliases`
    /// reaches the core schema with the alias list populated, so the
    /// host validator can accept the DSL spelling. Before this change
    /// `proto_attr_type_to_core` discarded the alias info because the
    /// equivalent `to_dsl` field was a non-serializable `fn` pointer.
    #[test]
    fn proto_enum_dsl_aliases_propagate_to_core() {
        let proto_attr = proto::AttributeType::StringEnum {
            name: "ObjectOwnership".to_string(),
            values: vec![
                "ObjectWriter".to_string(),
                "BucketOwnerEnforced".to_string(),
            ],
            namespace: None,
            dsl_aliases: vec![
                ("ObjectWriter".to_string(), "object_writer".to_string()),
                (
                    "BucketOwnerEnforced".to_string(),
                    "bucket_owner_enforced".to_string(),
                ),
            ],
        };
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::Enum { dsl_aliases, .. } => {
                assert_eq!(dsl_aliases.len(), 2);
                assert!(
                    dsl_aliases
                        .iter()
                        .any(|(a, d)| a == "BucketOwnerEnforced" && d == "bucket_owner_enforced"),
                    "dsl_aliases lost in proto -> core conversion: {dsl_aliases:?}"
                );
            }
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    /// Older provider components emit no `dsl_aliases` field; the
    /// proto deserializes it as an empty vec, which converts to a core
    /// `Enum` with an empty alias list. Validation behaves like
    /// the old `to_dsl: None` path: API-only spellings.
    #[test]
    fn proto_enum_without_aliases_yields_empty_core_aliases() {
        let json = r#"{"type":"string_enum","values":["A","B"],"name":"X"}"#;
        let proto_attr: proto::AttributeType = serde_json::from_str(json).unwrap();
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::Enum { dsl_aliases, .. } => {
                assert!(dsl_aliases.is_empty());
            }
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn proto_custom_enum_transform_lifts_state_string_to_canonical_enum() {
        let proto_attr = proto::AttributeType::CustomEnum {
            name: "ZoneName".to_string(),
            base: Box::new(proto_string()),
            namespace: "aws.AvailabilityZone".to_string(),
            dsl_transform: Some(proto::DslTransform::HyphenToUnderscore),
        };
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
        let state = carina_core::resource::Value::Concrete(
            carina_core::resource::ConcreteValue::String("ap-northeast-1a".to_string()),
        );

        let lifted = carina_core::utils::lift_enum_leaves(&state, &core_attr)
            .expect("WASM-bridged dynamic enum should lift provider state");

        match lifted {
            carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::CanonicalEnum(c),
            ) => {
                assert_eq!(c.identity().to_string(), "aws.AvailabilityZone.ZoneName");
                assert_eq!(c.api_value(), "ap-northeast-1a");
            }
            other => panic!("expected CanonicalEnum, got {other:?}"),
        }
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::Enum { base, .. } => {
                assert!(matches!(
                    base.shape_ref_free().expect("base is Ref-free"),
                    carina_core::schema::Shape::String { .. }
                ));
            }
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn proto_custom_enum_transform_does_not_lift_garbage_state_string() {
        let proto_attr = proto::AttributeType::CustomEnum {
            name: "ZoneName".to_string(),
            base: Box::new(proto_string()),
            namespace: "aws.AvailabilityZone".to_string(),
            dsl_transform: Some(proto::DslTransform::HyphenToUnderscore),
        };
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();

        let garbage = carina_core::resource::Value::Concrete(
            carina_core::resource::ConcreteValue::String("garbage-not-an-az".to_string()),
        );
        assert_eq!(
            carina_core::utils::lift_enum_leaves(&garbage, &core_attr),
            None,
            "unknown dynamic enum strings must remain String for strict validation"
        );

        let az_shaped_garbage = carina_core::resource::Value::Concrete(
            carina_core::resource::ConcreteValue::String("foo_bar_42".to_string()),
        );
        assert_eq!(
            carina_core::utils::lift_enum_leaves(&az_shaped_garbage, &core_attr),
            None,
            "already-DSL-looking unknown strings must stay String without data-form membership"
        );

        let already_dsl = carina_core::resource::Value::Concrete(
            carina_core::resource::ConcreteValue::enum_identifier("ap_northeast_1a".to_string()),
        );
        match carina_core::utils::lift_enum_leaves(&already_dsl, &core_attr) {
            Some(carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::CanonicalEnum(c),
            )) => {
                assert_eq!(c.identity().to_string(), "aws.AvailabilityZone.ZoneName");
                assert_eq!(c.api_value(), "ap-northeast-1a");
            }
            other => panic!(
                "parser-produced DSL enum identifiers must still canonicalize, got {other:?}"
            ),
        }

        for raw in ["   ", "", "AP-NORTHEAST-1A"] {
            let value = carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(raw.to_string()),
            );
            assert_eq!(
                carina_core::utils::lift_enum_leaves(&value, &core_attr),
                None,
                "{raw:?} must remain a String for strict validation"
            );
        }

        let valid = carina_core::resource::Value::Concrete(
            carina_core::resource::ConcreteValue::String("ap-northeast-1a".to_string()),
        );
        match carina_core::utils::lift_enum_leaves(&valid, &core_attr) {
            Some(carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::CanonicalEnum(c),
            )) => {
                assert_eq!(c.identity().to_string(), "aws.AvailabilityZone.ZoneName");
                assert_eq!(c.api_value(), "ap-northeast-1a");
            }
            other => {
                panic!(
                    "valid AZ strings must lift through the WASM-bridged transform, got {other:?}"
                )
            }
        }
    }

    #[test]
    fn proto_custom_enum_strip_suffix_transform_resolves_on_host() {
        let proto_attr = proto::AttributeType::CustomEnum {
            name: "ZoneName".to_string(),
            base: Box::new(proto_string()),
            namespace: "test.Dynamic".to_string(),
            dsl_transform: Some(proto::DslTransform::StripSuffix(".".to_string())),
        };
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::Enum {
                to_dsl: Some(transform),
                ..
            } => {
                assert_eq!(transform.apply("foo.bar."), "foo.bar");
            }
            other => panic!("expected Enum with transform, got {other:?}"),
        }
    }

    #[test]
    fn proto_custom_enum_unknown_transform_deserializes_and_uses_identity() {
        let json = r#"{
            "type":"custom_enum",
            "name":"ZoneName",
            "base":{"type":"String"},
            "namespace":"aws.AvailabilityZone",
            "dsl_transform":{"type":"FutureTransform"}
        }"#;
        let proto_attr: proto::AttributeType =
            serde_json::from_str(json).expect("future transform must deserialize");
        match &proto_attr {
            proto::AttributeType::CustomEnum { dsl_transform, .. } => {
                assert_eq!(
                    dsl_transform,
                    &Some(proto::DslTransform::Unknown(serde_json::json!({
                        "type": "FutureTransform"
                    })))
                );
            }
            other => panic!("expected proto CustomEnum, got {other:?}"),
        }

        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::Enum { to_dsl, .. } => {
                assert_eq!(
                    to_dsl,
                    Some(&proto::DslTransform::Unknown(serde_json::json!({
                        "type": "FutureTransform"
                    })))
                );
                assert_eq!(to_dsl.unwrap().apply("snake_to_kebab"), "snake_to_kebab");
            }
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn proto_custom_enum_none_transform_uses_no_transform() {
        let proto_attr = proto::AttributeType::CustomEnum {
            name: "ZoneName".to_string(),
            base: Box::new(proto_string()),
            namespace: "aws.AvailabilityZone".to_string(),
            dsl_transform: None,
        };
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::Enum { to_dsl, .. } => {
                assert!(to_dsl.is_none(), "None transform must remain no transform");
            }
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn proto_custom_pattern_and_length_propagate_to_core() {
        let proto_attr = proto::AttributeType::Custom {
            name: "awscc.wafv2.WebACL.EntityDescription".to_string(),
            base: Box::new(proto_string()),
            pattern: Some("^x$".to_string()),
            length: Some((Some(1), Some(9))),
            validate: None,
            to_dsl: None,
        };
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::String {
                identity,
                pattern,
                length,
                ..
            } => {
                assert_eq!(
                    identity.map(|id| id.kind.as_str()),
                    Some("EntityDescription")
                );
                assert_eq!(pattern, Some("^x$"));
                assert_eq!(length, Some((Some(1), Some(9))));
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn old_custom_string_payload_lifts_to_refined_core_string() {
        let json = r#"{
            "type":"custom",
            "name":"aws.route53.HostedZone.Name",
            "base":{"type":"String"},
            "pattern":"^[a-z.]+$",
            "length":[1,1024],
            "to_dsl":{"type":"StripSuffix","value":"."}
        }"#;
        let proto_attr: proto::AttributeType = serde_json::from_str(json).unwrap();
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();

        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::String {
                identity,
                pattern,
                length,
                to_dsl,
                ..
            } => {
                assert_eq!(identity.map(|id| id.kind.as_str()), Some("Name"));
                assert_eq!(pattern, Some("^[a-z.]+$"));
                assert_eq!(length, Some((Some(1), Some(1024))));
                assert_eq!(to_dsl.unwrap().apply("example.com."), "example.com");
            }
            other => panic!("expected refined String, got {other:?}"),
        }
    }

    #[test]
    fn old_custom_int_payload_lifts_length_to_refined_core_range() {
        let json = r#"{
            "type":"custom",
            "name":"aws.ec2.Port",
            "base":{"type":"Int"},
            "length":[0,65535]
        }"#;
        let proto_attr: proto::AttributeType = serde_json::from_str(json).unwrap();
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();

        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::Int {
                identity, range, ..
            } => {
                assert_eq!(identity.map(|id| id.kind.as_str()), Some("Port"));
                assert_eq!(range, Some((Some(0), Some(65535))));
            }
            other => panic!("expected refined Int, got {other:?}"),
        }
    }

    #[test]
    fn old_custom_float_payload_lifts_to_refined_core_float_identity() {
        let json = r#"{
            "type":"custom",
            "name":"awscc.wafv2.Size",
            "base":{"type":"Float"}
        }"#;
        let proto_attr: proto::AttributeType = serde_json::from_str(json).unwrap();
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();

        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::Float {
                identity, range, ..
            } => {
                assert_eq!(identity.map(|id| id.kind.as_str()), Some("Size"));
                assert!(range.is_none());
            }
            other => panic!("expected refined Float, got {other:?}"),
        }
    }

    #[test]
    fn legacy_nested_custom_resource_id_decodes_like_flat_custom() {
        let nested = proto_custom(
            "aws.ec2.VpcCidrBlockAssociation.Id",
            proto_custom("aws.ResourceId", proto_string(), None, None),
            None,
            None,
        );
        let flat = proto_custom(
            "aws.ec2.VpcCidrBlockAssociation.Id",
            proto_string(),
            None,
            None,
        );

        let nested_core = proto_attr_type_to_core(&nested).unwrap();
        let flat_core = proto_attr_type_to_core(&flat).unwrap();

        match (
            nested_core
                .shape_ref_free()
                .expect("test schema is Ref-free"),
            flat_core.shape_ref_free().expect("test schema is Ref-free"),
        ) {
            (
                carina_core::schema::Shape::String {
                    identity: nested_identity,
                    pattern: nested_pattern,
                    length: nested_length,
                    ..
                },
                carina_core::schema::Shape::String {
                    identity: flat_identity,
                    pattern: flat_pattern,
                    length: flat_length,
                    ..
                },
            ) => {
                assert_eq!(
                    nested_identity.map(|id| id.to_string()),
                    flat_identity.map(|id| id.to_string())
                );
                assert_eq!(nested_identity.map(|id| id.kind.as_str()), Some("Id"));
                assert_eq!(nested_pattern, flat_pattern);
                assert_eq!(nested_length, flat_length);
            }
            other => panic!("expected matching refined String shapes, got {other:?}"),
        }
    }

    #[test]
    fn legacy_nested_custom_outer_refinements_win_with_inner_fallback() {
        let nested = proto_custom(
            "aws.ec2.Vpc.Id",
            proto_custom(
                "aws.ResourceId",
                proto_string(),
                Some("^generic-"),
                Some((Some(1), Some(64))),
            ),
            Some("^vpc-"),
            Some((Some(4), Some(32))),
        );
        let core_attr = proto_attr_type_to_core(&nested).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::String {
                pattern, length, ..
            } => {
                assert_eq!(pattern, Some("^vpc-"));
                assert_eq!(length, Some((Some(4), Some(32))));
            }
            other => panic!("expected refined String, got {other:?}"),
        }

        let fallback = proto_custom(
            "aws.ec2.Vpc.Id",
            proto_custom(
                "aws.ResourceId",
                proto_string(),
                Some("^generic-"),
                Some((Some(1), Some(64))),
            ),
            None,
            None,
        );
        let core_attr = proto_attr_type_to_core(&fallback).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::String {
                pattern, length, ..
            } => {
                assert_eq!(pattern, Some("^generic-"));
                assert_eq!(length, Some((Some(1), Some(64))));
            }
            other => panic!("expected refined String, got {other:?}"),
        }
    }

    #[test]
    fn legacy_nested_custom_non_string_terminal_decodes_to_refined_int() {
        let proto_attr = proto_custom(
            "aws.ec2.Port",
            proto_custom(
                "aws.GenericPort",
                proto::AttributeType::Int {
                    range: None,
                    identity: None,
                },
                None,
                Some((Some(0), Some(65535))),
            ),
            None,
            None,
        );
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::Int {
                identity, range, ..
            } => {
                assert_eq!(identity.map(|id| id.kind.as_str()), Some("Port"));
                assert_eq!(range, Some((Some(0), Some(65535))));
            }
            other => panic!("expected refined Int, got {other:?}"),
        }
    }

    #[test]
    fn legacy_nested_custom_depth_three_resolves_terminal_primitive() {
        let proto_attr = proto_custom(
            "aws.ec2.Subnet.Id",
            proto_custom(
                "aws.ResourceId",
                proto_custom("aws.StringId", proto_string(), Some("^subnet-"), None),
                None,
                None,
            ),
            None,
            None,
        );
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::String {
                identity, pattern, ..
            } => {
                assert_eq!(identity.map(|id| id.kind.as_str()), Some("Id"));
                assert_eq!(pattern, Some("^subnet-"));
            }
            other => panic!("expected refined String, got {other:?}"),
        }
    }

    #[test]
    fn legacy_nested_custom_unsupported_terminal_returns_error() {
        let proto_attr = proto_custom(
            "aws.ec2.Unsupported.Id",
            proto_custom("aws.ResourceId", proto::AttributeType::Bool, None, None),
            None,
            None,
        );
        let err = proto_attr_type_to_core(&proto_attr).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("aws.ec2.Unsupported.Id"), "{msg}");
        assert!(msg.contains("Bool"), "{msg}");
    }

    #[test]
    fn json_to_schemas_decodes_real_nested_custom_wire_shape() {
        let json = r#"[
            {
                "resource_type":"aws.ec2.VpcCidrBlockAssociation",
                "attributes":{
                    "id":{
                        "name":"id",
                        "attr_type":{
                            "type":"custom",
                            "name":"aws.ec2.VpcCidrBlockAssociation.Id",
                            "base":{
                                "type":"custom",
                                "name":"aws.ResourceId",
                                "base":{"type":"String"}
                            }
                        },
                        "required":false
                    }
                }
            }
        ]"#;

        let schemas = json_to_schemas(json).unwrap();
        let attr_type = &schemas[0].attributes["id"].attr_type;
        match attr_type.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::String { identity, .. } => {
                assert_eq!(
                    identity.map(|id| id.to_string()),
                    Some("aws.ec2.VpcCidrBlockAssociation.Id".to_string())
                );
            }
            other => panic!("expected refined String, got {other:?}"),
        }
    }

    #[test]
    fn old_custom_list_payload_lifts_to_refined_core_list() {
        let json = r#"{
            "type":"custom",
            "name":"awscc.wafv2.StatementList",
            "base":{
                "type":"list",
                "inner":{"type":"String"},
                "ordered":false
            },
            "length":[1,5],
            "validate":true
        }"#;
        let proto_attr: proto::AttributeType = serde_json::from_str(json).unwrap();
        let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();

        match core_attr.shape_ref_free().expect("test schema is Ref-free") {
            carina_core::schema::Shape::List {
                element_type,
                ordered,
                length,
                ..
            } => {
                assert!(!ordered);
                assert_eq!(length, Some((Some(1), Some(5))));
                assert!(matches!(
                    element_type.shape_ref_free().expect("element is Ref-free"),
                    carina_core::schema::Shape::String { .. }
                ));
            }
            other => panic!("expected refined List, got {other:?}"),
        }
    }

    #[test]
    fn new_primitive_payloads_lift_to_refined_core_shapes() {
        let cases = [
            r#"{
                "type":"String",
                "identity":"aws.route53.HostedZone.Name",
                "pattern":"^[a-z.]+$",
                "length":[1,1024],
                "to_dsl":{"type":"StripSuffix","value":"."}
            }"#,
            r#"{"type":"Int","identity":"aws.ec2.Port","range":[-1,65535]}"#,
            r#"{"type":"Float","identity":"awscc.wafv2.Size","range":[0.0,1.0]}"#,
            r#"{
                "type":"list",
                "element_type":{"type":"String"},
                "ordered":true,
                "length":[1,3],
                "validate":true
            }"#,
        ];

        for json in cases {
            let proto_attr: proto::AttributeType = serde_json::from_str(json).unwrap();
            let core_attr = proto_attr_type_to_core(&proto_attr).unwrap();
            match core_attr.shape_ref_free().expect("test schema is Ref-free") {
                carina_core::schema::Shape::String {
                    identity,
                    pattern,
                    length,
                    to_dsl,
                    ..
                } => {
                    assert_eq!(identity.map(|id| id.kind.as_str()), Some("Name"));
                    assert_eq!(pattern, Some("^[a-z.]+$"));
                    assert_eq!(length, Some((Some(1), Some(1024))));
                    assert_eq!(to_dsl.unwrap().apply("example.com."), "example.com");
                }
                carina_core::schema::Shape::Int {
                    identity, range, ..
                } => {
                    assert_eq!(identity.map(|id| id.kind.as_str()), Some("Port"));
                    assert_eq!(range, Some((Some(-1), Some(65535))));
                }
                carina_core::schema::Shape::Float {
                    identity, range, ..
                } => {
                    assert_eq!(identity.map(|id| id.kind.as_str()), Some("Size"));
                    assert_eq!(range, Some((Some(0.0), Some(1.0))));
                }
                carina_core::schema::Shape::List {
                    ordered, length, ..
                } => {
                    assert!(ordered);
                    assert_eq!(length, Some((Some(1), Some(3))));
                }
                other => panic!("unexpected lifted shape: {other:?}"),
            }
        }
    }

    #[test]
    fn test_tags_validator_detects_key_value_pattern() {
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.s3.Bucket".to_string(),
            attributes: HashMap::new(),
            description: None,
            kind: proto::SchemaKind::Managed,
            name_attribute: None,
            operation_config: None,
            validators: vec![proto::ValidatorType::TagsKeyValueCheck],
            exclusive_required: vec![],
            defs: Default::default(),
        };
        let core_schema = proto_schema_to_core(&proto_schema).unwrap();
        let validator = core_schema.validator.unwrap();

        // key/value pattern should fail
        let mut attrs = HashMap::new();
        attrs.insert(
            "tags".to_string(),
            CoreValue::Concrete(ConcreteValue::Map(
                [
                    (
                        "key".to_string(),
                        CoreValue::Concrete(ConcreteValue::String("Project".into())),
                    ),
                    (
                        "value".to_string(),
                        CoreValue::Concrete(ConcreteValue::String("carina".into())),
                    ),
                ]
                .into_iter()
                .collect(),
            )),
        );
        assert!(validator(&attrs).is_err());

        // normal tags should pass
        let mut attrs = HashMap::new();
        attrs.insert(
            "tags".to_string(),
            CoreValue::Concrete(ConcreteValue::Map(
                [(
                    "Project".to_string(),
                    CoreValue::Concrete(ConcreteValue::String("carina".into())),
                )]
                .into_iter()
                .collect(),
            )),
        );
        assert!(validator(&attrs).is_ok());
    }
}
