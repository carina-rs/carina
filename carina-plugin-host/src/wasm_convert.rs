//! Conversions between carina-core types and Wasmtime-generated WIT types.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use carina_core::effect::PlanOp as CorePlanOp;
use carina_core::provider::{
    AppliedState as CoreAppliedState, CreateRequest as CoreCreateRequest,
    DeleteRequest as CoreDeleteRequest, EmptyAppliedIdentifier,
    PartialReadDiagnostic as CorePartialReadDiagnostic, PatchOp as CorePatchOp,
    PendingCreate as CorePendingCreate, ProviderError as CoreProviderError,
    ReadOutcome as CoreReadOutcome, ReadRequest as CoreReadRequest, SavedAttrs as CoreSavedAttrs,
    UpdateOutcome as CoreUpdateOutcome, UpdatePatch as CoreUpdatePatch,
    UpdateRequest as CoreUpdateRequest,
};
use carina_core::resource::{
    ConcreteValue, DataSource as CoreDataSource, DeferredValue, Directives, RedactedSecretHash,
    Resource as CoreResource, ResourceId as CoreResourceId, ResourceIdentityState,
    State as CoreState, Value as CoreValue,
};
#[cfg(test)]
use carina_core::schema::{
    AttributeSchema as CoreAttributeSchema, AttributeSchemaParts as CoreAttributeSchemaParts,
    AttributeType as CoreAttributeType, InputMode as CoreInputMode,
    ResourceSchemaParts as CoreResourceSchemaParts, SchemaKind as CoreSchemaKind,
    SchemaRegistry as CoreSchemaRegistry, StructField as CoreStructField,
    StructFieldParts as CoreStructFieldParts, UniqueNameSpec as CoreUniqueNameSpec,
};
use carina_core::schema::{
    ValidatedConfigSchema as CoreValidatedConfigSchema,
    ValidatedResourceSchema as CoreValidatedResourceSchema,
};
use carina_core::value::{PERSISTED_SECRET_KEY, SerializationContext, SerializationError};
use carina_core::wait::BindingPattern as CoreBindingPattern;
use carina_core::wait::predicate::{AttrPath as CoreAttrPath, AttrPathError};

use carina_provider_protocol::types as proto;

use crate::wasm_factory::version_checked_bindings::{WitPlanOp, wit_types as wit};

/// Error raised when provider-emitted schema wire data cannot be decoded.
///
/// External provider metadata is version-gated but still untrusted at this
/// boundary; carina#3459 makes unsupported wire shapes explicit errors instead
/// of panics so callers can attach provider context.
#[derive(Debug)]
pub enum SchemaDecodeError {
    JsonParse(serde_json::Error),
    Protocol(carina_core::protocol_convert::SchemaDecodeError),
}

impl SchemaDecodeError {
    fn schema_json_parse(err: serde_json::Error) -> Self {
        Self::JsonParse(err)
    }

    fn protocol_conversion(err: carina_core::protocol_convert::SchemaDecodeError) -> Self {
        Self::Protocol(err)
    }
}

impl fmt::Display for SchemaDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JsonParse(_) => f.write_str("schema JSON parse error"),
            Self::Protocol(_) => f.write_str("schema protocol conversion failed"),
        }
    }
}

impl std::error::Error for SchemaDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::JsonParse(source) => Some(source),
            Self::Protocol(source) => Some(source),
        }
    }
}

/// Failure to decode a JSON-backed recursive value at the WIT boundary.
///
/// The payload itself is deliberately not retained: `secret-val` may contain
/// plaintext credentials. JSON syntax failures retain the structured serde
/// error as typed data and render its diagnostic once without exposing the
/// input.
#[derive(Debug)]
pub struct WasmValueDecodeError {
    wit_variant: WitValueVariant,
    attribute_path: Vec<String>,
    kind: WasmValueDecodeErrorKind,
}

#[derive(Debug, Clone, Copy)]
enum WitValueVariant {
    StringList,
    List,
    Map,
    Secret,
    RedactedSecret,
}

impl fmt::Display for WitValueVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StringList => "string-list-val",
            Self::List => "list-val",
            Self::Map => "map-val",
            Self::Secret => "secret-val",
            Self::RedactedSecret => "redacted-secret-val",
        })
    }
}

#[derive(Debug)]
enum WasmValueDecodeErrorKind {
    InvalidJson(serde_json::Error),
    ExpectedArray,
    ExpectedObject,
    ExpectedString { index: usize },
    Null,
    UnsupportedNumber,
    MalformedPersistedSecret,
    ReservedPersistedKey,
    InvalidRedactedSecretDigest,
}

impl WasmValueDecodeError {
    fn new(wit_variant: WitValueVariant, kind: WasmValueDecodeErrorKind) -> Self {
        Self {
            wit_variant,
            attribute_path: Vec::new(),
            kind,
        }
    }

    fn prepend_attribute(mut self, name: &str) -> Self {
        self.attribute_path.insert(0, name.to_string());
        self
    }
}

impl fmt::Display for WasmValueDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to decode WIT {}", self.wit_variant)?;
        if !self.attribute_path.is_empty() {
            write!(f, " attribute '{}'", self.attribute_path.join("."))?;
        }
        match &self.kind {
            WasmValueDecodeErrorKind::InvalidJson(error) => {
                write!(f, ": invalid JSON: {error}")
            }
            WasmValueDecodeErrorKind::ExpectedArray => f.write_str(": expected a JSON array"),
            WasmValueDecodeErrorKind::ExpectedObject => f.write_str(": expected a JSON object"),
            WasmValueDecodeErrorKind::ExpectedString { index } => {
                write!(f, ": expected a JSON string at array index {index}")
            }
            WasmValueDecodeErrorKind::Null => {
                f.write_str(": JSON null is not supported by the value protocol")
            }
            WasmValueDecodeErrorKind::UnsupportedNumber => {
                f.write_str(": JSON number cannot be represented by the value protocol")
            }
            WasmValueDecodeErrorKind::MalformedPersistedSecret => {
                f.write_str(": malformed persisted redacted-secret tag")
            }
            WasmValueDecodeErrorKind::ReservedPersistedKey => {
                f.write_str(": unescaped reserved persisted-state object key")
            }
            WasmValueDecodeErrorKind::InvalidRedactedSecretDigest => {
                f.write_str(": invalid redacted-secret digest")
            }
        }
    }
}

impl std::error::Error for WasmValueDecodeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppliedStateOperation {
    Read,
    Create,
    Update,
}

impl fmt::Display for AppliedStateOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Create => "create",
            Self::Update => "update",
        })
    }
}

#[derive(Debug)]
enum WasmAppliedStateDecodeErrorKind {
    Attributes(Box<WasmValueDecodeError>),
    Identifier(EmptyAppliedIdentifier),
}

/// Failure to decode an existing-resource observation returned across the WIT
/// boundary.
///
/// The operation and request resource ID are host-owned context. This is a
/// terminal in-repo error: its display contains the complete diagnostic while
/// `kind` preserves structured classification for tests and callers.
#[derive(Debug)]
pub(crate) struct WasmAppliedStateDecodeError {
    operation: AppliedStateOperation,
    resource_id: Box<CoreResourceId>,
    kind: WasmAppliedStateDecodeErrorKind,
}

impl WasmAppliedStateDecodeError {
    fn attributes(
        operation: AppliedStateOperation,
        resource_id: &CoreResourceId,
        source: WasmValueDecodeError,
    ) -> Self {
        Self {
            operation,
            resource_id: Box::new(resource_id.clone()),
            kind: WasmAppliedStateDecodeErrorKind::Attributes(Box::new(source)),
        }
    }

    fn identifier(
        operation: AppliedStateOperation,
        resource_id: &CoreResourceId,
        source: EmptyAppliedIdentifier,
    ) -> Self {
        Self {
            operation,
            resource_id: Box::new(resource_id.clone()),
            kind: WasmAppliedStateDecodeErrorKind::Identifier(source),
        }
    }
}

impl fmt::Display for WasmAppliedStateDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            WasmAppliedStateDecodeErrorKind::Attributes(error) => write!(
                f,
                "failed to decode WIT {} outcome applied state attributes for resource '{}': {error}",
                self.operation, self.resource_id
            ),
            WasmAppliedStateDecodeErrorKind::Identifier(error) => write!(
                f,
                "failed to decode WIT {} outcome applied state identifier for resource '{}': {error}",
                self.operation, self.resource_id
            ),
        }
    }
}

impl std::error::Error for WasmAppliedStateDecodeError {}

// -- Value --

/// Convert a core Value to a WIT Value.
///
/// List and Map values are serialized to JSON strings because WIT does
/// not support recursive types.
///
/// Returns `Err(SerializationError)` for an unresolved `EnumIdentifier` and
/// for `Value::Deferred(DeferredValue::Unknown)`,
/// `Value::Deferred(DeferredValue::ResourceRef)`, `Value::Deferred(DeferredValue::Interpolation)`, and
/// `Value::Deferred(DeferredValue::FunctionCall)` / `Unavailable`. The scoped transport wrapper in
/// `WasmProviderNormalizer::normalize_desired` strips ref/Unknown-bearing
/// attributes before conversion and restores them after the guest call, so
/// reaching any of these arms is a producer-side bug — the diagnostic
/// identifies which variant leaked. See RFC #2371 stage 2 (#2378) for
/// `Unknown` and #2387 for `ResourceRef` / `Interpolation` / `FunctionCall`.
///
/// `Value::Deferred(DeferredValue::Secret(inner))` is encoded as the `secret-val` WIT variant
/// (#2390): the inner is JSON-encoded the same way `list-val` / `map-val`
/// encode their contents, and a typed signal crosses the boundary so the
/// WASM provider can distinguish a secret from a plain string. The
/// previous `format!("{v:?}")` debug-format fallback would send
/// `"Secret(String(\"…\"))"` literally to the provider — a contract leak
/// equivalent to the pre-#2387 `ResourceRef` debug-string.
pub(crate) fn core_to_wit_value(v: &CoreValue) -> Result<wit::Value, SerializationError> {
    match v {
        CoreValue::Concrete(ConcreteValue::String(s)) => {
            Ok(wit::Value::StrVal(s.expose_plaintext().to_owned()))
        }
        CoreValue::Concrete(ConcreteValue::EnumIdentifier(value)) => {
            Err(SerializationError::UnresolvedEnumIdentifier {
                value: value.to_string(),
                context: SerializationContext::WasmBoundary,
            })
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
            let json_str =
                serde_json::to_string(items).expect("Vec<String> -> String is infallible");
            Ok(wit::Value::StringListVal(json_str))
        }
        CoreValue::Concrete(ConcreteValue::Map(map)) => {
            let json_map: Result<serde_json::Map<String, serde_json::Value>, _> = map
                .iter()
                .map(|(key, value)| core_value_to_json(value).map(|value| (key.clone(), value)))
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
        CoreValue::Deferred(DeferredValue::Coalesce { .. }) => {
            Err(SerializationError::UnresolvedCoalesce {
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::Interpolation(_)) => {
            Err(SerializationError::UnresolvedInterpolation {
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(
            DeferredValue::FunctionCall { name, .. } | DeferredValue::Unavailable { name, .. },
        ) => Err(SerializationError::UnresolvedFunctionCall {
            name: name.to_string(),
            context: SerializationContext::WasmBoundary,
        }),
        CoreValue::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) => {
            Err(SerializationError::UnresolvedFunctionCall {
                name: function.name.clone(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::Access(_)) => {
            Err(SerializationError::UnresolvedAccess {
                expression: carina_core::value::format_value(v),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::Secret(inner)) => {
            let json = core_value_to_json(inner).map_err(SerializationError::masked_for_secret)?;
            let json_str =
                serde_json::to_string(&json).expect("serde_json::Value -> String is infallible");
            Ok(wit::Value::SecretVal(json_str))
        }
        CoreValue::Deferred(DeferredValue::RedactedSecret { .. }) => {
            Err(SerializationError::RedactedSecretNotAllowed {
                context: SerializationContext::WasmBoundary,
            })
        }
    }
}

fn persisted_json_key(key: &str) -> String {
    if key.starts_with('$') {
        format!("${key}")
    } else {
        key.to_string()
    }
}

fn ordinary_wit_value_into_persisted(
    value: wit::Value,
) -> Result<wit::PersistedValue, SerializationError> {
    Ok(match value {
        wit::Value::BoolVal(value) => wit::PersistedValue::BoolVal(value),
        wit::Value::IntVal(value) => wit::PersistedValue::IntVal(value),
        wit::Value::FloatVal(value) => wit::PersistedValue::FloatVal(value),
        wit::Value::StrVal(value) => wit::PersistedValue::StrVal(value),
        wit::Value::StringListVal(value) => wit::PersistedValue::StringListVal(value),
        wit::Value::ListVal(value) => wit::PersistedValue::ListVal(value),
        wit::Value::MapVal(value) => wit::PersistedValue::MapVal(value),
        wit::Value::SecretVal(_) => {
            return Err(SerializationError::SecretNotAllowed {
                context: SerializationContext::WasmBoundary,
            });
        }
    })
}

/// Convert a core value at a persisted-state position to its dedicated WIT
/// type. Canonical enums still cross as API strings because WIT has no enum
/// identity variant.
fn core_to_wit_persisted_value(
    value: &CoreValue,
) -> Result<wit::PersistedValue, SerializationError> {
    match value {
        CoreValue::Concrete(ConcreteValue::List(items)) => {
            let json = items
                .iter()
                .map(core_persisted_value_to_json)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(wit::PersistedValue::ListVal(
                serde_json::to_string(&json).expect("serde_json::Value -> String is infallible"),
            ))
        }
        CoreValue::Concrete(ConcreteValue::Map(map)) => {
            let json = map
                .iter()
                .map(|(key, value)| {
                    core_persisted_value_to_json(value)
                        .map(|value| (persisted_json_key(key), value))
                })
                .collect::<Result<serde_json::Map<_, _>, _>>()?;
            Ok(wit::PersistedValue::MapVal(
                serde_json::to_string(&json).expect("serde_json::Value -> String is infallible"),
            ))
        }
        CoreValue::Deferred(DeferredValue::Secret(_)) => {
            Err(SerializationError::SecretNotAllowed {
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::RedactedSecret { hash }) => Ok(
            wit::PersistedValue::RedactedSecretVal(hash.encoded_digest()),
        ),
        _ => core_to_wit_value(value).and_then(ordinary_wit_value_into_persisted),
    }
}

/// Convert a WIT Value to a core Value.
pub(crate) fn wit_to_core_value(v: &wit::Value) -> Result<CoreValue, WasmValueDecodeError> {
    match v {
        wit::Value::StrVal(s) => Ok(CoreValue::Concrete(ConcreteValue::String(s.clone().into()))),
        wit::Value::IntVal(i) => Ok(CoreValue::Concrete(ConcreteValue::Int(*i))),
        wit::Value::FloatVal(f) => Ok(CoreValue::Concrete(ConcreteValue::Float(*f))),
        wit::Value::BoolVal(b) => Ok(CoreValue::Concrete(ConcreteValue::Bool(*b))),
        wit::Value::StringListVal(json) => {
            let value: serde_json::Value = serde_json::from_str(json).map_err(|source| {
                WasmValueDecodeError::new(
                    WitValueVariant::StringList,
                    WasmValueDecodeErrorKind::InvalidJson(source),
                )
            })?;
            let serde_json::Value::Array(items) = value else {
                return Err(WasmValueDecodeError::new(
                    WitValueVariant::StringList,
                    WasmValueDecodeErrorKind::ExpectedArray,
                ));
            };
            let items = items
                .into_iter()
                .enumerate()
                .map(|(index, item)| match item {
                    serde_json::Value::String(item) => Ok(item.into()),
                    _ => Err(WasmValueDecodeError::new(
                        WitValueVariant::StringList,
                        WasmValueDecodeErrorKind::ExpectedString { index },
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CoreValue::Concrete(ConcreteValue::StringList(items)))
        }
        wit::Value::ListVal(json) => {
            let value: serde_json::Value = serde_json::from_str(json).map_err(|source| {
                WasmValueDecodeError::new(
                    WitValueVariant::List,
                    WasmValueDecodeErrorKind::InvalidJson(source),
                )
            })?;
            if !value.is_array() {
                return Err(WasmValueDecodeError::new(
                    WitValueVariant::List,
                    WasmValueDecodeErrorKind::ExpectedArray,
                ));
            }
            json_to_core_value(&value, WitValueVariant::List)
        }
        wit::Value::MapVal(json) => {
            let value: serde_json::Value = serde_json::from_str(json).map_err(|source| {
                WasmValueDecodeError::new(
                    WitValueVariant::Map,
                    WasmValueDecodeErrorKind::InvalidJson(source),
                )
            })?;
            if !value.is_object() {
                return Err(WasmValueDecodeError::new(
                    WitValueVariant::Map,
                    WasmValueDecodeErrorKind::ExpectedObject,
                ));
            }
            json_to_core_value(&value, WitValueVariant::Map)
        }
        wit::Value::SecretVal(json) => {
            // Decode the JSON-encoded inner value the same way `ListVal` /
            // `MapVal` decode theirs, then re-wrap in `Secret` so the host's
            // secret-tracking machinery (state hashing, plan redaction) keeps
            // working. Malformed input is a boundary error, never a value.
            let inner_json: serde_json::Value = serde_json::from_str(json).map_err(|source| {
                WasmValueDecodeError::new(
                    WitValueVariant::Secret,
                    WasmValueDecodeErrorKind::InvalidJson(source),
                )
            })?;
            let inner = json_to_core_value(&inner_json, WitValueVariant::Secret)?;
            Ok(CoreValue::Deferred(DeferredValue::Secret(Box::new(inner))))
        }
    }
}

fn wit_to_core_persisted_value(
    value: &wit::PersistedValue,
) -> Result<CoreValue, WasmValueDecodeError> {
    match value {
        wit::PersistedValue::RedactedSecretVal(digest) => {
            RedactedSecretHash::from_encoded_digest(digest)
                .map(|hash| CoreValue::Deferred(DeferredValue::RedactedSecret { hash }))
                .ok_or_else(|| {
                    WasmValueDecodeError::new(
                        WitValueVariant::RedactedSecret,
                        WasmValueDecodeErrorKind::InvalidRedactedSecretDigest,
                    )
                })
        }
        wit::PersistedValue::ListVal(json) => {
            let value: serde_json::Value = serde_json::from_str(json).map_err(|source| {
                WasmValueDecodeError::new(
                    WitValueVariant::List,
                    WasmValueDecodeErrorKind::InvalidJson(source),
                )
            })?;
            if !value.is_array() {
                return Err(WasmValueDecodeError::new(
                    WitValueVariant::List,
                    WasmValueDecodeErrorKind::ExpectedArray,
                ));
            }
            persisted_json_to_core_value(&value, WitValueVariant::List)
        }
        wit::PersistedValue::MapVal(json) => {
            let value: serde_json::Value = serde_json::from_str(json).map_err(|source| {
                WasmValueDecodeError::new(
                    WitValueVariant::Map,
                    WasmValueDecodeErrorKind::InvalidJson(source),
                )
            })?;
            if !value.is_object() {
                return Err(WasmValueDecodeError::new(
                    WitValueVariant::Map,
                    WasmValueDecodeErrorKind::ExpectedObject,
                ));
            }
            persisted_json_to_core_value(&value, WitValueVariant::Map)
        }
        wit::PersistedValue::BoolVal(value) => Ok(CoreValue::Concrete(ConcreteValue::Bool(*value))),
        wit::PersistedValue::IntVal(value) => Ok(CoreValue::Concrete(ConcreteValue::Int(*value))),
        wit::PersistedValue::FloatVal(value) => {
            Ok(CoreValue::Concrete(ConcreteValue::Float(*value)))
        }
        wit::PersistedValue::StrVal(value) => Ok(CoreValue::Concrete(ConcreteValue::String(
            value.clone().into(),
        ))),
        wit::PersistedValue::StringListVal(json) => {
            let ordinary = wit::Value::StringListVal(json.clone());
            wit_to_core_value(&ordinary)
        }
    }
}

pub(crate) fn core_to_wit_plan_op(op: CorePlanOp) -> WitPlanOp {
    match op {
        CorePlanOp::Create => WitPlanOp::Create,
        CorePlanOp::Read => WitPlanOp::Read,
        CorePlanOp::Update => WitPlanOp::Update,
        CorePlanOp::Delete => WitPlanOp::Delete,
    }
}

#[cfg(test)]
pub(crate) fn core_to_wit_binding_pattern(p: &CoreBindingPattern) -> wit::BindingPattern {
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

pub(crate) fn wit_to_core_binding_pattern(
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
/// that function's doc for the rationale and the scoped strip-and-restore
/// wrapper in `WasmProviderNormalizer::normalize_desired` that keeps these
/// arms unreachable in legitimate desired-normalization flows.
fn core_value_to_json(v: &CoreValue) -> Result<serde_json::Value, SerializationError> {
    match v {
        CoreValue::Concrete(ConcreteValue::String(s)) => {
            Ok(serde_json::Value::String(s.expose_plaintext().to_owned()))
        }
        CoreValue::Concrete(ConcreteValue::EnumIdentifier(value)) => {
            Err(SerializationError::UnresolvedEnumIdentifier {
                value: value.to_string(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Concrete(ConcreteValue::CanonicalEnum(c)) => {
            Ok(serde_json::Value::String(c.api_value().to_string()))
        }
        CoreValue::Concrete(ConcreteValue::Int(i)) => Ok(serde_json::Value::Number((*i).into())),
        CoreValue::Concrete(ConcreteValue::Float(f)) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .ok_or(SerializationError::NonFiniteFloat {
                value: *f,
                context: SerializationContext::WasmBoundary,
            }),
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
                .map(|s| serde_json::Value::String(s.expose_plaintext().to_owned()))
                .collect(),
        )),
        CoreValue::Concrete(ConcreteValue::Map(map)) => {
            let obj: Result<serde_json::Map<String, serde_json::Value>, _> = map
                .iter()
                .map(|(key, value)| core_value_to_json(value).map(|value| (key.clone(), value)))
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
        CoreValue::Deferred(DeferredValue::Coalesce { .. }) => {
            Err(SerializationError::UnresolvedCoalesce {
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::Interpolation(_)) => {
            Err(SerializationError::UnresolvedInterpolation {
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(
            DeferredValue::FunctionCall { name, .. } | DeferredValue::Unavailable { name, .. },
        ) => Err(SerializationError::UnresolvedFunctionCall {
            name: name.to_string(),
            context: SerializationContext::WasmBoundary,
        }),
        CoreValue::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) => {
            Err(SerializationError::UnresolvedFunctionCall {
                name: function.name.clone(),
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::Access(_)) => {
            Err(SerializationError::UnresolvedAccess {
                expression: carina_core::value::format_value(v),
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
        CoreValue::Deferred(DeferredValue::Secret(inner)) => {
            core_value_to_json(inner).map_err(SerializationError::masked_for_secret)
        }
        CoreValue::Deferred(DeferredValue::RedactedSecret { .. }) => {
            Err(SerializationError::RedactedSecretNotAllowed {
                context: SerializationContext::WasmBoundary,
            })
        }
    }
}

fn core_persisted_value_to_json(
    value: &CoreValue,
) -> Result<serde_json::Value, SerializationError> {
    match value {
        CoreValue::Concrete(ConcreteValue::List(items)) => items
            .iter()
            .map(core_persisted_value_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        CoreValue::Concrete(ConcreteValue::Map(map)) => map
            .iter()
            .map(|(key, value)| {
                core_persisted_value_to_json(value).map(|value| (persisted_json_key(key), value))
            })
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(serde_json::Value::Object),
        CoreValue::Deferred(DeferredValue::Secret(_)) => {
            Err(SerializationError::SecretNotAllowed {
                context: SerializationContext::WasmBoundary,
            })
        }
        CoreValue::Deferred(DeferredValue::RedactedSecret { hash }) => {
            Ok(serde_json::json!({ PERSISTED_SECRET_KEY: hash.encoded_digest() }))
        }
        _ => core_value_to_json(value),
    }
}

/// Helper: convert a serde_json::Value to a core Value.
fn json_to_core_value(
    v: &serde_json::Value,
    wit_variant: WitValueVariant,
) -> Result<CoreValue, WasmValueDecodeError> {
    match v {
        serde_json::Value::String(s) => {
            Ok(CoreValue::Concrete(ConcreteValue::String(s.clone().into())))
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(CoreValue::Concrete(ConcreteValue::Int(i)))
            } else if let Some(f) = n.as_f64() {
                Ok(CoreValue::Concrete(ConcreteValue::Float(f)))
            } else {
                Err(WasmValueDecodeError::new(
                    wit_variant,
                    WasmValueDecodeErrorKind::UnsupportedNumber,
                ))
            }
        }
        serde_json::Value::Bool(b) => Ok(CoreValue::Concrete(ConcreteValue::Bool(*b))),
        serde_json::Value::Array(items) => {
            let items = items
                .iter()
                .map(|item| json_to_core_value(item, wit_variant))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CoreValue::Concrete(ConcreteValue::List(items)))
        }
        serde_json::Value::Object(map) => {
            let map = map
                .iter()
                .map(|(key, value)| {
                    json_to_core_value(value, wit_variant)
                        .map(|value| (key.clone(), value))
                        .map_err(|error| error.prepend_attribute(key))
                })
                .collect::<Result<indexmap::IndexMap<_, _>, _>>()?;
            Ok(CoreValue::Concrete(ConcreteValue::Map(map)))
        }
        serde_json::Value::Null => Err(WasmValueDecodeError::new(
            wit_variant,
            WasmValueDecodeErrorKind::Null,
        )),
    }
}

fn persisted_json_to_core_value(
    value: &serde_json::Value,
    wit_variant: WitValueVariant,
) -> Result<CoreValue, WasmValueDecodeError> {
    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| persisted_json_to_core_value(value, wit_variant))
            .collect::<Result<Vec<_>, _>>()
            .map(ConcreteValue::List)
            .map(CoreValue::Concrete),
        serde_json::Value::Object(values) => {
            if values.contains_key(PERSISTED_SECRET_KEY) {
                if values.len() != 1 {
                    return Err(WasmValueDecodeError::new(
                        wit_variant,
                        WasmValueDecodeErrorKind::MalformedPersistedSecret,
                    ));
                }
                let Some(serde_json::Value::String(digest)) = values.get(PERSISTED_SECRET_KEY)
                else {
                    return Err(WasmValueDecodeError::new(
                        wit_variant,
                        WasmValueDecodeErrorKind::MalformedPersistedSecret,
                    ));
                };
                return RedactedSecretHash::from_encoded_digest(digest)
                    .map(|hash| CoreValue::Deferred(DeferredValue::RedactedSecret { hash }))
                    .ok_or_else(|| {
                        WasmValueDecodeError::new(
                            wit_variant,
                            WasmValueDecodeErrorKind::InvalidRedactedSecretDigest,
                        )
                    });
            }
            values
                .iter()
                .map(|(key, value)| {
                    let decoded_key = if key.starts_with("$$") {
                        key[1..].to_string()
                    } else if key.starts_with('$') {
                        return Err(WasmValueDecodeError::new(
                            wit_variant,
                            WasmValueDecodeErrorKind::ReservedPersistedKey,
                        )
                        .prepend_attribute(key));
                    } else {
                        key.clone()
                    };
                    persisted_json_to_core_value(value, wit_variant)
                        .map(|value| (decoded_key, value))
                        .map_err(|error| error.prepend_attribute(key))
                })
                .collect::<Result<indexmap::IndexMap<_, _>, _>>()
                .map(ConcreteValue::Map)
                .map(CoreValue::Concrete)
        }
        _ => json_to_core_value(value, wit_variant),
    }
}

// -- Value map helpers --

pub(crate) fn core_to_wit_value_map<'a, M>(
    map: M,
) -> Result<Vec<(String, wit::Value)>, SerializationError>
where
    M: IntoIterator<Item = (&'a String, &'a CoreValue)>,
{
    map.into_iter()
        .map(|(k, v)| core_to_wit_value(v).map(|wv| (k.clone(), wv)))
        .collect()
}

fn core_to_wit_persisted_value_map<'a, M>(
    map: M,
) -> Result<Vec<(String, wit::PersistedValue)>, SerializationError>
where
    M: IntoIterator<Item = (&'a String, &'a CoreValue)>,
{
    map.into_iter()
        .map(|(key, value)| core_to_wit_persisted_value(value).map(|value| (key.clone(), value)))
        .collect()
}

/// Encode one saved-attribute entry through the persisted-state wire policy.
///
/// `CoreSavedAttrs` can only be obtained from opaque `PersistedAttributes`
/// views returned by state rows and then passed through
/// `RawSavedAttrs::from_persisted(...).lift(...)`; desired attribute maps have
/// no conversion into that provenance type.
pub(crate) fn core_saved_attrs_to_wit(
    saved_attrs: &CoreSavedAttrs,
    id: &CoreResourceId,
) -> Result<Option<Vec<(String, wit::PersistedValue)>>, SerializationError> {
    saved_attrs
        .get(id)
        .map(core_to_wit_persisted_value_map)
        .transpose()
}

pub(crate) fn wit_to_core_value_map(
    entries: &[(String, wit::Value)],
) -> Result<HashMap<String, CoreValue>, WasmValueDecodeError> {
    entries
        .iter()
        .map(|(key, value)| {
            wit_to_core_value(value)
                .map(|value| (key.clone(), value))
                .map_err(|error| error.prepend_attribute(key))
        })
        .collect()
}

/// Decode attributes returned from `normalize_state`, `hydrate_read_state`,
/// or `update` using the structural persisted-state wire policy. Ordinary
/// provider `create`, `read`, and data-source responses use
/// [`wit_to_core_value_map`].
pub(crate) fn wit_to_core_persisted_value_map(
    entries: &[(String, wit::PersistedValue)],
) -> Result<HashMap<String, CoreValue>, WasmValueDecodeError> {
    entries
        .iter()
        .map(|(key, value)| {
            wit_to_core_persisted_value(value)
                .map(|value| (key.clone(), value))
                .map_err(|error| error.prepend_attribute(key))
        })
        .collect()
}

// -- ResourceId --

/// Render the legacy `ResourceId` map key used by the WASM guest contract.
///
/// This intentionally does not use `Display`: pending IDs must retain the
/// exact pre-pending-token wire spelling even though their diagnostic display
/// now includes `<pending>`.
pub(crate) fn resource_id_wire_key(id: &CoreResourceId) -> String {
    match (id.provider.is_empty(), id.identity_state()) {
        (true, ResourceIdentityState::Pending(_)) => id.resource_type.clone(),
        (true, ResourceIdentityState::Resolved(identity)) => {
            format!("{}.{}", id.resource_type, identity.as_str())
        }
        (false, ResourceIdentityState::Pending(_)) => {
            format!("{}.{}", id.provider, id.resource_type)
        }
        (false, ResourceIdentityState::Resolved(identity)) => {
            format!("{}.{}.{}", id.provider, id.resource_type, identity.as_str())
        }
    }
}

pub(crate) fn core_to_wit_resource_id(id: &CoreResourceId) -> wit::ResourceId {
    let wire = carina_core::protocol_convert::resource_id_to_protocol(id);
    wit::ResourceId {
        provider: wire.provider,
        resource_type: wire.resource_type,
        identity: wire.identity,
    }
}

fn wit_to_wire_resource_id(id: wit::ResourceId) -> proto::ResourceId {
    proto::ResourceId {
        provider: id.provider,
        resource_type: id.resource_type,
        identity: id.identity,
    }
}

// -- State --

pub(crate) fn core_to_wit_persisted_state(
    state: &CoreState,
) -> Result<wit::PersistedState, SerializationError> {
    Ok(wit::PersistedState {
        identifier: state.identifier.clone(),
        attributes: core_to_wit_persisted_value_map(&state.attributes)?,
        exists: state.exists,
    })
}

pub(crate) fn wit_to_core_state(
    state: &wit::State,
    id: &CoreResourceId,
) -> Result<CoreState, WasmValueDecodeError> {
    let attributes = wit_to_core_value_map(&state.attributes)?;
    if !state.exists {
        return Ok(CoreState::not_found(id.clone()));
    }
    let mut core_state = CoreState::existing(id.clone(), attributes);
    if let Some(ref ident) = state.identifier {
        core_state = core_state.with_identifier(ident);
    }
    Ok(core_state)
}

fn wit_to_core_applied_state_for_operation(
    state: wit::AppliedState,
    id: &CoreResourceId,
    operation: AppliedStateOperation,
) -> Result<CoreAppliedState, WasmAppliedStateDecodeError> {
    let wit::AppliedState {
        identifier,
        attributes,
    } = state;
    let attributes = wit_to_core_value_map(&attributes)
        .map_err(|source| WasmAppliedStateDecodeError::attributes(operation, id, source))?;
    CoreAppliedState::new(id.clone(), identifier, attributes)
        .map_err(|source| WasmAppliedStateDecodeError::identifier(operation, id, source))
}

fn wit_to_core_applied_persisted_state(
    state: wit::AppliedPersistedState,
    id: &CoreResourceId,
) -> Result<CoreAppliedState, WasmAppliedStateDecodeError> {
    let wit::AppliedPersistedState {
        identifier,
        attributes,
    } = state;
    let operation = AppliedStateOperation::Update;
    let attributes = wit_to_core_persisted_value_map(&attributes)
        .map_err(|source| WasmAppliedStateDecodeError::attributes(operation, id, source))?;
    CoreAppliedState::new(id.clone(), identifier, attributes)
        .map_err(|source| WasmAppliedStateDecodeError::identifier(operation, id, source))
}

pub(crate) fn wit_to_core_read_outcome(
    outcome: wit::ReadOutcome,
    id: &CoreResourceId,
) -> Result<CoreReadOutcome, WasmAppliedStateDecodeError> {
    match outcome {
        wit::ReadOutcome::NotFound => Ok(CoreReadOutcome::not_found(id.clone())),
        wit::ReadOutcome::Found(state) => Ok(CoreReadOutcome::found(
            wit_to_core_applied_state_for_operation(state, id, AppliedStateOperation::Read)?,
        )),
    }
}

#[derive(Clone)]
struct PendingToken(String);

impl fmt::Debug for PendingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted pending token>")
    }
}

#[derive(Clone, Debug)]
struct PendingOperation {
    token: PendingToken,
    poll_after_secs: u32,
}

impl PendingOperation {
    fn from_wit(pending: wit::PendingOperation) -> Self {
        let wit::PendingOperation {
            token,
            poll_after_secs,
        } = pending;
        Self {
            token: PendingToken(token),
            poll_after_secs,
        }
    }

    fn poll_after_secs(&self) -> u32 {
        self.poll_after_secs
    }

    fn into_wit(self) -> wit::PendingOperation {
        let Self {
            token: PendingToken(token),
            poll_after_secs,
        } = self;
        wit::PendingOperation {
            token,
            poll_after_secs,
        }
    }
}

#[derive(Clone)]
pub(crate) struct UpdatePendingOperation(PendingOperation);

impl UpdatePendingOperation {
    pub(crate) fn poll_after_secs(&self) -> u32 {
        self.0.poll_after_secs()
    }

    pub(crate) fn into_wit(self) -> wit::PendingOperation {
        self.0.into_wit()
    }
}

#[derive(Clone)]
pub(crate) struct DeletePendingOperation(PendingOperation);

impl DeletePendingOperation {
    pub(crate) fn poll_after_secs(&self) -> u32 {
        self.0.poll_after_secs()
    }

    pub(crate) fn into_wit(self) -> wit::PendingOperation {
        self.0.into_wit()
    }
}

pub(crate) type DecodedCreateStep =
    proto::CreateStep<HashMap<String, CoreValue>, CorePartialReadDiagnostic, proto::PendingToken>;
pub(crate) type DecodedCreateVerdict = proto::CreateVerdict<CoreReadOutcome, CoreProviderError>;
pub(crate) type DecodedPollVerdict = proto::PollVerdict<CoreReadOutcome, CoreProviderError>;
pub(crate) type DecodedResumeStep =
    proto::ResumeStep<HashMap<String, CoreValue>, CorePartialReadDiagnostic, proto::PendingToken>;
pub(crate) type DecodedResumeVerdict = proto::ResumeVerdict<CoreReadOutcome, CoreProviderError>;
type DecodedCreatedState = DecodedIssued<(proto::AppliedIdentifier, HashMap<String, CoreValue>)>;

/// Result of decoding a create-bearing WIT value after its provider
/// identifier may have become known.
///
/// `Err` is reserved for values that never yielded a valid identifier. Once
/// the identifier decodes, malformed attributes or observations can only
/// become `CreatedReadFailed`, so callers cannot accidentally downgrade the
/// resource to an untracked create.
#[derive(Debug)]
pub(crate) enum DecodedIssued<T> {
    Decoded(T),
    CreatedReadFailed(CreatedReadDecodeFailure),
}

impl<T> DecodedIssued<T> {
    fn map<U>(self, map: impl FnOnce(T) -> U) -> DecodedIssued<U> {
        match self {
            Self::Decoded(value) => DecodedIssued::Decoded(map(value)),
            Self::CreatedReadFailed(failure) => DecodedIssued::CreatedReadFailed(failure),
        }
    }
}

#[derive(Debug)]
pub(crate) struct CreatedReadDecodeFailure {
    identifier: proto::AppliedIdentifier,
    sent: Option<proto::Sent>,
    declared_error: Option<CoreProviderError>,
    decode_error: WasmAppliedStateDecodeError,
}

impl CreatedReadDecodeFailure {
    pub(crate) fn into_parts(
        self,
    ) -> (
        proto::AppliedIdentifier,
        Option<proto::Sent>,
        Option<CoreProviderError>,
        WasmAppliedStateDecodeError,
    ) {
        (
            self.identifier,
            self.sent,
            self.declared_error,
            self.decode_error,
        )
    }
}

pub(crate) enum WasmUpdateOutcome {
    Done(Box<CoreUpdateOutcome>),
    Pending(UpdatePendingOperation),
}

pub(crate) enum WasmDeleteOutcome {
    Deleted,
    Pending(DeletePendingOperation),
}

fn wit_to_core_sent(sent: wit::SentAttributes) -> proto::Sent {
    match sent {
        wit::SentAttributes::All => proto::Sent::AllRequestAttributes,
        wit::SentAttributes::Only(attributes) => proto::Sent::only(attributes),
    }
}

fn decode_create_identifier(
    identifier: String,
    id: &CoreResourceId,
) -> Result<proto::AppliedIdentifier, WasmAppliedStateDecodeError> {
    proto::AppliedIdentifier::new(identifier).map_err(|source| {
        WasmAppliedStateDecodeError::identifier(AppliedStateOperation::Create, id, source)
    })
}

fn decode_created_attributes(
    state: wit::AppliedState,
    id: &CoreResourceId,
    sent: Option<proto::Sent>,
    declared_error: Option<CoreProviderError>,
) -> Result<DecodedCreatedState, WasmAppliedStateDecodeError> {
    let wit::AppliedState {
        identifier,
        attributes,
    } = state;
    let identifier = decode_create_identifier(identifier, id)?;
    match wit_to_core_value_map(&attributes) {
        Ok(attributes) => Ok(DecodedIssued::Decoded((identifier, attributes))),
        Err(source) => Ok(DecodedIssued::CreatedReadFailed(CreatedReadDecodeFailure {
            identifier,
            sent,
            declared_error,
            decode_error: WasmAppliedStateDecodeError::attributes(
                AppliedStateOperation::Create,
                id,
                source,
            ),
        })),
    }
}

pub(crate) fn core_to_wit_pending_create(pending: &CorePendingCreate) -> wit::PendingCreate {
    core_to_wit_pending_create_parts(
        pending.token(),
        pending.identifier(),
        pending.sent(),
        pending.poll_after_secs(),
    )
}

pub(crate) fn core_to_wit_pending_create_parts(
    token: &proto::PendingToken,
    identifier: Option<&proto::AppliedIdentifier>,
    sent: &proto::Sent,
    poll_after_secs: u32,
) -> wit::PendingCreate {
    let sent = match sent {
        proto::Sent::AllRequestAttributes => wit::SentAttributes::All,
        proto::Sent::Only(attributes) => {
            wit::SentAttributes::Only(attributes.iter().cloned().collect())
        }
    };
    wit::PendingCreate {
        operation: wit::PendingOperation {
            token: token.expose_for_provider_call().to_string(),
            poll_after_secs,
        },
        identifier: identifier.map(ToString::to_string),
        sent,
    }
}

pub(crate) fn core_to_wit_resume_pending(
    token: &proto::PendingToken,
    identifier: Option<&proto::AppliedIdentifier>,
) -> wit::ResumePending {
    wit::ResumePending {
        operation: wit::PendingOperation {
            token: token.expose_for_provider_call().to_string(),
            poll_after_secs: 1,
        },
        identifier: identifier.map(ToString::to_string),
    }
}

fn wit_to_core_pending_create(
    pending: wit::PendingCreate,
    id: &CoreResourceId,
) -> Result<proto::PendingCreate<proto::PendingToken>, WasmAppliedStateDecodeError> {
    let wit::PendingCreate {
        operation,
        identifier,
        sent,
    } = pending;
    let wit::PendingOperation {
        token,
        poll_after_secs,
    } = operation;
    let identifier = identifier
        .map(|identifier| decode_create_identifier(identifier, id))
        .transpose()?;
    Ok(proto::PendingCreate {
        token: proto::PendingToken::new(token),
        identifier,
        sent: wit_to_core_sent(sent),
        poll_after_secs,
    })
}

fn wit_to_core_observation(
    observation: wit::Observation,
    id: &CoreResourceId,
) -> Result<proto::Observation<CoreReadOutcome, CoreProviderError>, WasmAppliedStateDecodeError> {
    match observation {
        wit::Observation::Read(read) => {
            wit_to_core_read_outcome(read, id).map(proto::Observation::Read)
        }
        wit::Observation::ReadFailed(error) => Ok(proto::Observation::ReadFailed(
            wit_to_core_provider_error(error),
        )),
    }
}

pub(crate) fn wit_to_core_create_step(
    step: wit::CreateStep,
    id: &CoreResourceId,
) -> Result<DecodedIssued<DecodedCreateStep>, WasmAppliedStateDecodeError> {
    match step {
        wit::CreateStep::Success(state) => {
            decode_created_attributes(state, id, Some(proto::Sent::AllRequestAttributes), None).map(
                |decoded| {
                    decoded.map(|(identifier, attributes)| proto::CreateStep::Success {
                        identifier,
                        attributes,
                    })
                },
            )
        }
        wit::CreateStep::PartialSuccess(partial) => {
            let wit::CreatePartialSuccess { state, diagnostic } = partial;
            let wit::PartialReadDiagnostic {
                reason,
                missing_attributes,
            } = diagnostic;
            decode_created_attributes(state, id, Some(proto::Sent::AllRequestAttributes), None).map(
                |decoded| {
                    decoded.map(|(identifier, attributes)| {
                        match CorePartialReadDiagnostic::new(reason, missing_attributes) {
                            Some(diagnostic) => proto::CreateStep::PartialSuccess {
                                identifier,
                                attributes,
                                diagnostic,
                            },
                            None => proto::CreateStep::Success {
                                identifier,
                                attributes,
                            },
                        }
                    })
                },
            )
        }
        wit::CreateStep::Pending(pending) => wit_to_core_pending_create(pending, id)
            .map(proto::CreateStep::Pending)
            .map(DecodedIssued::Decoded),
    }
}

fn wit_to_core_created_failure(
    created: wit::CreatedFailure,
    id: &CoreResourceId,
) -> Result<
    DecodedIssued<proto::CreatedFailure<CoreReadOutcome, CoreProviderError>>,
    WasmAppliedStateDecodeError,
> {
    let wit::CreatedFailure {
        identifier,
        read,
        sent,
        error,
    } = created;
    let identifier = decode_create_identifier(identifier, id)?;
    let sent = wit_to_core_sent(sent);
    let error = wit_to_core_provider_error(error);
    match wit_to_core_observation(read, id) {
        Ok(observation) => Ok(DecodedIssued::Decoded(proto::CreatedFailure {
            identifier,
            observation,
            sent,
            error,
        })),
        Err(decode_error) => Ok(DecodedIssued::CreatedReadFailed(CreatedReadDecodeFailure {
            identifier,
            sent: Some(sent),
            declared_error: Some(error),
            decode_error,
        })),
    }
}

pub(crate) fn wit_to_core_create_verdict(
    verdict: wit::CreateVerdict,
    id: &CoreResourceId,
) -> Result<DecodedIssued<DecodedCreateVerdict>, WasmAppliedStateDecodeError> {
    match verdict {
        wit::CreateVerdict::Untracked(error) => Ok(DecodedIssued::Decoded(
            proto::CreateVerdict::Untracked(wit_to_core_provider_error(error)),
        )),
        wit::CreateVerdict::Created(created) => wit_to_core_created_failure(created, id)
            .map(|decoded| decoded.map(proto::CreateVerdict::Created)),
        wit::CreateVerdict::Discarded(error) => Ok(DecodedIssued::Decoded(
            proto::CreateVerdict::Discarded(wit_to_core_provider_error(error)),
        )),
    }
}

pub(crate) fn wit_to_core_poll_verdict(
    verdict: wit::PollVerdict,
    id: &CoreResourceId,
) -> Result<DecodedIssued<DecodedPollVerdict>, WasmAppliedStateDecodeError> {
    match verdict {
        wit::PollVerdict::Untracked(error) => Ok(DecodedIssued::Decoded(
            proto::PollVerdict::Untracked(wit_to_core_provider_error(error)),
        )),
        wit::PollVerdict::Created(created) => wit_to_core_created_failure(created, id)
            .map(|decoded| decoded.map(proto::PollVerdict::Created)),
        wit::PollVerdict::Discarded(error) => Ok(DecodedIssued::Decoded(
            proto::PollVerdict::Discarded(wit_to_core_provider_error(error)),
        )),
        wit::PollVerdict::Undetermined(error) => Ok(DecodedIssued::Decoded(
            proto::PollVerdict::Undetermined(wit_to_core_provider_error(error)),
        )),
    }
}

pub(crate) fn wit_to_core_resume_step(
    step: wit::ResumeStep,
    id: &CoreResourceId,
) -> Result<DecodedIssued<DecodedResumeStep>, WasmAppliedStateDecodeError> {
    match step {
        wit::ResumeStep::Completed(state) => {
            decode_created_attributes(state, id, None, None).map(|decoded| {
                decoded.map(|(identifier, attributes)| proto::ResumeStep::Completed {
                    identifier,
                    attributes,
                })
            })
        }
        wit::ResumeStep::Pending(pending) => {
            let wit::ResumePending {
                operation,
                identifier,
            } = pending;
            let wit::PendingOperation {
                token,
                poll_after_secs,
            } = operation;
            let identifier = identifier
                .map(|identifier| decode_create_identifier(identifier, id))
                .transpose()?;
            Ok(DecodedIssued::Decoded(proto::ResumeStep::Pending(
                proto::ResumePending {
                    token: proto::PendingToken::new(token),
                    identifier,
                    poll_after_secs,
                },
            )))
        }
        wit::ResumeStep::Resent(step) => {
            wit_to_core_create_step(step, id).map(|decoded| decoded.map(proto::ResumeStep::Resent))
        }
    }
}

fn wit_to_core_resumed_failure(
    created: wit::ResumedFailure,
    id: &CoreResourceId,
) -> Result<
    DecodedIssued<proto::ResumedFailure<CoreReadOutcome, CoreProviderError>>,
    WasmAppliedStateDecodeError,
> {
    let wit::ResumedFailure {
        identifier,
        read,
        error,
    } = created;
    let identifier = decode_create_identifier(identifier, id)?;
    let error = wit_to_core_provider_error(error);
    match wit_to_core_observation(read, id) {
        Ok(observation) => Ok(DecodedIssued::Decoded(proto::ResumedFailure {
            identifier,
            observation,
            error,
        })),
        Err(decode_error) => Ok(DecodedIssued::CreatedReadFailed(CreatedReadDecodeFailure {
            identifier,
            sent: None,
            declared_error: Some(error),
            decode_error,
        })),
    }
}

pub(crate) fn wit_to_core_resume_verdict(
    verdict: wit::ResumeVerdict,
    id: &CoreResourceId,
) -> Result<DecodedIssued<DecodedResumeVerdict>, WasmAppliedStateDecodeError> {
    match verdict {
        wit::ResumeVerdict::Untracked(error) => Ok(DecodedIssued::Decoded(
            proto::ResumeVerdict::Untracked(wit_to_core_provider_error(error)),
        )),
        wit::ResumeVerdict::Created(created) => wit_to_core_resumed_failure(created, id)
            .map(|decoded| decoded.map(proto::ResumeVerdict::Created)),
        wit::ResumeVerdict::Discarded(error) => Ok(DecodedIssued::Decoded(
            proto::ResumeVerdict::Discarded(wit_to_core_provider_error(error)),
        )),
        wit::ResumeVerdict::Undetermined(error) => Ok(DecodedIssued::Decoded(
            proto::ResumeVerdict::Undetermined(wit_to_core_provider_error(error)),
        )),
        wit::ResumeVerdict::Resent(verdict) => wit_to_core_create_verdict(verdict, id)
            .map(|decoded| decoded.map(proto::ResumeVerdict::Resent)),
    }
}

pub(crate) fn wit_to_core_update_outcome(
    outcome: wit::UpdateOutcome,
    id: &CoreResourceId,
) -> Result<WasmUpdateOutcome, WasmAppliedStateDecodeError> {
    match outcome {
        wit::UpdateOutcome::Success(state) => Ok(WasmUpdateOutcome::Done(Box::new(
            CoreUpdateOutcome::Success {
                state: wit_to_core_applied_persisted_state(state, id)?,
            },
        ))),
        wit::UpdateOutcome::PartialSuccess(partial) => {
            let wit::UpdatePartialSuccess { state, diagnostic } = partial;
            let wit::PartialReadDiagnostic {
                reason,
                missing_attributes,
            } = diagnostic;
            Ok(WasmUpdateOutcome::Done(Box::new(
                CoreUpdateOutcome::partial_success(
                    wit_to_core_applied_persisted_state(state, id)?,
                    reason,
                    missing_attributes,
                ),
            )))
        }
        wit::UpdateOutcome::Pending(pending) => Ok(WasmUpdateOutcome::Pending(
            UpdatePendingOperation(PendingOperation::from_wit(pending)),
        )),
    }
}

pub(crate) fn wit_to_core_delete_outcome(outcome: wit::DeleteOutcome) -> WasmDeleteOutcome {
    match outcome {
        wit::DeleteOutcome::Deleted => WasmDeleteOutcome::Deleted,
        wit::DeleteOutcome::Pending(pending) => {
            WasmDeleteOutcome::Pending(DeletePendingOperation(PendingOperation::from_wit(pending)))
        }
    }
}

// -- Resource --

pub(crate) fn core_to_wit_resource<S>(
    resource: &CoreResource<S>,
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
pub(crate) fn core_data_source_to_wit_resource<S>(
    data_source: &CoreDataSource<S>,
) -> Result<wit::ResourceDef, SerializationError> {
    Ok(wit::ResourceDef {
        id: core_to_wit_resource_id(&data_source.id),
        attributes: core_to_wit_value_map(&carina_core::resource::attrs_to_hashmap(
            &data_source.attributes,
        ))?,
    })
}

#[cfg(test)]
pub(crate) fn wit_to_core_resource(
    resource: &wit::ResourceDef,
) -> Result<CoreResource, WasmValueDecodeError> {
    let wire_id = wit_to_wire_resource_id(resource.id.clone());
    let id = carina_core::protocol_convert::protocol_to_resource_id(&wire_id);
    let mut core_resource = CoreResource::from_id(id);
    core_resource.attributes = wit_to_core_value_map(&resource.attributes)?
        .into_iter()
        .collect();
    Ok(core_resource)
}

// -- ProviderError --

/// Encode the protocol record produced by [`CoreProviderError::to_wire`] as a
/// WIT `provider-error` variant.
#[cfg(test)]
pub(crate) fn wire_to_wit_provider_error(err: proto::ProviderError) -> wit::ProviderError {
    let wit_detail = wit::ErrorDetail {
        message: err.message,
        resource_id: err.resource_id.map(|id| wit::ResourceId {
            provider: id.provider,
            resource_type: id.resource_type,
            identity: id.identity,
        }),
        cause: err.cause,
        provider_name: err.provider_name,
        operation: err.operation,
        status: err.status,
        code: err.code,
        request_id: err.request_id,
    };
    match err.kind {
        proto::ProviderErrorKind::InvalidInput => wit::ProviderError::InvalidInput(wit_detail),
        proto::ProviderErrorKind::ApiError => wit::ProviderError::ApiError(wit_detail),
        proto::ProviderErrorKind::NotFound => wit::ProviderError::NotFound(wit_detail),
        proto::ProviderErrorKind::Timeout => wit::ProviderError::Timeout(wit_detail),
        proto::ProviderErrorKind::Internal => wit::ProviderError::Internal(wit_detail),
    }
}

pub(crate) fn wit_to_wire_provider_error(err: wit::ProviderError) -> proto::ProviderError {
    let (detail, kind) = match err {
        wit::ProviderError::InvalidInput(d) => (d, proto::ProviderErrorKind::InvalidInput),
        wit::ProviderError::ApiError(d) => (d, proto::ProviderErrorKind::ApiError),
        wit::ProviderError::NotFound(d) => (d, proto::ProviderErrorKind::NotFound),
        wit::ProviderError::Timeout(d) => (d, proto::ProviderErrorKind::Timeout),
        wit::ProviderError::Internal(d) => (d, proto::ProviderErrorKind::Internal),
    };
    proto::ProviderError {
        kind,
        message: detail.message,
        resource_id: detail.resource_id.map(wit_to_wire_resource_id),
        cause: detail.cause,
        provider_name: detail.provider_name,
        operation: detail.operation,
        status: detail.status,
        code: detail.code,
        request_id: detail.request_id,
    }
}

/// Convert a WIT [`wit::ProviderError`] into the host-side core
/// [`CoreProviderError`]. The variant is preserved exactly; the
/// flattened `cause` string is rehydrated as a typed `FlattenedWireCause` inside
/// [`CoreProviderError::from_wire`].
pub(crate) fn wit_to_core_provider_error(err: wit::ProviderError) -> CoreProviderError {
    CoreProviderError::from_wire(wit_to_wire_provider_error(err))
}

/// Decode a guest initialization error and attach its boundary phase when the
/// guest did not provide a more specific operation.
///
/// Provider identity is attached later by the wiring layer, which owns the
/// configured name in both built-in and source-loaded paths. Keeping the
/// decoded [`CoreProviderError`] intact preserves its kind, source text,
/// resource id, and structured cloud fields.
pub fn decode_provider_initialization_error(error: proto::ProviderError) -> CoreProviderError {
    let mut error = CoreProviderError::from_wire(error);
    if error.detail().operation.is_none() {
        error.detail_mut().operation = Some("initialize".to_string());
    }
    error
}

// -- Update request and patch --

/// Build a [`wit::UpdateRequest`] from the host-side core
/// [`CoreUpdateRequest`]. Patch op order is preserved.
pub(crate) fn core_to_wit_update_request(
    request: &CoreUpdateRequest,
) -> Result<wit::UpdateRequest, SerializationError> {
    Ok(wit::UpdateRequest {
        current: core_to_wit_persisted_state(request.from())?,
        patch: core_to_wit_update_patch(request.patch())?,
    })
}

/// Build a [`wit::CreateRequest`] from the host-side core
/// [`CoreCreateRequest`].
pub(crate) fn core_to_wit_create_request(
    request: &CoreCreateRequest,
) -> Result<wit::CreateRequest, SerializationError> {
    Ok(wit::CreateRequest {
        res: core_to_wit_resource(request.resource().as_resource())?,
    })
}

/// Build a [`wit::ReadRequest`].
///
/// `ReadRequest` carries no operationally meaningful fields; the
/// `reserved` placeholder exists because the wasm component model
/// rejects records with zero fields.
pub(crate) fn core_to_wit_read_request(_request: &CoreReadRequest) -> wit::ReadRequest {
    wit::ReadRequest { reserved: false }
}

/// Build a [`wit::DeleteRequest`] from the host-side core
/// [`CoreDeleteRequest`].
pub(crate) fn core_to_wit_delete_request(request: &CoreDeleteRequest) -> wit::DeleteRequest {
    wit::DeleteRequest {
        directives: core_to_wit_directives(&request.directives),
    }
}

/// Convert a [`Directives`] to a [`wit::Directives`].
pub(crate) fn core_to_wit_directives(directives: &Directives) -> wit::Directives {
    // `depends_on` and `provider_instance` are host-only orchestration metadata;
    // the WIT Directives record intentionally carries only these three flags.
    wit::Directives {
        force_delete: directives.force_delete,
        create_before_destroy: directives.create_before_destroy,
        prevent_destroy: directives.prevent_destroy,
    }
}

/// Convert a host-side [`CoreUpdatePatch`] to a [`wit::UpdatePatch`].
/// Op order is preserved.
pub(crate) fn core_to_wit_update_patch(
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
pub(crate) fn core_to_wit_patch_op(op: &CorePatchOp) -> Result<wit::PatchOp, SerializationError> {
    Ok(match op {
        CorePatchOp::Add { key, value } => wit::PatchOp::Add(wit::PatchValue {
            key: key.clone(),
            value: core_to_wit_value(value)?,
        }),
        CorePatchOp::Replace { key, value } => wit::PatchOp::Replace(wit::PatchValue {
            key: key.clone(),
            value: core_to_wit_value(value)?,
        }),
        CorePatchOp::Reassert { key, value } => wit::PatchOp::Reassert(wit::PatchValue {
            key: key.clone(),
            value: core_to_wit_value(value)?,
        }),
        CorePatchOp::Remove { key } => wit::PatchOp::Remove(key.clone()),
    })
}

/// Convert a [`wit::PatchOp`] to a host-side [`CorePatchOp`].
#[cfg(test)]
pub(crate) fn wit_to_core_patch_op(op: &wit::PatchOp) -> Result<CorePatchOp, WasmValueDecodeError> {
    Ok(match op {
        wit::PatchOp::Add(value) => CorePatchOp::Add {
            key: value.key.clone(),
            value: wit_to_core_value(&value.value)?,
        },
        wit::PatchOp::Replace(value) => CorePatchOp::Replace {
            key: value.key.clone(),
            value: wit_to_core_value(&value.value)?,
        },
        wit::PatchOp::Reassert(value) => CorePatchOp::Reassert {
            key: value.key.clone(),
            value: wit_to_core_value(&value.value)?,
        },
        wit::PatchOp::Remove(key) => CorePatchOp::Remove { key: key.clone() },
    })
}

/// Convert a host-side [`carina_core::schema::TypeIdentity`] to the
/// WIT [`wit::TypeIdentity`] record.
///
/// The WIT record has no `option` provider field; an absent provider
/// axis is encoded as an empty string (see the `type-identity` record
/// doc in `provider.wit`).
pub(crate) fn core_type_identity_to_wit(
    identity: &carina_core::schema::TypeIdentity,
) -> wit::TypeIdentity {
    wit::TypeIdentity {
        provider: identity.provider.clone().unwrap_or_default(),
        segments: identity.segments.clone(),
        kind: identity.kind.clone(),
    }
}

/// Deserialize JSON to (name, display_name, version) tuple from ProviderInfo.
pub fn json_to_provider_info(json: &str) -> Result<(String, String, String), serde_json::Error> {
    let info = serde_json::from_str::<proto::ProviderInfo>(json)?;
    Ok((info.name, info.display_name, info.version))
}

/// Reject a provider whose protocol is outside this host's supported range.
/// The SDK stamps the version it was compiled against into the info() envelope,
/// so this is a compile-time fact of the provider's protocol crate, not a value
/// the author sets. A provider predating the envelope omits the field ->
/// deserializes to 0 -> below any host minimum (the carina#3364 class).
/// carina#3365.
pub fn check_protocol_version(info_json: &str) -> Result<(), String> {
    let envelope: proto::ProviderInfoEnvelope =
        serde_json::from_str(info_json).map_err(|e| format!("invalid provider info: {e}"))?;
    let host = carina_provider_protocol::PROTOCOL_VERSION;
    let min = carina_provider_protocol::MIN_SUPPORTED_PROTOCOL_VERSION;
    if envelope.protocol_version < min {
        return Err(format!(
            "provider '{}' was built against protocol version {} but this host \
             requires at least version {}; rebuild the provider against the current carina protocol",
            envelope.info.name, envelope.protocol_version, min
        ));
    }
    if envelope.protocol_version > host {
        return Err(format!(
            "provider '{}' was built against protocol version {} but this host \
             supports up to version {}; upgrade carina to load this provider",
            envelope.info.name, envelope.protocol_version, host
        ));
    }
    Ok(())
}

/// Deserialize JSON to validated core resource schemas.
pub fn json_to_schemas(json: &str) -> Result<Vec<CoreValidatedResourceSchema>, SchemaDecodeError> {
    let proto_schemas: Vec<proto::ResourceSchema> =
        serde_json::from_str(json).map_err(SchemaDecodeError::schema_json_parse)?;
    carina_core::protocol_convert::protocol_to_resource_schemas(&proto_schemas)
        .map_err(SchemaDecodeError::protocol_conversion)
}

/// Deserialize a JSON-encoded `BTreeMap<String, AttributeType>` from a WASM
/// guest and convert it to core `AttributeType` values.
pub fn json_to_attribute_types(json: &str) -> Result<CoreValidatedConfigSchema, SchemaDecodeError> {
    let proto_types: BTreeMap<String, proto::AttributeType> =
        serde_json::from_str(json).map_err(SchemaDecodeError::schema_json_parse)?;
    carina_core::protocol_convert::protocol_to_config_schema(proto_types)
        .map_err(SchemaDecodeError::protocol_conversion)
}

#[cfg(test)]
fn proto_schema_to_core(
    schema: &proto::ResourceSchema,
) -> Result<CoreValidatedResourceSchema, SchemaDecodeError> {
    carina_core::protocol_convert::protocol_to_resource_schema(schema)
        .map_err(SchemaDecodeError::protocol_conversion)
}

#[cfg(test)]
fn proto_attr_schema_to_core(
    attribute: &proto::AttributeSchema,
) -> Result<CoreAttributeSchema, SchemaDecodeError> {
    carina_core::protocol_convert::protocol_to_attribute_schema(attribute)
        .map_err(SchemaDecodeError::protocol_conversion)
}

#[cfg(test)]
fn proto_attr_type_to_core(
    attribute_type: &proto::AttributeType,
) -> Result<CoreAttributeType, SchemaDecodeError> {
    carina_core::protocol_convert::protocol_to_attribute_type(attribute_type)
        .map_err(SchemaDecodeError::protocol_conversion)
}

#[cfg(test)]
fn proto_struct_field_to_core(
    field: &proto::StructField,
) -> Result<CoreStructField, SchemaDecodeError> {
    carina_core::protocol_convert::protocol_to_struct_field(field)
        .map_err(SchemaDecodeError::protocol_conversion)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_error_in_chain<'a, T: std::error::Error + 'static>(
        error: &'a (dyn std::error::Error + 'static),
    ) -> Option<&'a T> {
        let mut current = Some(error);
        while let Some(error) = current {
            if let Some(found) = error.downcast_ref::<T>() {
                return Some(found);
            }
            current = error.source();
        }
        None
    }

    fn render_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
        let mut rendered = vec![error.to_string()];
        let mut current = error.source();
        while let Some(source) = current {
            rendered.push(source.to_string());
            current = source.source();
        }
        rendered.join(" -> ")
    }

    #[test]
    fn read_outcome_rejects_empty_identifier_with_typed_context() {
        let id = CoreResourceId::with_identity("test", "example");
        let outcome = wit::ReadOutcome::Found(wit::AppliedState {
            identifier: String::new(),
            attributes: Vec::new(),
        });

        let error = wit_to_core_read_outcome(outcome, &id)
            .expect_err("an existing read must not accept an empty identifier");

        let message = error.to_string();
        assert!(message.contains("read"), "{message}");
        assert!(message.contains(&id.to_string()), "{message}");
        assert!(matches!(
            &error.kind,
            WasmAppliedStateDecodeErrorKind::Identifier(EmptyAppliedIdentifier)
        ));
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn create_step_rejects_empty_applied_identifier_with_typed_context() {
        let id = CoreResourceId::with_identity("test", "example");
        let step = wit::CreateStep::Success(wit::AppliedState {
            identifier: String::new(),
            attributes: Vec::new(),
        });

        let error = match wit_to_core_create_step(step, &id) {
            Err(error) => error,
            Ok(_) => panic!("a successful create must not accept an empty identifier"),
        };

        let message = error.to_string();
        assert!(message.contains("create"), "{message}");
        assert!(message.contains(&id.to_string()), "{message}");
        assert!(matches!(
            &error.kind,
            WasmAppliedStateDecodeErrorKind::Identifier(EmptyAppliedIdentifier)
        ));
        assert!(std::error::Error::source(&error).is_none());
    }

    fn malformed_created_attributes() -> Vec<(String, wit::Value)> {
        vec![(
            "broken".to_string(),
            wit::Value::ListVal("not-json".to_string()),
        )]
    }

    fn wit_internal_error(message: &str) -> wit::ProviderError {
        wire_to_wit_provider_error(CoreProviderError::internal(message).to_wire())
    }

    #[test]
    fn create_success_decode_failure_after_identifier_is_recoverable() {
        let id = CoreResourceId::with_identity("test", "example");
        let step = wit::CreateStep::Success(wit::AppliedState {
            identifier: "created-id".to_string(),
            attributes: malformed_created_attributes(),
        });

        assert!(
            wit_to_core_create_step(step, &id).is_ok(),
            "a valid created identifier must survive an attribute decode failure"
        );
    }

    #[test]
    fn created_verdict_decode_failure_after_identifier_is_recoverable() {
        let id = CoreResourceId::with_identity("test", "example");
        let verdict = wit::CreateVerdict::Created(wit::CreatedFailure {
            identifier: "created-id".to_string(),
            read: wit::Observation::Read(wit::ReadOutcome::Found(wit::AppliedState {
                identifier: "created-id".to_string(),
                attributes: malformed_created_attributes(),
            })),
            sent: wit::SentAttributes::All,
            error: wit_internal_error("guest create failure"),
        });

        assert!(
            wit_to_core_create_verdict(verdict, &id).is_ok(),
            "a valid created identifier must survive an observation decode failure"
        );
    }

    #[test]
    fn resume_completed_decode_failure_after_identifier_is_recoverable() {
        let id = CoreResourceId::with_identity("test", "example");
        let step = wit::ResumeStep::Completed(wit::AppliedState {
            identifier: "created-id".to_string(),
            attributes: malformed_created_attributes(),
        });

        assert!(
            wit_to_core_resume_step(step, &id).is_ok(),
            "a valid resumed identifier must survive an attribute decode failure"
        );
    }

    #[test]
    fn update_outcome_rejects_empty_applied_identifier_with_typed_context() {
        let id = CoreResourceId::with_identity("test", "example");
        let outcome = wit::UpdateOutcome::Success(wit::AppliedPersistedState {
            identifier: String::new(),
            attributes: Vec::new(),
        });

        let error = match wit_to_core_update_outcome(outcome, &id) {
            Err(error) => error,
            Ok(_) => panic!("a successful update must not accept an empty identifier"),
        };

        let message = error.to_string();
        assert!(message.contains("update"), "{message}");
        assert!(message.contains(&id.to_string()), "{message}");
        assert!(matches!(
            &error.kind,
            WasmAppliedStateDecodeErrorKind::Identifier(EmptyAppliedIdentifier)
        ));
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn pending_token_debug_is_redacted() {
        let debug = format!("{:?}", PendingToken("do-not-log-this".to_string()));
        assert_eq!(debug, "<redacted pending token>");
        assert!(!debug.contains("do-not-log-this"));
    }

    #[derive(Debug)]
    struct TestOperationError {
        operation: &'static str,
        source: std::io::Error,
    }

    impl std::fmt::Display for TestOperationError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "in operation {}: {}", self.operation, self.source)
        }
    }

    impl std::error::Error for TestOperationError {}

    fn redacted_secret_and_digest(plaintext: &str) -> (CoreValue, String) {
        let secret = CoreValue::Deferred(DeferredValue::Secret(Box::new(CoreValue::Concrete(
            ConcreteValue::String(plaintext.into()),
        ))));
        let persisted = carina_core::value::PersistedValue::encode_provider_value(&secret, None)
            .expect("serialize test secret")
            .as_json()
            .clone();
        let digest = persisted[PERSISTED_SECRET_KEY]
            .as_str()
            .expect("structural secret digest")
            .to_string();
        let hash = RedactedSecretHash::from_encoded_digest(&digest)
            .expect("parse generated redacted-secret digest");
        (
            CoreValue::Deferred(DeferredValue::RedactedSecret { hash }),
            digest,
        )
    }

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

    fn proto_attr(name: &str, attr_type: proto::AttributeType) -> proto::AttributeSchema {
        proto::AttributeSchema {
            name: name.to_string(),
            attr_type,
            required: false,
            default: None,
            description: None,
            create_only: false,
            read_only: false,
            write_only: false,
            block_name: None,
            provider_name: None,
            removable: None,
            identity: false,
            deferred_populate: false,
        }
    }

    fn proto_resource(
        resource_type: &str,
        attributes: impl IntoIterator<Item = (String, proto::AttributeSchema)>,
    ) -> proto::ResourceSchema {
        proto::ResourceSchema {
            resource_type: resource_type.to_string(),
            attributes: attributes.into_iter().collect(),
            description: None,
            kind: proto::SchemaKind::Managed,
            unique_name: proto::UniqueNameSpec::Conflicting,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![],
            defs: Default::default(),
        }
    }

    #[test]
    fn sdk_json_host_round_trip_property_covers_portable_defaults() {
        use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

        let cases = vec![
            (
                AttributeType::bool(),
                CoreValue::Concrete(ConcreteValue::Bool(true)),
            ),
            (
                AttributeType::int(),
                CoreValue::Concrete(ConcreteValue::Int(-42)),
            ),
            (
                AttributeType::float(),
                CoreValue::Concrete(ConcreteValue::Float(1.25)),
            ),
            (
                AttributeType::string(),
                CoreValue::Concrete(ConcreteValue::String("portable".into())),
            ),
            (
                AttributeType::list(AttributeType::string()),
                CoreValue::Concrete(ConcreteValue::StringList(vec!["one".into(), "two".into()])),
            ),
            (
                AttributeType::list(AttributeType::int()),
                CoreValue::Concrete(ConcreteValue::List(vec![
                    CoreValue::Concrete(ConcreteValue::Int(1)),
                    CoreValue::Concrete(ConcreteValue::Int(2)),
                ])),
            ),
            (
                AttributeType::map(AttributeType::string()),
                CoreValue::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                    "key".to_string(),
                    CoreValue::Concrete(ConcreteValue::String("value".into())),
                )]))),
            ),
        ];

        for (index, (attribute_type, default)) in cases.into_iter().enumerate() {
            let schema = ResourceSchema::new(format!("test.Portable{index}"))
                .attribute(
                    AttributeSchema::optional("value", attribute_type)
                        .with_default(default)
                        .build(),
                )
                .validate()
                .expect("generated portable witness must validate");
            let guest_json = carina_plugin_sdk::schemas_to_json(&[schema])
                .expect("typed SDK witness must serialize");
            let host_witnesses =
                json_to_schemas(&guest_json).expect("host must decode SDK schema JSON");
            let host_json = carina_plugin_sdk::schemas_to_json(&host_witnesses)
                .expect("host witness must serialize through the same seam");

            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&guest_json).unwrap(),
                serde_json::from_str::<serde_json::Value>(&host_json).unwrap(),
                "portable JSON projection changed for generated case {index}"
            );
        }
    }

    #[test]
    fn typed_sdk_resource_and_config_witnesses_load_in_host() {
        use carina_core::schema::{
            AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
            ValidatedConfigSchema, ValidatorType, legacy_validator,
        };

        let resource_record = AttributeType::struct_(
            "ResourceRecord",
            vec![StructField::required("name", AttributeType::string()).build()],
        );
        let output = AttributeType::list(AttributeType::struct_(
            "DomainValidation",
            vec![
                StructField::required("domain_name", AttributeType::string()).build(),
                StructField::optional("resource_record", resource_record)
                    .deferred_populate()
                    .build(),
            ],
        ));
        let schema = ResourceSchema::new("test.Certificate")
            .attribute(
                AttributeSchema::provider_populated("domain_validation_options", output).build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "count",
                    AttributeType::refined_int_with_validator(
                        TypeIdentity::new(Some("test"), ["Certificate"], "PositiveCount"),
                        None,
                        legacy_validator(|value| match value {
                            CoreValue::Concrete(ConcreteValue::Int(value)) if *value > 0 => Ok(()),
                            _other => Err("count must be positive".to_string()),
                        }),
                    ),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .with_declared_validators(vec![ValidatorType::TagsKeyValueCheck])
            .validate()
            .expect("SDK resource fixture must validate");
        let schema_json = carina_plugin_sdk::schemas_to_json(&[schema]).unwrap();
        let decoded = json_to_schemas(&schema_json).expect("typed resource witness must load");
        let resource_record = decoded[0]
            .effective_node_at_path(&[
                "domain_validation_options".to_string(),
                "resource_record".to_string(),
            ])
            .expect("nested output path must survive transport");
        assert!(resource_record.is_read_only());
        assert!(resource_record.is_deferred_populate());

        let config = ValidatedConfigSchema::validate(BTreeMap::from([(
            "assume_role".to_string(),
            AttributeType::struct_(
                "AssumeRole",
                vec![
                    StructField::required("role_arn", AttributeType::string()).build(),
                    StructField::optional("session_name", AttributeType::string()).build(),
                ],
            ),
        )]))
        .expect("SDK config fixture must validate");
        let config_json = carina_plugin_sdk::provider_config_schema_to_json(&config).unwrap();
        let decoded_config =
            json_to_attribute_types(&config_json).expect("typed config witness must load");
        assert!(
            decoded_config
                .effective_field_at_path(&["assume_role".to_string(), "role_arn".to_string(),])
                .is_some()
        );
    }

    fn core_struct_field_to_proto_for_schema_round_trip(
        field: &CoreStructField,
    ) -> proto::StructField {
        carina_core::protocol_convert::struct_field_to_protocol(field)
            .expect("round-trip fixture must cross the core protocol seam")
    }

    fn core_attr_schema_to_proto_for_schema_round_trip(
        attr: &CoreAttributeSchema,
    ) -> proto::AttributeSchema {
        carina_core::protocol_convert::attribute_schema_to_protocol(attr)
            .expect("round-trip fixture must cross the core protocol seam")
    }

    #[test]
    fn required_read_only_struct_field_is_rejected_at_the_wire_boundary() {
        let field = proto::StructField {
            name: "provider_id".to_string(),
            field_type: proto_string(),
            required: true,
            description: None,
            block_name: None,
            provider_name: None,
            read_only: true,
            write_only: false,
            deferred_populate: false,
        };

        let error = proto_struct_field_to_core(&field)
            .expect_err("required and read-only must not form a core StructField");

        let rendered = render_error_chain(&error);
        assert!(
            rendered.contains(
                "attribute 'provider_id': input mode cannot be both required and read-only"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn read_only_write_only_struct_field_is_rejected_at_the_wire_boundary() {
        let field = proto::StructField {
            name: "client_secret".to_string(),
            field_type: proto_string(),
            required: false,
            description: None,
            block_name: None,
            provider_name: None,
            read_only: true,
            write_only: true,
            deferred_populate: false,
        };

        let error = proto_struct_field_to_core(&field)
            .expect_err("read-only and write-only must not form a core StructField");

        let rendered = render_error_chain(&error);
        assert!(
            rendered.contains(
                "attribute 'client_secret': struct field cannot be both write-only and provider-populated"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn host_decodes_a_shared_def_without_changing_its_fields() {
        let mut writable = proto_attr(
            "tags",
            proto::AttributeType::List {
                element_type: Box::new(proto::AttributeType::Ref {
                    name: "Tag".to_string(),
                }),
                ordered: true,
                length: None,
                validate: None,
            },
        );
        writable.required = true;
        let mut output = proto_attr(
            "effective_tags",
            proto::AttributeType::List {
                element_type: Box::new(proto::AttributeType::Ref {
                    name: "Tag".to_string(),
                }),
                ordered: true,
                length: None,
                validate: None,
            },
        );
        output.read_only = true;
        let mut schema = proto_resource(
            "test.Widget",
            [
                ("tags".to_string(), writable),
                ("effective_tags".to_string(), output),
            ],
        );
        schema.defs.insert(
            "Tag".to_string(),
            proto::AttributeType::Struct {
                name: "Tag".to_string(),
                fields: vec![proto::StructField {
                    name: "key".to_string(),
                    field_type: proto_string(),
                    required: true,
                    description: None,
                    block_name: Some("tag_key".to_string()),
                    provider_name: Some("Key".to_string()),
                    read_only: false,
                    write_only: false,
                    deferred_populate: false,
                }],
            },
        );

        let decoded = proto_schema_to_core(&schema)
            .expect("shared definitions must be validated independently for each path");
        let tag = decoded
            .as_schema()
            .defs()
            .get("Tag")
            .expect("Tag definition must survive decode");
        let fields = match tag.raw_shape() {
            carina_core::schema::RawShape::Struct {
                name: _name,
                fields,
            } => fields,
            shape => panic!("expected struct, got {shape:?}"),
        };
        assert_eq!(
            fields[0].parts().declared_input_mode,
            CoreInputMode::Required
        );
        assert_eq!(fields[0].parts().block_name, Some("tag_key"));
    }

    #[test]
    fn descendant_write_only_remains_invalid_below_provider_output() {
        let mut output = proto_attr(
            "output",
            proto::AttributeType::Struct {
                name: "Output".to_string(),
                fields: vec![proto::StructField {
                    name: "secret".to_string(),
                    field_type: proto_string(),
                    required: false,
                    description: None,
                    block_name: None,
                    provider_name: None,
                    read_only: false,
                    write_only: true,
                    deferred_populate: false,
                }],
            },
        );
        output.read_only = true;
        let schema = proto_resource("test.Widget", [("output".to_string(), output)]);

        let error = proto_schema_to_core(&schema)
            .expect_err("shared validation must reject write-only output descendants");
        assert_eq!(
            render_error_chain(&error).split(" -> ").nth(1).unwrap(),
            "resource 'test.Widget' attribute 'output.secret': resource schema is invalid"
        );
        let source =
            find_error_in_chain::<carina_core::schema::ResourceSchemaValidationError>(&error)
                .expect("typed resource validation error must remain in the chain");
        assert!(
            source.issues().any(|(path, message)| {
                path == "output.secret" && message.contains("write-only")
            })
        );
    }

    #[test]
    fn required_read_only_attribute_is_rejected_with_resource_and_attribute_context() {
        let mut arn = proto_attr("arn", proto_string());
        arn.required = true;
        arn.read_only = true;
        let schema = proto_resource("awscc.acm.Certificate", [("arn".to_string(), arn)]);
        let json = serde_json::to_string(&vec![schema]).unwrap();

        let error = json_to_schemas(&json)
            .expect_err("required and read-only must not form a core AttributeSchema");

        let rendered = render_error_chain(&error);
        assert!(rendered.contains(
            "resource 'awscc.acm.Certificate' attribute 'arn': input mode cannot be both required and read-only"
        ), "{rendered}");
    }

    #[test]
    fn read_only_write_only_attribute_is_rejected_at_the_wire_boundary() {
        let mut token = proto_attr("token", proto_string());
        token.read_only = true;
        token.write_only = true;
        let schema = proto_resource("test.Resource", [("token".to_string(), token)]);

        let error = proto_schema_to_core(&schema)
            .expect_err("read-only and write-only must not form a core AttributeSchema");

        let rendered = render_error_chain(&error);
        assert!(rendered.contains(
            "resource 'test.Resource' attribute 'token': attribute cannot be both write-only and provider-populated"
        ), "{rendered}");
    }

    #[test]
    fn nested_input_mode_error_names_resource_and_full_attribute_path() {
        let illegal_leaf = proto::StructField {
            name: "table_arn".to_string(),
            field_type: proto_string(),
            required: true,
            description: None,
            block_name: None,
            provider_name: None,
            read_only: true,
            write_only: false,
            deferred_populate: false,
        };
        let destination = proto::StructField {
            name: "destination".to_string(),
            field_type: proto::AttributeType::Struct {
                name: "Destination".to_string(),
                fields: vec![illegal_leaf],
            },
            required: false,
            description: None,
            block_name: None,
            provider_name: None,
            read_only: false,
            write_only: false,
            deferred_populate: false,
        };
        let metadata = proto_attr(
            "metadata_configuration",
            proto::AttributeType::Struct {
                name: "MetadataConfiguration".to_string(),
                fields: vec![destination],
            },
        );
        let schema = proto_resource(
            "awscc.s3.Bucket",
            [("metadata_configuration".to_string(), metadata)],
        );
        let json = serde_json::to_string(&vec![schema]).unwrap();

        let error = json_to_schemas(&json)
            .expect_err("the invalid nested field must abort schema decoding");

        let rendered = render_error_chain(&error);
        assert!(rendered.contains(
            "resource 'awscc.s3.Bucket' attribute 'metadata_configuration.destination.table_arn': input mode cannot be both required and read-only"
        ), "{rendered}");
    }

    #[test]
    fn top_level_write_only_and_write_only_bearing_is_rejected_at_wire_boundary() {
        let mut config = proto_attr(
            "config",
            proto::AttributeType::Struct {
                name: "Config".to_string(),
                fields: vec![proto::StructField {
                    name: "client_secret".to_string(),
                    field_type: proto_string(),
                    required: false,
                    description: None,
                    block_name: None,
                    provider_name: None,
                    read_only: false,
                    write_only: true,
                    deferred_populate: false,
                }],
            },
        );
        config.write_only = true;
        let schema = proto_resource("test.Resource", [("config".to_string(), config)]);

        let error = proto_schema_to_core(&schema)
            .expect_err("whole and nested write-only modes cannot be combined");

        let rendered = render_error_chain(&error);
        assert!(
            rendered.contains(
                "resource 'test.Resource' attribute 'config': resource schema is invalid"
            ),
            "{rendered}"
        );
        let source =
            find_error_in_chain::<carina_core::schema::ResourceSchemaValidationError>(&error)
                .expect("typed resource validation error must remain in the chain");
        assert!(source.issues().any(|(path, message)| {
            path == "config"
                && message == "top-level attribute cannot be both write-only and write-only-bearing"
        }));
    }

    #[test]
    fn schema_write_only_conflict_reports_lexicographically_first_attribute() {
        let conflicting_attribute = |name: &str| {
            let mut attribute = proto_attr(
                name,
                proto::AttributeType::Struct {
                    name: "Config".to_string(),
                    fields: vec![proto::StructField {
                        name: "client_secret".to_string(),
                        field_type: proto_string(),
                        required: false,
                        description: None,
                        block_name: None,
                        provider_name: None,
                        read_only: false,
                        write_only: true,
                        deferred_populate: false,
                    }],
                },
            );
            attribute.write_only = true;
            attribute
        };

        // Insert in reverse lexical order. The protocol container itself must
        // canonicalize the walk; the decoder must not repair ordering later.
        let attributes = BTreeMap::from([
            (
                "z_conflict".to_string(),
                conflicting_attribute("z_conflict"),
            ),
            (
                "a_conflict".to_string(),
                conflicting_attribute("a_conflict"),
            ),
        ]);
        let mut schema = proto_resource("test.Resource", std::iter::empty());
        schema.attributes = attributes;

        let error = proto_schema_to_core(&schema)
            .expect_err("both attributes violate the nested write-only invariant");

        let rendered = render_error_chain(&error);
        assert!(
            rendered.contains(
                "resource 'test.Resource' attribute 'a_conflict': resource schema is invalid"
            ),
            "{rendered}"
        );
        let source =
            find_error_in_chain::<carina_core::schema::ResourceSchemaValidationError>(&error)
                .expect("typed resource validation error must remain in the chain");
        assert_eq!(
            source.issues().next(),
            Some((
                "a_conflict",
                "top-level attribute cannot be both write-only and write-only-bearing"
            ))
        );
    }

    #[test]
    fn write_only_bearing_union_member_is_rejected_at_wire_boundary() {
        let authentication = proto_attr(
            "authentication",
            proto::AttributeType::Union {
                members: vec![
                    proto_string(),
                    proto::AttributeType::Struct {
                        name: "OidcConfig".to_string(),
                        fields: vec![proto::StructField {
                            name: "client_secret".to_string(),
                            field_type: proto_string(),
                            required: false,
                            description: None,
                            block_name: None,
                            provider_name: None,
                            read_only: false,
                            write_only: true,
                            deferred_populate: false,
                        }],
                    },
                ],
            },
        );
        let schema = proto_resource(
            "test.Resource",
            [("authentication".to_string(), authentication)],
        );

        let error = proto_schema_to_core(&schema)
            .expect_err("a union member must not hide nested write-only fields");

        let rendered = render_error_chain(&error);
        assert!(
            rendered.contains(
                "resource 'test.Resource' attribute 'authentication': resource schema is invalid"
            ),
            "{rendered}"
        );
        let source =
            find_error_in_chain::<carina_core::schema::ResourceSchemaValidationError>(&error)
                .expect("typed resource validation error must remain in the chain");
        assert!(source.issues().any(|(path, message)| {
            path == "authentication" && message == "union members cannot be write-only-bearing"
        }));
    }

    #[test]
    fn schema_transport_round_trip_is_exhaustive_for_touched_types() {
        let core_field =
            CoreStructField::provider_populated("provider_id", CoreAttributeType::string())
                .with_description("Provider-assigned identifier")
                .with_provider_name("ProviderId")
                .deferred_populate()
                .build();
        let field_json = serde_json::to_string(&core_struct_field_to_proto_for_schema_round_trip(
            &core_field,
        ))
        .unwrap();
        let proto_field: proto::StructField = serde_json::from_str(&field_json).unwrap();
        let round_tripped_field = proto_struct_field_to_core(&proto_field).unwrap();
        let CoreStructFieldParts {
            name,
            field_type,
            declared_input_mode,
            description,
            provider_name,
            block_name,
            deferred_populate,
            write_only,
        } = round_tripped_field.parts();
        assert_eq!(name, "provider_id");
        assert!(matches!(
            field_type
                .shape_ref_free()
                .expect("round-tripped field type must be Ref-free"),
            carina_core::schema::Shape::String { .. }
        ));
        assert_eq!(declared_input_mode, CoreInputMode::ProviderPopulated);
        assert_eq!(description, Some("Provider-assigned identifier"));
        assert_eq!(provider_name, Some("ProviderId"));
        assert_eq!(block_name, None);
        assert!(deferred_populate);
        assert!(!write_only);

        let write_only_field =
            CoreStructField::optional("client_secret", CoreAttributeType::string())
                .with_description("Write-only client secret")
                .with_provider_name("ClientSecret")
                .with_block_name("client_secret_block")
                .write_only()
                .build();
        let wire = core_struct_field_to_proto_for_schema_round_trip(&write_only_field);
        assert!(wire.write_only);
        let json = serde_json::to_string(&wire).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["write_only"],
            true
        );
        let round_tripped =
            proto_struct_field_to_core(&serde_json::from_str::<proto::StructField>(&json).unwrap())
                .unwrap();
        assert!(round_tripped.parts().write_only);

        let required_field = CoreStructField::required("name", CoreAttributeType::string()).build();
        let proto_required = core_struct_field_to_proto_for_schema_round_trip(&required_field);
        assert!(proto_required.required);
        assert!(!proto_required.read_only);
        let round_tripped_required = proto_struct_field_to_core(&proto_required).unwrap();
        assert_eq!(
            round_tripped_required.parts().declared_input_mode,
            CoreInputMode::Required
        );

        let core_attr = CoreAttributeSchema::optional("status", CoreAttributeType::string())
            .with_default(CoreValue::Concrete(ConcreteValue::String(
                "provider-default".into(),
            )))
            .with_description("Eventually populated status")
            .with_provider_name("Status")
            .create_only()
            .non_removable()
            .with_block_name("status_block")
            .write_only()
            .identity()
            .build();
        let attr_json =
            serde_json::to_string(&core_attr_schema_to_proto_for_schema_round_trip(&core_attr))
                .unwrap();
        let proto_attr: proto::AttributeSchema = serde_json::from_str(&attr_json).unwrap();
        let round_tripped_attr = proto_attr_schema_to_core(&proto_attr).unwrap();
        let CoreAttributeSchemaParts {
            name,
            attr_type,
            declared_input_mode,
            default,
            description,
            completions,
            provider_name,
            create_only,
            removable,
            block_name,
            write_only,
            identity,
            deferred_populate,
        } = round_tripped_attr.parts();
        assert_eq!(name, "status");
        assert!(matches!(
            attr_type
                .shape_ref_free()
                .expect("round-tripped attribute type must be Ref-free"),
            carina_core::schema::Shape::String { .. }
        ));
        assert_eq!(declared_input_mode, CoreInputMode::Optional);
        assert!(matches!(
            default,
            Some(CoreValue::Concrete(ConcreteValue::String(value)))
                if value == "provider-default"
        ));
        assert_eq!(description, Some("Eventually populated status"));
        assert!(completions.is_none());
        assert_eq!(provider_name, Some("Status"));
        assert!(create_only);
        assert_eq!(removable, Some(false));
        assert_eq!(block_name, Some("status_block"));
        assert!(write_only);
        assert!(identity);
        assert!(!deferred_populate);

        let provider_populated_attr =
            CoreAttributeSchema::provider_populated("arn", CoreAttributeType::string())
                .with_description("Provider ARN")
                .with_provider_name("Arn")
                .deferred_populate()
                .build();
        let proto_provider_populated =
            core_attr_schema_to_proto_for_schema_round_trip(&provider_populated_attr);
        assert!(proto_provider_populated.read_only);
        assert!(!proto_provider_populated.write_only);
        let round_tripped_provider_populated =
            proto_attr_schema_to_core(&proto_provider_populated).unwrap();
        assert_eq!(
            round_tripped_provider_populated.input_mode(),
            CoreInputMode::ProviderPopulated
        );
        assert!(!round_tripped_provider_populated.is_write_only());
        assert!(round_tripped_provider_populated.is_deferred_populate());

        let required_attr = CoreAttributeSchema::required("region", CoreAttributeType::string())
            .with_description("Required region")
            .with_provider_name("Region")
            .with_block_name("region_block")
            .create_only()
            .identity()
            .build();
        let proto_required = core_attr_schema_to_proto_for_schema_round_trip(&required_attr);
        assert!(proto_required.required);
        assert!(!proto_required.read_only);
        let round_tripped_required = proto_attr_schema_to_core(&proto_required).unwrap();
        assert_eq!(round_tripped_required.input_mode(), CoreInputMode::Required);
        assert_eq!(round_tripped_required.default_value(), None);
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
    /// WASM-boundary converter during desired normalization is a stage-2
    /// invariant violation (`WasmProviderNormalizer::normalize_desired` must
    /// hide its containing attribute first). The converters now return `Err(SerializationError::
    /// UnknownNotAllowed)` so a silent `format!("{v:?}")` fallback
    /// would re-introduce the v1 corruption bug (#2375); these tests
    /// pin the contract by asserting the variant + context survives.
    #[test]
    fn unknown_returns_err_in_core_to_wit_value() {
        use carina_core::resource::{AccessPath, UnknownReason};
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let v = CoreValue::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
            path: path.clone(),
        }));
        let err = core_to_wit_value(&v).unwrap_err();
        match err {
            SerializationError::UnknownNotAllowed {
                reason: UnknownReason::UpstreamExport { path: p },
                context: SerializationContext::WasmBoundary,
            } => assert_eq!(p, path),
            other => {
                panic!("expected UnknownNotAllowed/UpstreamExport/WasmBoundary, got: {other:?}")
            }
        }
    }

    #[test]
    fn redacted_saved_plan_secret_is_rejected_at_wasm_boundary() {
        let (value, _) = redacted_secret_and_digest("saved-plan-secret");
        assert!(matches!(
            core_to_wit_value(&value),
            Err(SerializationError::RedactedSecretNotAllowed {
                context: SerializationContext::WasmBoundary
            })
        ));
    }

    #[test]
    fn unknown_returns_err_in_core_value_to_json() {
        use carina_core::resource::{AccessPath, UnknownReason};
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let v = CoreValue::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
            path: path.clone(),
        }));
        let err = core_value_to_json(&v).unwrap_err();
        assert!(matches!(
            err,
            SerializationError::UnknownNotAllowed {
                reason: UnknownReason::UpstreamExport { .. },
                context: SerializationContext::WasmBoundary,
            }
        ));
    }

    #[test]
    fn enum_identifier_is_rejected_by_scalar_wit_encoder() {
        let value = CoreValue::Concrete(ConcreteValue::enum_identifier("aws.x.Mode.allow"));

        assert!(matches!(
            core_to_wit_value(&value),
            Err(SerializationError::UnresolvedEnumIdentifier {
                value,
                context: SerializationContext::WasmBoundary,
            }) if value == "aws.x.Mode.allow"
        ));
    }

    #[test]
    fn enum_identifier_is_rejected_by_nested_json_encoder() {
        let value = CoreValue::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "items".to_string() => CoreValue::Concrete(ConcreteValue::List(vec![
                CoreValue::Concrete(ConcreteValue::enum_identifier("aws.x.Mode.allow")),
            ])),
        }));

        assert!(matches!(
            core_value_to_json(&value),
            Err(SerializationError::UnresolvedEnumIdentifier {
                value,
                context: SerializationContext::WasmBoundary,
            }) if value == "aws.x.Mode.allow"
        ));
    }

    #[test]
    fn secret_enum_identifier_error_is_masked_by_scalar_wit_encoder() {
        let plaintext = "scalar-secret-plaintext-must-not-leak";
        let value = CoreValue::Deferred(DeferredValue::Secret(Box::new(CoreValue::Concrete(
            ConcreteValue::enum_identifier(plaintext),
        ))));

        let error = core_to_wit_value(&value).expect_err("the unresolved enum must be rejected");
        let display = error.to_string();
        assert!(!display.contains(plaintext), "{display}");
        assert!(display.contains("secret"), "{display}");
    }

    #[test]
    fn secret_enum_identifier_error_is_masked_by_nested_json_encoder() {
        let plaintext = "nested-secret-plaintext-must-not-leak";
        let value = CoreValue::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "items".to_string() => CoreValue::Deferred(DeferredValue::Secret(Box::new(
                CoreValue::Concrete(ConcreteValue::enum_identifier(plaintext)),
            ))),
        }));

        let error = core_value_to_json(&value).expect_err("the unresolved enum must be rejected");
        let display = error.to_string();
        assert!(!display.contains(plaintext), "{display}");
        assert!(display.contains("secret"), "{display}");
    }

    #[test]
    fn string_resembling_enum_syntax_is_encoded_literally() {
        let value =
            CoreValue::Concrete(ConcreteValue::String("aws.x.Mode.allow".to_string().into()));

        assert!(matches!(
            core_to_wit_value(&value),
            Ok(wit::Value::StrVal(text)) if text == "aws.x.Mode.allow"
        ));
    }

    #[test]
    fn canonical_enum_uses_api_spelling_even_in_persisted_wit_json() {
        let attr_type = carina_core::schema::AttributeType::enum_(
            carina_core::schema::enum_identity("Mode", Some("aws.x")),
            Some(vec!["ALLOW".to_string()]),
            vec![("ALLOW".to_string(), "allow".to_string())],
            None,
            None,
        );
        let canonical = carina_core::resource::EnumValueResolver::new(&attr_type)
            .resolve_raw(&carina_core::resource::RawEnumIdentifier::parse(
                "aws.x.Mode.allow",
            ))
            .unwrap();
        let value = CoreValue::Concrete(ConcreteValue::CanonicalEnum(canonical));

        assert!(matches!(
            core_to_wit_value(&value),
            Ok(wit::Value::StrVal(text)) if text == "ALLOW"
        ));

        let nested = CoreValue::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "mode".to_string() => value,
        }));
        let wit = core_to_wit_persisted_value(&nested).expect("encode persisted WIT JSON");
        let wit::PersistedValue::MapVal(json) = wit else {
            panic!("nested canonical enum must use map-val")
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).expect("valid map-val JSON"),
            serde_json::json!({"mode": "ALLOW"})
        );

        let stored = carina_core::value::PersistedValue::encode_provider_value(&nested, None)
            .expect("encode core persisted JSON");
        assert!(
            stored.as_json()["mode"].get("$enum").is_some(),
            "core storage preserves enum identity and is not the WIT wire encoding"
        );
    }

    #[test]
    fn persisted_state_rejects_live_secret_without_exposing_plaintext() {
        let plaintext = "persisted-boundary-secret-must-not-leak";
        let secret = CoreValue::Deferred(DeferredValue::Secret(Box::new(CoreValue::Concrete(
            ConcreteValue::String(plaintext.into()),
        ))));
        let nested = CoreValue::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "token".to_string() => secret.clone(),
        }));

        for value in [secret, nested] {
            let error = core_to_wit_persisted_value(&value)
                .expect_err("persisted state must contain only redacted secret identity");
            assert!(matches!(
                error,
                SerializationError::SecretNotAllowed {
                    context: SerializationContext::WasmBoundary
                }
            ));
            assert!(!error.to_string().contains(plaintext));
        }
    }

    #[test]
    fn malformed_json_container_values_are_rejected() {
        let values = [
            (
                wit::Value::StringListVal("not json".to_string()),
                "string-list-val",
            ),
            (wit::Value::ListVal("not json".to_string()), "list-val"),
            (wit::Value::MapVal("not json".to_string()), "map-val"),
            (wit::Value::SecretVal("not json".to_string()), "secret-val"),
        ];

        for (value, variant) in values {
            let error = wit_to_core_value(&value)
                .expect_err("malformed JSON must not decode to a substitute value");
            let rendered = error.to_string();
            assert!(rendered.contains(variant), "{rendered}");
            assert_eq!(rendered.matches("invalid JSON").count(), 1, "{rendered}");
            assert!(rendered.contains("line 1 column"), "{rendered}");
            assert!(std::error::Error::source(&error).is_none());
        }

        assert!(wit_to_core_value(&wit::Value::ListVal("{}".to_string())).is_err());
        assert!(wit_to_core_value(&wit::Value::MapVal("[]".to_string())).is_err());
    }

    #[test]
    fn null_json_container_values_are_rejected() {
        let values = [
            wit::Value::ListVal("[null]".to_string()),
            wit::Value::MapVal(r#"{"key":null}"#.to_string()),
            wit::Value::SecretVal("null".to_string()),
        ];

        for value in values {
            assert!(
                wit_to_core_value(&value).is_err(),
                "{value:?} must not decode JSON null"
            );
        }
    }

    #[test]
    fn malformed_secret_error_does_not_expose_payload() {
        let plaintext = "boundary-secret-do-not-log";
        let error = wit_to_core_value(&wit::Value::SecretVal(format!("\"{plaintext}")))
            .expect_err("unterminated secret JSON must fail");

        assert!(!error.to_string().contains(plaintext));
        assert!(!format!("{error:?}").contains(plaintext));
        let rendered = error.to_string();
        assert_eq!(rendered.matches("invalid JSON").count(), 1, "{rendered}");
        assert!(rendered.contains("line 1 column"), "{rendered}");
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn wrong_container_shapes_do_not_expose_payloads() {
        let plaintext = "wrong-shape-secret-do-not-log";
        let scalar = serde_json::to_string(plaintext).unwrap();
        let values = [
            wit::Value::StringListVal(scalar.clone()),
            wit::Value::ListVal(scalar.clone()),
            wit::Value::MapVal(scalar),
            wit::Value::StringListVal(format!(r#"[{{"secret":"{plaintext}"}}]"#)),
        ];

        for value in values {
            let error = wit_to_core_value(&value).expect_err("wrong container shape must fail");
            assert!(!error.to_string().contains(plaintext), "{error}");
            assert!(!format!("{error:?}").contains(plaintext), "{error:?}");
        }
    }

    #[test]
    fn provider_read_state_with_malformed_map_is_rejected() {
        let id = CoreResourceId::with_provider_identity("mock", "test.resource", "example", None);
        let state = wit::State {
            identifier: Some("provider-id".to_string()),
            attributes: vec![(
                "settings".to_string(),
                wit::Value::MapVal("not json".to_string()),
            )],
            exists: true,
        };

        let error = wit_to_core_state(&state, &id).expect_err("malformed state must be rejected");
        assert!(error.to_string().contains("map-val"));
        assert!(error.to_string().contains("settings"));
    }

    #[test]
    fn non_finite_float_nested_in_list_is_rejected_at_wasm_boundary() {
        let value = CoreValue::Concrete(ConcreteValue::List(vec![CoreValue::Concrete(
            ConcreteValue::Float(f64::NAN),
        )]));

        assert!(matches!(
            core_to_wit_value(&value),
            Err(SerializationError::NonFiniteFloat {
                context: SerializationContext::WasmBoundary,
                ..
            })
        ));
    }

    // -- Value roundtrips --

    #[test]
    fn test_scalar_bool_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::Bool(true));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit).unwrap();
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_int_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::Int(42));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit).unwrap();
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_float_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::Float(2.78));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit).unwrap();
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_string_roundtrip() {
        let core = CoreValue::Concrete(ConcreteValue::String("hello".into()));
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit).unwrap();
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
        let back = wit_to_core_value(&wit).unwrap();
        assert_eq!(core, back);
    }

    #[test]
    fn test_string_list_roundtrip_preserves_variant() {
        let core = CoreValue::Concrete(ConcreteValue::StringList(vec![
            "s3:GetObject".into(),
            "s3:ListBucket".into(),
        ]));
        let wit = core_to_wit_value(&core).unwrap();
        assert!(matches!(wit, wit::Value::StringListVal(_)));
        let back = wit_to_core_value(&wit).unwrap();
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
        let back = wit_to_core_value(&wit).unwrap();
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
        let back = wit_to_core_value(&wit).unwrap();
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
        let back = wit_to_core_value(&wit).unwrap();
        assert_eq!(core, back);
    }

    /// #2387: `Value::Deferred(DeferredValue::ResourceRef)` reaching `core_to_wit_value` is a
    /// stage-2 invariant violation now — the scoped strip-and-restore wrapper
    /// in `WasmProviderNormalizer::normalize_desired` removes any attribute
    /// that recursively contains a `ResourceRef` before this boundary. The
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
    fn coalesce_returns_err_in_core_to_wit_value() {
        let value = CoreValue::Deferred(DeferredValue::Coalesce {
            value: Box::new(CoreValue::resource_ref("up", "optional", vec![])),
            default: Box::new(CoreValue::Concrete(ConcreteValue::String(
                "fallback".into(),
            ))),
        });
        assert!(matches!(
            core_to_wit_value(&value),
            Err(SerializationError::UnresolvedCoalesce {
                context: SerializationContext::WasmBoundary,
            })
        ));
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
            name: carina_core::builtins::BuiltinFunction::join,
            args: vec![
                CoreValue::Concrete(ConcreteValue::String("-".into())).into(),
                CoreValue::Deferred(DeferredValue::ResourceRef {
                    path: AccessPath::new("vpc", "id"),
                })
                .into(),
            ],
            source_line: 0.into(),
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
    fn postfix_access_returns_err_in_core_to_wit_value() {
        use carina_core::resource::{
            AccessMissPolicy, PathSegment, Subscript, apply_postfix_access,
        };

        let value = apply_postfix_access(
            CoreValue::Deferred(DeferredValue::FunctionCall {
                name: carina_core::builtins::BuiltinFunction::split,
                args: vec![],
                source_line: 0.into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 1 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("computed access remains deferred");
        let error = core_to_wit_value(&value).unwrap_err();

        assert!(matches!(
            error,
            SerializationError::UnresolvedAccess {
                expression,
                context: SerializationContext::WasmBoundary,
            } if expression == "split()[1]"
        ));
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
        let back = wit_to_core_value(&wit_v).unwrap();
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

    #[test]
    fn string_list_uses_dedicated_wit_variant() {
        let v = CoreValue::Concrete(ConcreteValue::StringList(vec!["p".into()]));
        let wit_v = core_to_wit_value(&v).unwrap();
        match wit_v {
            wit::Value::StringListVal(json) => assert_eq!(json, "[\"p\"]"),
            other => panic!("expected StringListVal, got: {other:?}"),
        }
    }

    // -- ResourceId roundtrip --

    #[test]
    fn test_resource_id_roundtrip() {
        let core = CoreResourceId::with_provider_identity(
            "aws",
            "s3.Bucket",
            "my-bucket",
            Some("west".to_string()),
        );
        let wire = carina_core::protocol_convert::resource_id_to_protocol(&core);
        assert_eq!(wire.provider, "aws");
        assert_eq!(wire.resource_type, "s3.Bucket");
        assert_eq!(wire.identity, "my-bucket");
        let wit = core_to_wit_resource_id(&core);
        assert_eq!(wit.provider, "aws");
        assert_eq!(wit.resource_type, "s3.Bucket");
        assert_eq!(wit.identity, "my-bucket");
        let back = carina_core::protocol_convert::protocol_to_resource_id(&proto::ResourceId {
            provider: wit.provider,
            resource_type: wit.resource_type,
            identity: wit.identity,
        });
        assert_eq!(back.provider_instance, None);
        assert_eq!(back.provider, core.provider);
        assert_eq!(back.resource_type, core.resource_type);
        assert_eq!(back.identity_str(), core.identity_str());
    }

    #[test]
    fn pending_resource_id_wasm_conversion_never_reuses_identity() {
        let pending = CoreResourceId::pending_with_provider(
            "mock",
            "test.resource",
            Some("secondary".to_string()),
        );
        let encoded = carina_core::protocol_convert::resource_id_to_protocol(&pending);
        assert_eq!(encoded.identity, "");
        let wit = core_to_wit_resource_id(&pending);
        assert_eq!(wit.provider, "mock");
        assert_eq!(wit.resource_type, "test.resource");
        assert_eq!(wit.identity, "");

        let wire = proto::ResourceId {
            provider: wit.provider,
            resource_type: wit.resource_type,
            identity: wit.identity,
        };
        let first = carina_core::protocol_convert::protocol_to_resource_id(&wire);
        let second = carina_core::protocol_convert::protocol_to_resource_id(&wire);
        assert_eq!(first.identity_str(), None);
        assert_eq!(second.identity_str(), None);
        assert_eq!(first.provider_instance, None);
        assert_eq!(second.provider_instance, None);
        assert_ne!(first, second);
    }

    #[test]
    fn resource_id_wire_key_matches_origin_main_display_format() {
        let resolved =
            CoreResourceId::with_provider_identity("mock", "test.resource", "example", None);
        let pending = CoreResourceId::pending_with_provider("mock", "test.resource", None);
        let providerless_resolved = CoreResourceId::with_identity("test.resource", "example");
        let providerless_pending = CoreResourceId::pending("test.resource");

        assert_eq!(
            resource_id_wire_key(&resolved),
            "mock.test.resource.example"
        );
        assert_eq!(resource_id_wire_key(&pending), "mock.test.resource");
        assert_eq!(
            resource_id_wire_key(&providerless_resolved),
            "test.resource.example"
        );
        assert_eq!(resource_id_wire_key(&providerless_pending), "test.resource");
    }

    // -- State roundtrip --

    #[test]
    fn test_state_roundtrip() {
        let id = CoreResourceId::with_provider_identity("aws", "s3.Bucket", "my-bucket", None);
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

        let wit = wit::State {
            identifier: core.identifier.clone(),
            attributes: core_to_wit_value_map(&core.attributes).unwrap(),
            exists: core.exists,
        };
        let back = wit_to_core_state(&wit, &id).unwrap();

        assert_eq!(back.id, core.id);
        assert_eq!(back.attributes, core.attributes);
        assert!(back.exists);
    }

    #[test]
    fn test_state_with_identifier_roundtrip() {
        let id = CoreResourceId::with_provider_identity("aws", "ec2.Vpc", "main", None);
        let attrs = HashMap::from([(
            "cidr".into(),
            CoreValue::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        )]);
        let core = CoreState::existing(id.clone(), attrs).with_identifier("vpc-12345");

        let wit = wit::State {
            identifier: core.identifier.clone(),
            attributes: core_to_wit_value_map(&core.attributes).unwrap(),
            exists: core.exists,
        };
        assert_eq!(wit.identifier, Some("vpc-12345".into()));
        let back = wit_to_core_state(&wit, &id).unwrap();
        assert_eq!(back.identifier, Some("vpc-12345".into()));
    }

    #[test]
    fn persisted_state_and_saved_attrs_use_redacted_secret_variants() {
        let (redacted, digest) = redacted_secret_and_digest("wire-secret");
        let id = CoreResourceId::with_provider_identity("aws", "test.Resource", "example", None);
        let state = CoreState::existing(
            id.clone(),
            HashMap::from([("token".to_string(), redacted.clone())]),
        );

        let wit_state = core_to_wit_persisted_state(&state)
            .expect("persisted state must retain typed secret identity");
        assert!(matches!(
            wit_state.attributes.as_slice(),
            [(key, wit::PersistedValue::RedactedSecretVal(value))]
                if key == "token" && value == &digest
        ));

        let saved = carina_core::provider::RawSavedAttrs::from_decoded_values_for_test(
            HashMap::from([(id.clone(), HashMap::from([("token".to_string(), redacted)]))]),
        )
        .lift(&carina_core::schema::SchemaRegistry::new());
        let wit_saved = core_saved_attrs_to_wit(saved.as_provider_saved_attrs(), &id)
            .expect("saved attributes must retain typed secret identity");
        assert!(matches!(
            wit_saved.as_deref(),
            Some([(key, wit::PersistedValue::RedactedSecretVal(value))])
                if key == "token" && value == &digest
        ));
    }

    #[test]
    fn desired_resource_rejects_redacted_secret_placeholder() {
        let (redacted, _) = redacted_secret_and_digest("desired-secret");
        let resource = CoreResource::with_provider("aws", "test.Resource", "example", None)
            .with_attribute("token", redacted);

        assert!(matches!(
            core_to_wit_resource(&resource),
            Err(SerializationError::RedactedSecretNotAllowed {
                context: SerializationContext::WasmBoundary
            })
        ));
    }

    #[test]
    fn only_persisted_redacted_secret_variant_assigns_secret_semantics() {
        let (expected, digest) = redacted_secret_and_digest("wire-secret");
        let marker = format!("_secret:{digest}");
        let plain_wire = wit::Value::StrVal(marker.clone());

        assert_eq!(
            wit_to_core_value(&plain_wire).expect("ordinary provider value"),
            CoreValue::Concrete(ConcreteValue::String(marker.into()))
        );
        let state = wit_to_core_persisted_value_map(&[(
            "token".to_string(),
            wit::PersistedValue::RedactedSecretVal(digest),
        )])
        .expect("persisted state value");
        assert!(matches!(
            state.get("token"),
            Some(CoreValue::Deferred(DeferredValue::RedactedSecret { hash: loaded }))
                if CoreValue::Deferred(DeferredValue::RedactedSecret {
                    hash: loaded.clone()
                }) == expected
        ));
    }

    #[test]
    fn persisted_str_val_never_assigns_secret_semantics() {
        let (_, digest) = redacted_secret_and_digest("plain-wit-marker");
        let marker = format!("_secret:{digest}");
        let state = wit_to_core_persisted_value_map(&[(
            "token".to_string(),
            wit::PersistedValue::StrVal(marker.clone()),
        )])
        .expect("persisted marker-shaped str-val is ordinary text");

        assert_eq!(
            state.get("token"),
            Some(&CoreValue::Concrete(ConcreteValue::String(marker.into())))
        );
    }

    #[test]
    fn persisted_redacted_secret_variant_is_typed_and_validated() {
        let (expected, digest) = redacted_secret_and_digest("typed-wit-secret");
        let state = wit_to_core_persisted_value_map(&[(
            "token".to_string(),
            wit::PersistedValue::RedactedSecretVal(digest),
        )])
        .expect("valid redacted-secret-val");

        assert_eq!(state.get("token"), Some(&expected));
        assert!(
            wit_to_core_persisted_value_map(&[(
                "token".to_string(),
                wit::PersistedValue::RedactedSecretVal("not-a-digest".to_string()),
            )])
            .is_err(),
            "the host must reject invalid redacted-secret-val payloads",
        );
    }

    #[test]
    fn nested_persisted_wit_json_uses_secret_tags_and_escaped_user_keys() {
        let (_, digest) = redacted_secret_and_digest("nested-wit-secret");
        let marker = format!("_secret:{digest}");
        let json = serde_json::json!({
            "plain": marker,
            "typed": {"$secret": digest},
            "$$secret": "user",
        })
        .to_string();
        let state = wit_to_core_persisted_value_map(&[(
            "payload".to_string(),
            wit::PersistedValue::MapVal(json.clone()),
        )])
        .expect("decode persisted nested JSON");
        let Some(CoreValue::Concrete(ConcreteValue::Map(values))) = state.get("payload") else {
            panic!("persisted nested payload must remain a map")
        };

        assert_eq!(
            values.get("plain"),
            Some(&CoreValue::Concrete(ConcreteValue::String(marker.into())))
        );
        assert!(matches!(
            values.get("typed"),
            Some(CoreValue::Deferred(DeferredValue::RedactedSecret { .. }))
        ));
        assert_eq!(
            values.get("$secret"),
            Some(&CoreValue::Concrete(ConcreteValue::String("user".into())))
        );

        let ordinary = wit_to_core_value(&wit::Value::MapVal(json))
            .expect("ordinary provider JSON keeps ordinary semantics");
        assert!(!carina_core::value::contains_redacted_secret(&ordinary));
        let CoreValue::Concrete(ConcreteValue::Map(ordinary_values)) = ordinary else {
            panic!("ordinary nested payload must remain a map")
        };
        let Some(CoreValue::Concrete(ConcreteValue::Map(secret_tag))) =
            ordinary_values.get("typed")
        else {
            panic!("ordinary $secret-shaped JSON must remain a map")
        };
        assert_eq!(
            secret_tag.get("$secret"),
            Some(&CoreValue::Concrete(ConcreteValue::String(digest.into())))
        );
        assert_eq!(
            ordinary_values.get("$$secret"),
            Some(&CoreValue::Concrete(ConcreteValue::String("user".into())))
        );
        assert!(!ordinary_values.contains_key("$secret"));
    }

    #[test]
    fn persisted_core_redacted_secrets_use_the_dedicated_wit_variant() {
        let (redacted, digest) = redacted_secret_and_digest("core-to-wit-secret");
        let id = CoreResourceId::with_provider_identity("mock", "test.Resource", "main", None);
        let state = CoreState::existing(id, HashMap::from([("token".to_string(), redacted)]));
        let wit = core_to_wit_persisted_state(&state).expect("encode persisted state");

        assert!(matches!(
            wit.attributes.as_slice(),
            [(key, wit::PersistedValue::RedactedSecretVal(value))]
                if key == "token" && value == &digest
        ));
        assert!(matches!(
            core_to_wit_value(state.attributes.get("token").unwrap()),
            Err(SerializationError::RedactedSecretNotAllowed {
                context: SerializationContext::WasmBoundary
            })
        ));
    }

    #[test]
    fn persisted_wasm_decode_keeps_raw_legacy_marker_plain() {
        let marker = format!("_secret:argon2:{}", "ab".repeat(32));
        let state = wit_to_core_persisted_value_map(&[(
            "token".to_string(),
            wit::PersistedValue::StrVal(marker.clone()),
        )])
        .expect("legacy-marker-shaped WIT strings are ordinary text");

        assert_eq!(
            state.get("token"),
            Some(&CoreValue::Concrete(ConcreteValue::String(marker.into())))
        );
    }

    #[test]
    fn persisted_wasm_decode_keeps_malformed_legacy_looking_text_plain() {
        let marker = "_secret:argon2:xyz";
        let state = wit_to_core_persisted_value_map(&[(
            "token".to_string(),
            wit::PersistedValue::StrVal(marker.to_string()),
        )])
        .expect("an invalid raw legacy marker is ordinary persisted text");

        assert_eq!(
            state.get("token"),
            Some(&CoreValue::Concrete(ConcreteValue::String(marker.into())))
        );
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
        let back = wit_to_core_value_map(&wit).unwrap();
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
        assert_eq!(wit.id.identity, "my-bucket");

        let back = wit_to_core_resource(&wit).unwrap();
        assert_eq!(back.id, resource.id);
        // Compare resolved attributes
        assert_eq!(back.resolved_attributes(), resource.resolved_attributes());
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
            let wit_err = wire_to_wit_provider_error(err.to_wire());
            let back = wit_to_core_provider_error(wit_err);
            assert!(is_variant(&back), "variant lost for {}", err.variant_name());
            assert_eq!(
                back.detail().context(),
                err.detail().context(),
                "context lost"
            );
        }
    }

    #[test]
    fn test_provider_error_terminal_cause_and_detail_fields_round_trip() {
        // Resource id, complete terminal cause, provider name, and structured
        // fields must survive core -> wire -> WIT -> wire -> core. The
        // operation wrapper renders its actionable typed detail itself.
        let cause = TestOperationError {
            operation: "read",
            source: std::io::Error::other("inner io error"),
        };
        let id = CoreResourceId::with_provider_identity("aws", "s3.Bucket", "my-bucket", None);
        let err = CoreProviderError::api_error_caused_by("Failed to read", cause)
            .for_resource(id.clone())
            .for_provider("aws")
            .with_operation("s3.HeadBucket")
            .with_status(503)
            .with_code("ServiceUnavailable")
            .with_request_id("request-123");

        let wire: proto::ProviderError = err.to_wire();
        assert_eq!(
            wire.cause.as_deref(),
            Some("in operation read: inner io error")
        );
        let wit_err = wire_to_wit_provider_error(wire);
        let back = wit_to_core_provider_error(wit_err);
        assert!(matches!(back, CoreProviderError::ApiError(_)));
        let detail = back.detail();
        assert_eq!(detail.context(), "Failed to read");
        assert_eq!(detail.provider_name.as_deref(), Some("aws"));
        assert_eq!(detail.operation.as_deref(), Some("s3.HeadBucket"));
        assert_eq!(detail.status, Some(503));
        assert_eq!(detail.code.as_deref(), Some("ServiceUnavailable"));
        assert_eq!(detail.request_id.as_deref(), Some("request-123"));
        let rid = detail.resource_id.as_ref().expect("resource_id preserved");
        assert_eq!(rid.provider, "aws");
        assert_eq!(rid.resource_type, "s3.Bucket");
        assert_eq!(rid.identity_str().expect("resolved identity"), "my-bucket");
        let cause_str = detail
            .cause()
            .map(|cause| cause.to_string())
            .expect("cause preserved as string");
        assert_eq!(cause_str, "in operation read: inner io error");
        let rendered = back.to_string();
        assert_eq!(
            rendered.matches("in operation read").count(),
            1,
            "{rendered}"
        );
        assert_eq!(rendered.matches("inner io error").count(), 1, "{rendered}");
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

        let wit_err = wire_to_wit_provider_error(err.to_wire());
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
    fn pending_resource_id_round_trips_on_provider_error_without_routing_instance() {
        let pending =
            CoreResourceId::pending_with_provider("aws", "s3.Bucket", Some("audit".to_string()));
        let err = CoreProviderError::not_found("Bucket identity is pending").for_resource(pending);

        let wire = err.to_wire();
        let wire_id = wire.resource_id.as_ref().expect("resource id encoded");
        assert_eq!(wire_id.identity, "");
        let wit_err = wire_to_wit_provider_error(wire);
        let back = wit_to_core_provider_error(wit_err);
        let id = back
            .detail()
            .resource_id
            .as_deref()
            .expect("resource id decoded");

        assert_eq!(id.provider, "aws");
        assert_eq!(id.resource_type, "s3.Bucket");
        assert_eq!(id.identity_str(), None);
        assert_eq!(id.provider_instance, None);
    }

    #[tokio::test]
    async fn test_update_patch_round_trip_preserves_op_order_and_kinds() {
        use carina_core::resource::Value as CV;
        let (persisted_secret, digest) = redacted_secret_and_digest("patch-secret");
        let resource = CoreResource::new("test", "patch-round-trip")
            .with_attribute("a", CV::Concrete(ConcreteValue::String("alpha".into())))
            .with_attribute("b", CV::Concrete(ConcreteValue::Int(42)))
            .with_attribute(
                "token",
                CV::Concrete(ConcreteValue::String("unchanged".into())),
            );
        let from = CoreState::existing(
            resource.id.clone(),
            HashMap::from([
                ("b".to_string(), CV::Concrete(ConcreteValue::Int(1))),
                (
                    "c".to_string(),
                    CV::Concrete(ConcreteValue::String("removed".into())),
                ),
                ("persisted_token".to_string(), persisted_secret),
                (
                    "token".to_string(),
                    CV::Concrete(ConcreteValue::String("unchanged".into())),
                ),
            ]),
        );
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
        let mut schemas = carina_core::schema::SchemaRegistry::new();
        schemas.insert(
            "",
            carina_core::schema::ResourceSchema::new("test")
                .attribute(
                    carina_core::schema::AttributeSchema::optional(
                        "token",
                        carina_core::schema::AttributeType::string(),
                    )
                    .write_only()
                    .build(),
                )
                .validate()
                .expect("test schema must be valid"),
        );
        let preparation = carina_core::executor::ProviderPreparationContext::new(
            &bindings,
            &module_gate,
            &[],
            &carina_core::provider::NoopNormalizer,
            &[],
            &schemas,
        );
        let request = carina_core::executor::prepare_update_request(
            resource,
            from,
            &["a".to_string(), "b".to_string(), "c".to_string()],
            &preparation,
        )
        .await
        .expect("checked request should be prepared");
        let wit_request = core_to_wit_update_request(&request).unwrap();
        assert!(wit_request.current.attributes.iter().any(|(key, value)| {
            key == "persisted_token"
                && matches!(value, wit::PersistedValue::RedactedSecretVal(value) if value == &digest)
        }));
        let back: Vec<_> = wit_request
            .patch
            .ops
            .iter()
            .map(wit_to_core_patch_op)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(back.len(), 4);
        assert!(matches!(&back[0], CorePatchOp::Add { key, .. } if key == "a"));
        assert_eq!(
            back[0].sent_value(),
            Some(&CV::Concrete(ConcreteValue::String("alpha".into())))
        );
        assert!(matches!(&back[1], CorePatchOp::Replace { key, .. } if key == "b"));
        assert_eq!(
            back[1].sent_value(),
            Some(&CV::Concrete(ConcreteValue::Int(42)))
        );
        assert!(matches!(&back[2], CorePatchOp::Remove { key } if key == "c"));
        assert_eq!(back[2].sent_value(), None);
        assert!(matches!(&back[3], CorePatchOp::Reassert { key, .. } if key == "token"));
        assert_eq!(
            back[3].sent_value(),
            Some(&CV::Concrete(ConcreteValue::String("unchanged".into())))
        );
    }

    #[test]
    fn test_json_to_provider_info_valid() {
        let json = r#"{"name":"aws","display_name":"AWS Provider","version":"1.0.0"}"#;
        let (name, display, version) = json_to_provider_info(json).unwrap();
        assert_eq!(name, "aws");
        assert_eq!(display, "AWS Provider");
        assert_eq!(version, "1.0.0");
    }

    #[test]
    fn test_json_to_provider_info_missing_required_version_is_rejected() {
        let json = r#"{"name":"old","display_name":"Old Provider"}"#;
        assert!(json_to_provider_info(json).is_err());
    }

    #[test]
    fn test_json_to_provider_info_malformed_json_is_rejected() {
        assert!(json_to_provider_info("not json").is_err());
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
    fn test_protocol_version_range_is_v12_only() {
        assert_eq!(carina_provider_protocol::PROTOCOL_VERSION, 12);
        assert_eq!(carina_provider_protocol::MIN_SUPPORTED_PROTOCOL_VERSION, 12);
    }

    #[test]
    fn test_check_protocol_version_rejects_v11_provider() {
        let error = check_protocol_version(
            r#"{"name":"old","display_name":"Old Provider","version":"1.0.0","protocol_version":11}"#,
        )
        .expect_err("a v12 host must reject a v11 provider");

        assert!(error.contains("provider 'old'"));
        assert!(error.contains("protocol version 11"));
        assert!(error.contains("requires at least version 12"));
    }

    #[test]
    fn test_check_protocol_version_rejects_lower_version() {
        let err = check_protocol_version(
            r#"{"name":"aws","display_name":"AWS Provider","version":"1.0.0","protocol_version":4}"#,
        )
        .unwrap_err();

        assert!(err.contains("provider 'aws'"));
        assert!(err.contains("protocol version 4"));
        assert!(err.contains(&format!(
            "requires at least version {}",
            carina_provider_protocol::MIN_SUPPORTED_PROTOCOL_VERSION
        )));
    }

    #[test]
    fn test_check_protocol_version_rejects_future_version() {
        let json = format!(
            r#"{{"name":"aws","display_name":"AWS Provider","version":"1.0.0","protocol_version":{}}}"#,
            carina_provider_protocol::PROTOCOL_VERSION + 1
        );
        let err = check_protocol_version(&json).unwrap_err();

        assert!(err.contains("provider 'aws'"));
        assert!(err.contains(&format!(
            "protocol version {}",
            carina_provider_protocol::PROTOCOL_VERSION + 1
        )));
        assert!(err.contains(&format!(
            "supports up to version {}",
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
            "requires at least version {}",
            carina_provider_protocol::MIN_SUPPORTED_PROTOCOL_VERSION
        )));
    }

    #[test]
    fn test_json_to_schemas_empty() {
        let schemas = json_to_schemas("[]").unwrap();
        assert!(schemas.is_empty());
    }

    #[test]
    fn released_protocol_v12_schema_fixtures_load_through_the_core_seam() {
        let fixtures = [
            (
                "acm.Certificate",
                include_str!(
                    "../tests/fixtures/protocol-v12/aws-acm-domain-validation-options.json"
                ),
            ),
            (
                "ec2.EgressOnlyInternetGateway",
                include_str!(
                    "../tests/fixtures/protocol-v12/released-aws-egress-only-internet-gateway.json"
                ),
            ),
            (
                "ec2.Vpc",
                include_str!("../tests/fixtures/protocol-v12/released-awscc-vpc.json"),
            ),
        ];

        for (expected_resource_type, json) in fixtures {
            let schemas = json_to_schemas(json).unwrap_or_else(|error| {
                panic!(
                    "released fixture {expected_resource_type} must load: {}",
                    render_error_chain(&error)
                )
            });
            assert_eq!(schemas.len(), 1);
            assert_eq!(
                schemas[0].as_schema().resource_type(),
                expected_resource_type
            );
            match expected_resource_type {
                "ec2.EgressOnlyInternetGateway" => {
                    let vpc_id = schemas[0].parts().attributes["vpc_id"].attr_type();
                    assert!(matches!(
                        vpc_id.raw_shape(),
                        carina_core::schema::RawShape::String {
                            identity: Some(identity),
                            pattern: Some("^vpc-[0-9a-f]{8,}$"),
                            ..
                        } if identity.to_string() == "aws.ec2.Vpc.Id"
                    ));
                }
                "ec2.Vpc" => {
                    let tenancy = schemas[0].parts().attributes["instance_tenancy"].attr_type();
                    assert!(matches!(
                        tenancy.raw_shape(),
                        carina_core::schema::RawShape::Enum {
                            identity,
                            values: Some([first, second, third]),
                            ..
                        } if identity.to_string() == "aws.ec2.Vpc.InstanceTenancy"
                            && first == "default"
                            && second == "dedicated"
                            && third == "host"
                    ));
                }
                "acm.Certificate" => {}
                other => panic!("unhandled released fixture {other}"),
            }
        }
    }

    #[test]
    fn released_acm_optional_deferred_descendant_under_read_only_output_loads() {
        let schemas = json_to_schemas(
            r#"[{
                "resource_type":"acm.Certificate",
                "attributes":{
                    "domain_validation_options":{
                        "name":"domain_validation_options",
                        "attr_type":{
                            "type":"list",
                            "element_type":{
                                "type":"struct",
                                "name":"DomainValidation",
                                "fields":[{
                                    "name":"domain_name",
                                    "field_type":{"type":"String"},
                                    "required":true,
                                    "read_only":false,
                                    "write_only":false,
                                    "deferred_populate":false,
                                    "provider_name":"DomainName"
                                },{
                                    "name":"http_redirect",
                                    "field_type":{
                                        "type":"struct",
                                        "name":"HttpRedirect",
                                        "fields":[{
                                            "name":"redirect_from",
                                            "field_type":{"type":"String"},
                                            "required":false,
                                            "read_only":false,
                                            "write_only":false,
                                            "deferred_populate":false,
                                            "provider_name":"RedirectFrom"
                                        },{
                                            "name":"redirect_to",
                                            "field_type":{"type":"String"},
                                            "required":false,
                                            "read_only":false,
                                            "write_only":false,
                                            "deferred_populate":false,
                                            "provider_name":"RedirectTo"
                                        }]
                                    },
                                    "required":false,
                                    "read_only":false,
                                    "write_only":false,
                                    "deferred_populate":false,
                                    "provider_name":"HttpRedirect"
                                },{
                                    "name":"resource_record",
                                    "field_type":{
                                        "type":"struct",
                                        "name":"ResourceRecord",
                                        "fields":[{
                                            "name":"name",
                                            "field_type":{"type":"String"},
                                            "required":true,
                                            "read_only":false,
                                            "write_only":false,
                                            "deferred_populate":false,
                                            "provider_name":"Name"
                                        },{
                                            "name":"type",
                                            "field_type":{
                                                "type":"string_enum",
                                                "values":["CNAME"],
                                                "name":"Type",
                                                "namespace":"aws.acm.Certificate.DomainValidation.ResourceRecord",
                                                "dsl_aliases":[["CNAME","cname"]]
                                            },
                                            "required":true,
                                            "read_only":false,
                                            "write_only":false,
                                            "deferred_populate":false,
                                            "provider_name":"Type"
                                        },{
                                            "name":"value",
                                            "field_type":{"type":"String"},
                                            "required":true,
                                            "read_only":false,
                                            "write_only":false,
                                            "deferred_populate":false,
                                            "provider_name":"Value"
                                        }]
                                    },
                                    "required":false,
                                    "read_only":false,
                                    "write_only":false,
                                    "deferred_populate":true,
                                    "provider_name":"ResourceRecord"
                                },{
                                    "name":"validation_domain",
                                    "field_type":{"type":"String"},
                                    "required":false,
                                    "read_only":false,
                                    "write_only":false,
                                    "deferred_populate":false,
                                    "provider_name":"ValidationDomain"
                                },{
                                    "name":"validation_emails",
                                    "field_type":{
                                        "type":"list",
                                        "element_type":{"type":"String","identity":"Email"},
                                        "ordered":true
                                    },
                                    "required":false,
                                    "read_only":false,
                                    "write_only":false,
                                    "deferred_populate":false,
                                    "provider_name":"ValidationEmails"
                                },{
                                    "name":"validation_method",
                                    "field_type":{
                                        "type":"string_enum",
                                        "values":["DNS","EMAIL","HTTP"],
                                        "name":"ValidationMethod",
                                        "namespace":"aws.acm.Certificate.DomainValidation",
                                        "dsl_aliases":[
                                            ["DNS","dns"],
                                            ["EMAIL","email"],
                                            ["HTTP","http"]
                                        ]
                                    },
                                    "required":false,
                                    "read_only":false,
                                    "write_only":false,
                                    "deferred_populate":false,
                                    "provider_name":"ValidationMethod"
                                },{
                                    "name":"validation_status",
                                    "field_type":{
                                        "type":"string_enum",
                                        "values":["FAILED","PENDING_VALIDATION","SUCCESS"],
                                        "name":"ValidationStatus",
                                        "namespace":"aws.acm.Certificate.DomainValidation",
                                        "dsl_aliases":[
                                            ["FAILED","failed"],
                                            ["PENDING_VALIDATION","pending_validation"],
                                            ["SUCCESS","success"]
                                        ]
                                    },
                                    "required":false,
                                    "read_only":false,
                                    "write_only":false,
                                    "deferred_populate":false,
                                    "provider_name":"ValidationStatus"
                                }]
                            },
                            "ordered":true
                        },
                        "required":false,
                        "read_only":true,
                        "write_only":false,
                        "provider_name":"DomainValidationOptions"
                    }
                }
            }]"#,
        )
        .expect("the released version-12 ACM schema must remain loadable");

        let validated_schema = &schemas[0];
        let schema = validated_schema.as_schema();
        let domain_validation_options = schema
            .attributes()
            .get("domain_validation_options")
            .expect("domain_validation_options");
        let list_element = match domain_validation_options.attr_type().raw_shape() {
            carina_core::schema::RawShape::List { element_type, .. } => element_type,
            other => panic!("expected list, got {other:?}"),
        };
        let fields = match list_element.raw_shape() {
            carina_core::schema::RawShape::Struct { fields, .. } => fields,
            other => panic!("expected struct, got {other:?}"),
        };
        let domain_name = fields
            .iter()
            .find(|field| field.name() == "domain_name")
            .expect("domain_name");
        assert_eq!(
            domain_name.parts().declared_input_mode,
            CoreInputMode::Required
        );

        let resource_record = fields
            .iter()
            .find(|field| field.name() == "resource_record")
            .expect("resource_record");
        assert_eq!(
            resource_record.parts().declared_input_mode,
            CoreInputMode::Optional
        );
        assert!(resource_record.parts().deferred_populate);
        let record_fields = match resource_record.field_type().raw_shape() {
            carina_core::schema::RawShape::Struct { fields, .. } => fields,
            other => panic!("expected resource record struct, got {other:?}"),
        };
        assert!(
            record_fields
                .iter()
                .all(|field| field.parts().declared_input_mode == CoreInputMode::Required)
        );

        let domain_name = validated_schema
            .effective_node_at_path(&[
                "domain_validation_options".to_string(),
                "domain_name".to_string(),
            ])
            .expect("domain_name effective path must resolve");
        assert_eq!(
            domain_name.effective_input_mode(),
            CoreInputMode::ProviderPopulated
        );
        assert!(domain_name.is_read_only());

        let resource_record = validated_schema
            .effective_node_at_path(&[
                "domain_validation_options".to_string(),
                "resource_record".to_string(),
            ])
            .expect("resource_record effective path must resolve");
        assert_eq!(
            resource_record.effective_input_mode(),
            CoreInputMode::ProviderPopulated
        );
        assert!(resource_record.is_deferred_populate());
    }

    #[test]
    fn test_json_to_schemas_requires_nested_write_only_flag() {
        let err = json_to_schemas(
            r#"[{
                "resource_type":"test.Resource",
                "attributes":{
                    "config":{
                        "name":"config",
                        "attr_type":{
                            "type":"struct",
                            "name":"Config",
                            "fields":[{
                                "name":"client_secret",
                                "field_type":{"type":"String"},
                                "required":true
                            }]
                        },
                        "required":true
                    }
                }
            }]"#,
        )
        .expect_err("protocol v12 schema JSON must declare nested write_only");

        let rendered = render_error_chain(&err);
        assert!(rendered.contains("schema JSON parse error"));
        assert!(rendered.contains("missing field `write_only`"));
    }

    #[test]
    fn test_json_to_schemas_malformed_unique_name_spec_returns_error() {
        let err = json_to_schemas(
            r#"[{
                "resource_type":"s3.Bucket",
                "attributes":{},
                "unique_name_attribute":{"type":"attribute"}
            }]"#,
        )
        .unwrap_err();
        let rendered = render_error_chain(&err);

        assert!(
            rendered.contains("schema JSON parse error"),
            "error should identify schema JSON parsing: {rendered}"
        );
        assert!(
            rendered.contains("missing field `value`"),
            "error should include serde detail: {rendered}"
        );
    }

    #[test]
    fn test_json_to_schemas_with_complex_attributes() {
        let json = r#"[
          {
            "resource_type": "ec2.SecurityGroup",
            "description": "EC2 Security Group",
            "unique_name_attribute": "group_name",
            "attributes": {
              "group_name": {
                "name": "group_name",
                "attr_type": { "type": "String" },
                "required": true,
                "description": "Group name",
                "create_only": true,
                "read_only": false,
                "write_only": false
              },
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
                            "write_only": false,
                            "description": "Start of port range",
                            "block_name": "from_port_block",
                            "provider_name": "FromPort"
                          },
                          {
                            "name": "protocol",
                            "field_type": { "type": "String" },
                            "required": true,
                            "write_only": false,
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
        let CoreResourceSchemaParts {
            resource_type,
            attributes,
            description,
            validation,
            kind,
            unique_name,
            operation_config,
            exclusive_required,
            default_wait_timeout,
            default_wait_interval,
            defs,
        } = schema.parts();
        assert_eq!(resource_type, "ec2.SecurityGroup");
        assert_eq!(description, Some("EC2 Security Group"));
        assert!(!matches!(kind, carina_core::schema::SchemaKind::DataSource));
        assert_eq!(
            unique_name,
            &CoreUniqueNameSpec::Attribute("group_name".to_string())
        );
        let _ = (
            validation,
            operation_config,
            exclusive_required,
            default_wait_timeout,
            default_wait_interval,
            defs,
        );

        // Basic attribute types
        let desc_attr = attributes
            .get("description")
            .expect("description attribute");
        assert_eq!(desc_attr.name(), "description");
        assert!(matches!(
            desc_attr
                .attr_type()
                .shape_ref_free()
                .expect("test schema is Ref-free"),
            carina_core::schema::Shape::String { .. }
        ));
        assert!(desc_attr.is_required());

        let enabled_attr = attributes.get("enabled").expect("enabled attribute");
        assert!(matches!(
            enabled_attr
                .attr_type()
                .shape_ref_free()
                .expect("test schema is Ref-free"),
            carina_core::schema::Shape::Bool
        ));

        let priority_attr = attributes.get("priority").expect("priority attribute");
        assert!(matches!(
            priority_attr
                .attr_type()
                .shape_ref_free()
                .expect("test schema is Ref-free"),
            carina_core::schema::Shape::Int { .. }
        ));

        // Ingress attribute: list with ordered=false, provider_name, block_name, removable
        let ingress_attr = attributes.get("ingress").expect("ingress attribute");
        assert_eq!(ingress_attr.provider_name(), Some("IpPermissions"));
        assert_eq!(ingress_attr.block_name(), Some("ingress_block"));
        assert_eq!(ingress_attr.removability_override(), Some(false));

        // List with ordered: false
        match ingress_attr
            .attr_type()
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
                                assert_eq!(from_port.name(), "from_port");
                                assert!(matches!(
                                    from_port
                                        .field_type()
                                        .shape_ref_free()
                                        .expect("test schema is Ref-free"),
                                    carina_core::schema::Shape::Int { .. }
                                ));
                                assert_eq!(
                                    from_port.parts().declared_input_mode,
                                    CoreInputMode::Required
                                );
                                assert_eq!(from_port.description(), Some("Start of port range"));
                                assert_eq!(from_port.parts().block_name, Some("from_port_block"));
                                assert_eq!(from_port.provider_name(), Some("FromPort"));

                                let protocol = &fields[1];
                                assert_eq!(protocol.name(), "protocol");
                                assert!(matches!(
                                    protocol
                                        .field_type()
                                        .shape_ref_free()
                                        .expect("test schema is Ref-free"),
                                    carina_core::schema::Shape::String { .. }
                                ));
                                assert!(protocol.parts().block_name.is_none());
                                assert!(protocol.provider_name().is_none());
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
                .parts()
                .get("timeout")
                .map(|t| t.shape_ref_free().expect("test schema is Ref-free")),
            Some(carina_core::schema::Shape::Duration)
        ));
    }

    #[test]
    fn json_to_attribute_types_rejects_malformed_json() {
        let error = json_to_attribute_types("not json").unwrap_err();
        let rendered = render_error_chain(&error);
        assert_eq!(rendered.matches("schema JSON parse error").count(), 1);
        assert!(rendered.contains("expected ident"), "{rendered}");
        assert!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<serde_json::Error>())
                .is_some(),
            "JSON parse error must remain reachable through Error::source"
        );
    }

    #[test]
    fn config_schema_validation_error_remains_the_source_with_all_issues() {
        let error = json_to_attribute_types(
            r#"{
                "first":{"type":"ref","name":"MissingFirst"},
                "second":{"type":"ref","name":"MissingSecond"}
            }"#,
        )
        .expect_err("both dangling config references must be rejected");

        let source =
            find_error_in_chain::<carina_core::schema::ConfigSchemaValidationError>(&error)
                .expect("typed config validation error must remain in the chain");
        assert_eq!(source.issues().len(), 2);
        assert_eq!(error.to_string(), "schema protocol conversion failed");
        let rendered = render_error_chain(&error);
        assert_eq!(
            rendered
                .matches("schema protocol conversion failed")
                .count(),
            1,
            "the host wrapper must not repeat the core conversion error: {rendered}"
        );
        assert!(rendered.contains("MissingFirst"), "{rendered}");
        assert!(rendered.contains("MissingSecond"), "{rendered}");
        assert_eq!(rendered.matches("MissingFirst").count(), 1, "{rendered}");
        assert_eq!(rendered.matches("MissingSecond").count(), 1, "{rendered}");
    }

    #[test]
    fn resource_schema_validation_error_remains_the_source_with_all_issues() {
        let schema = proto_resource(
            "test.Widget",
            [
                (
                    "first".to_string(),
                    proto_attr(
                        "first",
                        proto::AttributeType::Ref {
                            name: "MissingFirst".to_string(),
                        },
                    ),
                ),
                (
                    "second".to_string(),
                    proto_attr(
                        "second",
                        proto::AttributeType::Ref {
                            name: "MissingSecond".to_string(),
                        },
                    ),
                ),
            ],
        );
        let error = proto_schema_to_core(&schema)
            .expect_err("both dangling resource references must be rejected");

        let source =
            find_error_in_chain::<carina_core::schema::ResourceSchemaValidationError>(&error)
                .expect("typed resource validation error must remain in the chain");
        assert_eq!(source.issues().len(), 2);
        assert_eq!(error.to_string(), "schema protocol conversion failed");
        let rendered = render_error_chain(&error);
        assert_eq!(
            rendered
                .matches("schema protocol conversion failed")
                .count(),
            1,
            "the host wrapper label must occur once: {rendered}"
        );
        assert!(rendered.contains("MissingFirst"), "{rendered}");
        assert!(rendered.contains("MissingSecond"), "{rendered}");
        assert_eq!(rendered.matches("MissingFirst").count(), 1, "{rendered}");
        assert_eq!(rendered.matches("MissingSecond").count(), 1, "{rendered}");
    }

    #[test]
    fn resource_schema_non_attribute_issue_is_not_rendered_as_an_attribute() {
        let mut schema = proto_resource(
            "test.Widget",
            [("known".to_string(), proto_attr("known", proto_string()))],
        );
        schema.exclusive_required = vec![vec!["missing".to_string()]];

        let error = proto_schema_to_core(&schema)
            .expect_err("unknown exclusive-required member must be rejected");
        let rendered = render_error_chain(&error);

        assert!(rendered.contains("exclusive_required[0]"), "{rendered}");
        assert!(
            !rendered.contains("attribute 'exclusive_required[0]'"),
            "{rendered}"
        );
    }

    #[test]
    fn resource_schema_definition_issue_is_not_rendered_as_an_attribute() {
        let mut schema = proto_resource("test.Widget", std::iter::empty());
        schema.defs.insert(
            "Tag".to_string(),
            proto::AttributeType::Struct {
                name: "Tag".to_string(),
                fields: vec![proto::StructField {
                    name: "key".to_string(),
                    field_type: proto::AttributeType::Ref {
                        name: "Missing".to_string(),
                    },
                    required: false,
                    description: None,
                    block_name: None,
                    provider_name: None,
                    read_only: false,
                    write_only: false,
                    deferred_populate: false,
                }],
            },
        );

        let error = proto_schema_to_core(&schema)
            .expect_err("a dangling definition reference must be rejected");
        let rendered = render_error_chain(&error);

        assert!(rendered.contains("defs.Tag.key"), "{rendered}");
        assert!(!rendered.contains("attribute 'defs.Tag.key'"), "{rendered}");
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
        let back = wit_to_core_value(&wit).unwrap();
        assert_eq!(policies, back);
    }

    #[test]
    fn test_proto_schema_with_tags_validator_reconstructed() {
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.s3.Bucket".to_string(),
            attributes: BTreeMap::from([(
                "tags".to_string(),
                proto_attr(
                    "tags",
                    proto::AttributeType::Map {
                        inner: Box::new(proto_string()),
                        key: Box::new(proto_string()),
                    },
                ),
            )]),
            description: None,
            kind: proto::SchemaKind::Managed,
            unique_name: proto::UniqueNameSpec::Conflicting,
            operation_config: None,
            validators: vec![proto::ValidatorType::TagsKeyValueCheck],
            exclusive_required: vec![],
            defs: Default::default(),
        };
        let core_schema = proto_schema_to_core(&proto_schema).unwrap();
        assert!(matches!(
            core_schema.parts().validation,
            carina_core::schema::ResourceValidation::Declared(validators)
                if validators == &[proto::ValidatorType::TagsKeyValueCheck]
        ));
    }

    #[test]
    fn test_proto_schema_without_validators_has_no_validator() {
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.s3.Bucket".to_string(),
            attributes: BTreeMap::new(),
            description: None,
            kind: proto::SchemaKind::Managed,
            unique_name: proto::UniqueNameSpec::Conflicting,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![],
            defs: Default::default(),
        };
        let core_schema = proto_schema_to_core(&proto_schema).unwrap();
        assert!(matches!(
            core_schema.parts().validation,
            carina_core::schema::ResourceValidation::None
        ));
    }

    #[test]
    fn test_exclusive_required_roundtrips_through_proto() {
        // Declarative exclusive_required must survive the proto boundary so
        // WASM providers can express `oneOf` constraints as data.
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.ec2.Vpc".to_string(),
            attributes: [
                (
                    "cidr_block".to_string(),
                    proto_attr("cidr_block", proto_string()),
                ),
                (
                    "ipv4_ipam_pool_id".to_string(),
                    proto_attr("ipv4_ipam_pool_id", proto_string()),
                ),
            ]
            .into_iter()
            .collect(),
            description: None,
            kind: proto::SchemaKind::Managed,
            unique_name: proto::UniqueNameSpec::Conflicting,
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
            core_schema.parts().exclusive_required,
            &[vec![
                "cidr_block".to_string(),
                "ipv4_ipam_pool_id".to_string(),
            ]]
        );

        // And the resulting core schema rejects empty attributes.
        let mut registry = CoreSchemaRegistry::new();
        registry.insert("awscc", core_schema);
        let resolver = carina_core::parser::ProviderCustomTypes::default();
        let err = registry
            .get("awscc", "awscc.ec2.Vpc", CoreSchemaKind::Resource)
            .expect("decoded fixture must be owner-bound")
            .validate_with_resolver(&HashMap::new(), &|_| false, &resolver)
            .unwrap_err();
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
            attributes: [
                ("a".to_string(), proto_attr("a", proto_string())),
                ("b".to_string(), proto_attr("b", proto_string())),
            ]
            .into_iter()
            .collect(),
            description: None,
            kind: proto::SchemaKind::Managed,
            unique_name: proto::UniqueNameSpec::Conflicting,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![vec!["a".to_string(), "b".to_string()]],
            defs: Default::default(),
        };
        let json = serde_json::to_string(&vec![proto_schema]).unwrap();
        let schemas = json_to_schemas(&json).unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(
            schemas[0].parts().exclusive_required,
            &[vec!["a".to_string(), "b".to_string()]]
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
            carina_core::resource::ConcreteValue::String("ap-northeast-1a".into()),
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
            carina_core::resource::ConcreteValue::String("garbage-not-an-az".into()),
        );
        assert_eq!(
            carina_core::utils::lift_enum_leaves(&garbage, &core_attr),
            None,
            "unknown dynamic enum strings must remain String for strict validation"
        );

        let az_shaped_garbage = carina_core::resource::Value::Concrete(
            carina_core::resource::ConcreteValue::String("foo_bar_42".into()),
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
                carina_core::resource::ConcreteValue::String(raw.to_string().into()),
            );
            assert_eq!(
                carina_core::utils::lift_enum_leaves(&value, &core_attr),
                None,
                "{raw:?} must remain a String for strict validation"
            );
        }

        let valid = carina_core::resource::Value::Concrete(
            carina_core::resource::ConcreteValue::String("ap-northeast-1a".into()),
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
        assert!(matches!(
            find_error_in_chain::<carina_core::protocol_convert::ProtocolConvertError>(&err),
            Some(
                carina_core::protocol_convert::ProtocolConvertError::LegacyCustomUnsupportedBase {
                    identity,
                    base_kind: "Bool",
                }
            ) if identity == "aws.ec2.Unsupported.Id"
        ));
    }

    #[test]
    fn non_null_opaque_validator_payload_is_rejected() {
        let payloads = [
            r#"{
                "type":"String",
                "identity":"test.ValidatedString",
                "validate":true
            }"#,
            r#"{
                "type":"list",
                "element_type":{"type":"String"},
                "ordered":true,
                "validate":{"legacy":"callback"}
            }"#,
            r#"{
                "type":"custom",
                "name":"test.Legacy",
                "base":{"type":"String"},
                "validate":"opaque"
            }"#,
        ];

        for payload in payloads {
            let wire: proto::AttributeType = serde_json::from_str(payload).unwrap();
            let error = proto_attr_type_to_core(&wire)
                .expect_err("opaque executable validators must not be silently discarded");
            let rendered = render_error_chain(&error);
            assert!(
                rendered.contains("opaque validator"),
                "unexpected error: {rendered}"
            );
        }
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
        let attr_type = schemas[0].parts().attributes["id"].attr_type();
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
    fn old_custom_list_payload_rejects_unrepresentable_enclosing_identity() {
        let json = r#"{
            "type":"custom",
            "name":"awscc.wafv2.StatementList",
            "base":{
                "type":"list",
                "inner":{"type":"String"},
                "ordered":false
            },
            "length":[1,5],
            "validate":null
        }"#;
        let proto_attr: proto::AttributeType = serde_json::from_str(json).unwrap();
        let error = proto_attr_type_to_core(&proto_attr)
            .expect_err("core cannot bind an enclosing custom identity to a List");
        assert!(matches!(
            find_error_in_chain::<carina_core::protocol_convert::ProtocolConvertError>(&error),
            Some(
                carina_core::protocol_convert::ProtocolConvertError::LegacyCustomUnsupportedBase {
                    identity,
                    base_kind: "List",
                }
            ) if identity == "awscc.wafv2.StatementList"
        ));
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
                "length":[1,3]
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
    fn protocol_json_reconstructs_every_core_builtin_validator_by_identity() {
        for (kind, expected_identity) in carina_core::schema::builtin_validator_registry().keys() {
            let wire_type = match kind {
                carina_core::schema::RefinedScalarKind::String => proto::AttributeType::String {
                    pattern: None,
                    length: None,
                    validate: None,
                    to_dsl: None,
                    identity: Some(expected_identity.to_string()),
                },
                carina_core::schema::RefinedScalarKind::Int => proto::AttributeType::Int {
                    range: None,
                    identity: Some(expected_identity.to_string()),
                },
                carina_core::schema::RefinedScalarKind::Float => proto::AttributeType::Float {
                    range: None,
                    identity: Some(expected_identity.to_string()),
                },
            };
            let json = serde_json::to_string(&wire_type).expect("builtin type must serialize");
            let decoded_wire: proto::AttributeType =
                serde_json::from_str(&json).expect("builtin type JSON must decode");
            let decoded =
                proto_attr_type_to_core(&decoded_wire).expect("builtin identity must decode");

            let (actual_identity, has_callback) = match decoded.raw_shape() {
                carina_core::schema::RawShape::String {
                    identity,
                    pattern: _,
                    length: _,
                    validate,
                    to_dsl: _,
                } => (identity, validate.is_some()),
                carina_core::schema::RawShape::Int {
                    identity,
                    range: _,
                    validate,
                } => (identity, validate.is_some()),
                carina_core::schema::RawShape::Float {
                    identity,
                    range: _,
                    validate,
                } => (identity, validate.is_some()),
                other => panic!("unexpected builtin shape for {expected_identity}: {other:?}"),
            };
            assert_eq!(actual_identity, Some(expected_identity));
            assert!(
                has_callback,
                "host decode dropped the callback for {expected_identity}"
            );
        }
    }

    #[test]
    fn test_tags_validator_detects_key_value_pattern() {
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.s3.Bucket".to_string(),
            attributes: BTreeMap::from([(
                "tags".to_string(),
                proto_attr(
                    "tags",
                    proto::AttributeType::Map {
                        inner: Box::new(proto_string()),
                        key: Box::new(proto_string()),
                    },
                ),
            )]),
            description: None,
            kind: proto::SchemaKind::Managed,
            unique_name: proto::UniqueNameSpec::Conflicting,
            operation_config: None,
            validators: vec![proto::ValidatorType::TagsKeyValueCheck],
            exclusive_required: vec![],
            defs: Default::default(),
        };
        let core_schema = proto_schema_to_core(&proto_schema).unwrap();
        assert!(matches!(
            core_schema.parts().validation,
            carina_core::schema::ResourceValidation::Declared(validators)
                if validators == &[proto::ValidatorType::TagsKeyValueCheck]
        ));
        let mut registry = CoreSchemaRegistry::new();
        registry.insert("awscc", core_schema);
        let resolver = carina_core::parser::ProviderCustomTypes::default();
        let bound_schema = registry
            .get("awscc", "awscc.s3.Bucket", CoreSchemaKind::Resource)
            .expect("decoded fixture must be owner-bound");

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
        assert!(
            bound_schema
                .validate_with_resolver(&attrs, &|_| false, &resolver)
                .is_err()
        );

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
        assert!(
            bound_schema
                .validate_with_resolver(&attrs, &|_| false, &resolver)
                .is_ok()
        );
    }
}
