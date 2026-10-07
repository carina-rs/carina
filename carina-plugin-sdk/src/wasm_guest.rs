//! WASM guest-side helpers for the carina-provider WIT interface.
//!
//! Provides type conversion functions between WIT guest types and protocol types.
//! The `export_provider!` macro generates the wit-bindgen bindings and Guest trait
//! implementation in the consumer crate.

use carina_provider_protocol::types as proto;

// -- JSON conversion helpers --

/// JSON-backed WIT value variant being decoded.
#[derive(Debug, Clone, Copy)]
pub enum WitValueVariant {
    StringListVal,
    ListVal,
    MapVal,
    SecretVal,
}

impl std::fmt::Display for WitValueVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StringListVal => "string-list-val",
            Self::ListVal => "list-val",
            Self::MapVal => "map-val",
            Self::SecretVal => "secret-val",
        })
    }
}

/// Failure to decode a JSON-backed value received from the WASM host.
///
/// The raw JSON is never retained because `secret-val` can contain plaintext
/// credentials. Syntax errors retain their structured serde source.
#[derive(Debug)]
pub struct WasmValueDecodeError {
    wit_variant: WitValueVariant,
    attribute_path: Vec<String>,
    kind: WasmValueDecodeErrorKind,
}

#[derive(Debug)]
enum WasmValueDecodeErrorKind {
    InvalidJson(serde_json::Error),
    ExpectedArray,
    ExpectedObject,
    ExpectedString { index: usize },
    Null,
    UnsupportedNumber,
}

impl WasmValueDecodeError {
    fn new(wit_variant: WitValueVariant, kind: WasmValueDecodeErrorKind) -> Self {
        Self {
            wit_variant,
            attribute_path: Vec::new(),
            kind,
        }
    }

    pub fn prepend_attribute(mut self, name: &str) -> Self {
        self.attribute_path.insert(0, name.to_string());
        self
    }
}

impl std::fmt::Display for WasmValueDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to decode WIT {}", self.wit_variant)?;
        if !self.attribute_path.is_empty() {
            write!(f, " attribute '{}'", self.attribute_path.join("."))?;
        }
        match &self.kind {
            WasmValueDecodeErrorKind::InvalidJson(_) => f.write_str(": invalid JSON"),
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
        }
    }
}

impl std::error::Error for WasmValueDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            WasmValueDecodeErrorKind::InvalidJson(source) => Some(source),
            WasmValueDecodeErrorKind::ExpectedArray
            | WasmValueDecodeErrorKind::ExpectedObject
            | WasmValueDecodeErrorKind::ExpectedString { .. }
            | WasmValueDecodeErrorKind::Null
            | WasmValueDecodeErrorKind::UnsupportedNumber => None,
        }
    }
}

fn json_to_proto_value_for_variant(
    v: serde_json::Value,
    wit_variant: WitValueVariant,
) -> Result<proto::Value, WasmValueDecodeError> {
    match v {
        serde_json::Value::Bool(b) => Ok(proto::Value::Bool(b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(proto::Value::Int(i))
            } else if let Some(f) = n.as_f64() {
                Ok(proto::Value::Float(f))
            } else {
                Err(WasmValueDecodeError::new(
                    wit_variant,
                    WasmValueDecodeErrorKind::UnsupportedNumber,
                ))
            }
        }
        serde_json::Value::String(s) => Ok(proto::Value::String(s)),
        serde_json::Value::Array(items) => {
            let items = items
                .into_iter()
                .map(|item| json_to_proto_value_for_variant(item, wit_variant))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(proto::Value::List(items))
        }
        serde_json::Value::Object(entries) => {
            let entries = entries
                .into_iter()
                .map(|(key, value)| {
                    json_to_proto_value_for_variant(value, wit_variant)
                        .map(|value| (key.clone(), value))
                        .map_err(|error| error.prepend_attribute(&key))
                })
                .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
            Ok(proto::Value::Map(entries))
        }
        serde_json::Value::Null => Err(WasmValueDecodeError::new(
            wit_variant,
            WasmValueDecodeErrorKind::Null,
        )),
    }
}

pub fn decode_string_list_val(json: &str) -> Result<proto::Value, WasmValueDecodeError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|source| {
        WasmValueDecodeError::new(
            WitValueVariant::StringListVal,
            WasmValueDecodeErrorKind::InvalidJson(source),
        )
    })?;
    let serde_json::Value::Array(items) = value else {
        return Err(WasmValueDecodeError::new(
            WitValueVariant::StringListVal,
            WasmValueDecodeErrorKind::ExpectedArray,
        ));
    };
    let items = items
        .into_iter()
        .enumerate()
        .map(|(index, item)| match item {
            serde_json::Value::String(item) => Ok(item),
            _ => Err(WasmValueDecodeError::new(
                WitValueVariant::StringListVal,
                WasmValueDecodeErrorKind::ExpectedString { index },
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(proto::Value::StringList(items))
}

pub fn decode_list_val(json: &str) -> Result<proto::Value, WasmValueDecodeError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|source| {
        WasmValueDecodeError::new(
            WitValueVariant::ListVal,
            WasmValueDecodeErrorKind::InvalidJson(source),
        )
    })?;
    if !value.is_array() {
        return Err(WasmValueDecodeError::new(
            WitValueVariant::ListVal,
            WasmValueDecodeErrorKind::ExpectedArray,
        ));
    }
    json_to_proto_value_for_variant(value, WitValueVariant::ListVal)
}

pub fn decode_map_val(json: &str) -> Result<proto::Value, WasmValueDecodeError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|source| {
        WasmValueDecodeError::new(
            WitValueVariant::MapVal,
            WasmValueDecodeErrorKind::InvalidJson(source),
        )
    })?;
    if !value.is_object() {
        return Err(WasmValueDecodeError::new(
            WitValueVariant::MapVal,
            WasmValueDecodeErrorKind::ExpectedObject,
        ));
    }
    json_to_proto_value_for_variant(value, WitValueVariant::MapVal)
}

pub fn decode_secret_val(json: &str) -> Result<proto::Value, WasmValueDecodeError> {
    let inner = serde_json::from_str(json).map_err(|source| {
        WasmValueDecodeError::new(
            WitValueVariant::SecretVal,
            WasmValueDecodeErrorKind::InvalidJson(source),
        )
    })?;
    json_to_proto_value_for_variant(inner, WitValueVariant::SecretVal)
}

/// Render a decode error for boundaries that cannot transport a typed source chain.
pub fn format_wasm_value_decode_error(error: &WasmValueDecodeError) -> String {
    use std::fmt::Write as _;

    let mut rendered = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        write!(rendered, ": {error}").expect("writing to String is infallible");
        source = error.source();
    }
    rendered
}

/// Failure to encode a proto value into a JSON-backed WIT value.
#[derive(Debug)]
pub struct WasmValueEncodeError {
    value: f64,
    attribute_path: Vec<String>,
}

impl WasmValueEncodeError {
    fn non_finite_float(value: f64) -> Self {
        Self {
            value,
            attribute_path: Vec::new(),
        }
    }

    pub fn prepend_attribute(mut self, name: &str) -> Self {
        self.attribute_path.insert(0, name.to_string());
        self
    }
}

impl std::fmt::Display for WasmValueEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("failed to encode a JSON-backed WIT value")?;
        if !self.attribute_path.is_empty() {
            write!(f, " at attribute '{}'", self.attribute_path.join("."))?;
        }
        write!(f, ": non-finite float {} is not valid JSON", self.value)
    }
}

impl std::error::Error for WasmValueEncodeError {}

pub fn proto_value_to_json(v: &proto::Value) -> Result<serde_json::Value, WasmValueEncodeError> {
    match v {
        proto::Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        proto::Value::Int(i) => Ok(serde_json::Value::Number((*i).into())),
        proto::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| WasmValueEncodeError::non_finite_float(*f)),
        proto::Value::String(s) => Ok(serde_json::Value::String(s.clone())),
        proto::Value::StringList(items) => Ok(serde_json::Value::Array(
            items
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        )),
        proto::Value::List(items) => Ok(serde_json::Value::Array(
            items
                .iter()
                .map(proto_value_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        proto::Value::Map(map) => {
            let obj = map
                .iter()
                .map(|(key, value)| {
                    proto_value_to_json(value)
                        .map(|value| (key.clone(), value))
                        .map_err(|error| error.prepend_attribute(key))
                })
                .collect::<Result<serde_json::Map<String, serde_json::Value>, _>>()?;
            Ok(serde_json::Value::Object(obj))
        }
    }
}

/// Parse a ResourceId string (provider.resource_type.identity) into a proto::ResourceId.
///
/// Delegates to `crate::parse_resource_id_string` which is available on all targets.
pub fn parse_resource_id_string(key: &str) -> crate::types::ResourceId {
    crate::parse_resource_id_string(key)
}

/// Macro to export a `CarinaProvider` implementation as a WASM component.
///
/// This macro generates wit-bindgen bindings in the consumer crate and implements
/// the Guest trait by bridging to the CarinaProvider trait.
///
/// Usage:
/// ```ignore
/// // Non-HTTP provider (e.g., MockProvider)
/// #[cfg(target_arch = "wasm32")]
/// carina_plugin_sdk::export_provider!(MyProvider);
///
/// // HTTP-capable provider (e.g., AWS provider)
/// #[cfg(target_arch = "wasm32")]
/// carina_plugin_sdk::export_provider!(MyProvider, http);
/// ```
#[macro_export]
macro_rules! export_provider {
    ($provider_type:ty) => {
        $crate::export_provider!(@internal $provider_type, "carina-provider");
    };
    ($provider_type:ty, http) => {
        mod __carina_wasm_guest {
            wit_bindgen::generate!({
                path: "../carina-plugin-wit/wit",
                world: "carina-provider-with-http",
                with: {
                    "wasi:io/poll@0.2.6": ::wasi::io::poll,
                    "wasi:io/error@0.2.6": ::wasi::io::error,
                    "wasi:io/streams@0.2.6": ::wasi::io::streams,
                    "wasi:clocks/monotonic-clock@0.2.6": ::wasi::clocks::monotonic_clock,
                    "wasi:http/types@0.2.6": ::wasi::http::types,
                    "wasi:http/outgoing-handler@0.2.6": ::wasi::http::outgoing_handler,
                },
            });

            use super::*;
            use $crate::types as proto;
            use $crate::wasm_guest as helpers;
            use std::collections::HashMap;

            use carina::provider::types as wit_types;

            fn get_provider() -> &'static ::std::sync::Mutex<$provider_type> {
                static PROVIDER: ::std::sync::OnceLock<::std::sync::Mutex<$provider_type>> =
                    ::std::sync::OnceLock::new();
                PROVIDER.get_or_init(|| ::std::sync::Mutex::new(<$provider_type>::default()))
            }

            fn wit_to_proto_resource_id(id: &wit_types::ResourceId) -> proto::ResourceId {
                proto::ResourceId {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                    identity: id.identity.clone(),
                }
            }

            fn wit_to_proto_type_identity(
                ty: &wit_types::TypeIdentity,
            ) -> proto::TypeIdentity {
                proto::TypeIdentity {
                    provider: ty.provider.clone(),
                    segments: ty.segments.clone(),
                    kind: ty.kind.clone(),
                }
            }

            fn proto_to_wit_resource_id(id: &proto::ResourceId) -> wit_types::ResourceId {
                wit_types::ResourceId {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                    identity: id.identity.clone(),
                }
            }

            fn wit_to_proto_value(
                v: &wit_types::Value,
            ) -> Result<proto::Value, helpers::WasmValueDecodeError> {
                match v {
                    wit_types::Value::BoolVal(b) => Ok(proto::Value::Bool(*b)),
                    wit_types::Value::IntVal(i) => Ok(proto::Value::Int(*i)),
                    wit_types::Value::FloatVal(f) => Ok(proto::Value::Float(*f)),
                    wit_types::Value::StrVal(s) => Ok(proto::Value::String(s.clone())),
                    wit_types::Value::StringListVal(json) => helpers::decode_string_list_val(json),
                    wit_types::Value::ListVal(json) => helpers::decode_list_val(json),
                    wit_types::Value::MapVal(json) => helpers::decode_map_val(json),
                    // Desired-state round trips (`normalize_desired` and
                    // `merge_default_tags`) are host-sealed and never carry
                    // `SecretVal` (carina#3800). CRUD inputs decode the inner
                    // JSON into an ordinary provider value because
                    // `proto::Value` has no `Secret` arm. Malformed or null
                    // payloads remain boundary errors. Providers MUST NOT log
                    // or persist the plaintext.
                    wit_types::Value::SecretVal(json) => helpers::decode_secret_val(json),
                }
            }

            fn proto_to_wit_value(
                v: &proto::Value,
            ) -> Result<wit_types::Value, helpers::WasmValueEncodeError> {
                Ok(match v {
                    proto::Value::Bool(b) => wit_types::Value::BoolVal(*b),
                    proto::Value::Int(i) => wit_types::Value::IntVal(*i),
                    proto::Value::Float(f) => wit_types::Value::FloatVal(*f),
                    proto::Value::String(s) => wit_types::Value::StrVal(s.clone()),
                    proto::Value::StringList(items) => {
                        wit_types::Value::StringListVal(serde_json::to_string(items).unwrap())
                    }
                    proto::Value::List(_) => {
                        let json = helpers::proto_value_to_json(v)?;
                        wit_types::Value::ListVal(serde_json::to_string(&json).unwrap())
                    }
                    proto::Value::Map(_) => {
                        let json = helpers::proto_value_to_json(v)?;
                        wit_types::Value::MapVal(serde_json::to_string(&json).unwrap())
                    }
                })
            }

            fn wit_to_proto_value_map(
                entries: &[(String, wit_types::Value)],
            ) -> Result<HashMap<String, proto::Value>, helpers::WasmValueDecodeError> {
                entries
                    .iter()
                    .map(|(key, value): &(String, wit_types::Value)| {
                        wit_to_proto_value(value)
                            .map(|value| (key.clone(), value))
                            .map_err(|error| error.prepend_attribute(key))
                    })
                    .collect()
            }

            fn proto_to_wit_value_map(
                map: &HashMap<String, proto::Value>,
            ) -> Result<
                Vec<(String, wit_types::Value)>,
                helpers::WasmValueEncodeError,
            > {
                map.iter()
                    .map(|(key, value)| {
                        proto_to_wit_value(value)
                            .map(|value| (key.clone(), value))
                            .map_err(|error| error.prepend_attribute(key))
                    })
                    .collect()
            }

            fn wit_to_proto_state(
                id: &proto::ResourceId,
                state: &wit_types::State,
            ) -> Result<proto::State, helpers::WasmValueDecodeError> {
                Ok(proto::State {
                    id: id.clone(),
                    identifier: state.identifier.clone(),
                    attributes: wit_to_proto_value_map(&state.attributes)?,
                    exists: state.exists,
                })
            }

            fn proto_to_wit_state(
                state: &proto::State,
            ) -> Result<wit_types::State, helpers::WasmValueEncodeError> {
                Ok(wit_types::State {
                    identifier: state.identifier.clone(),
                    attributes: proto_to_wit_value_map(&state.attributes)?,
                    exists: state.exists,
                })
            }

            fn proto_to_wit_create_outcome(
                outcome: &proto::CreateOutcome,
            ) -> Result<wit_types::CreateOutcome, helpers::WasmValueEncodeError> {
                Ok(match outcome {
                    proto::CreateOutcome::Success { state } => {
                        wit_types::CreateOutcome::Success(proto_to_wit_state(state)?)
                    }
                    proto::CreateOutcome::PartialSuccess { state, diagnostic } => {
                        wit_types::CreateOutcome::PartialSuccess(
                            wit_types::CreatePartialSuccess {
                                state: proto_to_wit_state(state)?,
                                diagnostic: wit_types::PartialReadDiagnostic {
                                    reason: diagnostic.reason.clone(),
                                    missing_attributes: diagnostic.missing_attributes.clone(),
                                },
                            },
                        )
                    }
                })
            }

            fn proto_to_wit_update_outcome(
                outcome: &proto::UpdateOutcome,
            ) -> Result<wit_types::UpdateOutcome, helpers::WasmValueEncodeError> {
                Ok(match outcome {
                    proto::UpdateOutcome::Success { state } => {
                        wit_types::UpdateOutcome::Success(proto_to_wit_state(state)?)
                    }
                    proto::UpdateOutcome::PartialSuccess { state, diagnostic } => {
                        wit_types::UpdateOutcome::PartialSuccess(
                            wit_types::UpdatePartialSuccess {
                                state: proto_to_wit_state(state)?,
                                diagnostic: wit_types::PartialReadDiagnostic {
                                    reason: diagnostic.reason.clone(),
                                    missing_attributes: diagnostic.missing_attributes.clone(),
                                },
                            },
                        )
                    }
                })
            }

            fn wit_to_proto_resource(
                res: &wit_types::ResourceDef,
            ) -> Result<proto::Resource, helpers::WasmValueDecodeError> {
                Ok(proto::Resource {
                    id: wit_to_proto_resource_id(&res.id),
                    attributes: wit_to_proto_value_map(&res.attributes)?,
                    directives: proto::Directives::default(),
                })
            }

            fn proto_to_wit_resource(
                res: &proto::Resource,
            ) -> Result<wit_types::ResourceDef, helpers::WasmValueEncodeError> {
                Ok(wit_types::ResourceDef {
                    id: proto_to_wit_resource_id(&res.id),
                    attributes: proto_to_wit_value_map(&res.attributes)?,
                })
            }

            // -- Guest trait implementation --

            fn proto_to_wit_provider_error(err: proto::ProviderError) -> wit_types::ProviderError {
                let detail = wit_types::ErrorDetail {
                    message: err.message,
                    resource_id: err.resource_id.as_ref().map(proto_to_wit_resource_id),
                    cause: err.cause,
                    provider_name: err.provider_name,
                    operation: err.operation,
                    status: err.status,
                    code: err.code,
                    request_id: err.request_id,
                };
                match err.kind {
                    proto::ProviderErrorKind::InvalidInput => {
                        wit_types::ProviderError::InvalidInput(detail)
                    }
                    proto::ProviderErrorKind::ApiError => {
                        wit_types::ProviderError::ApiError(detail)
                    }
                    proto::ProviderErrorKind::NotFound => {
                        wit_types::ProviderError::NotFound(detail)
                    }
                    proto::ProviderErrorKind::Timeout => {
                        wit_types::ProviderError::Timeout(detail)
                    }
                    proto::ProviderErrorKind::Internal => {
                        wit_types::ProviderError::Internal(detail)
                    }
                }
            }

            fn validate_string_to_provider_error(
                msg: String,
            ) -> wit_types::ProviderError {
                wit_types::ProviderError::InvalidInput(wit_types::ErrorDetail {
                    message: msg,
                    resource_id: None,
                    cause: None,
                    provider_name: None,
                    operation: None,
                    status: None,
                    code: None,
                    request_id: None,
                })
            }

            fn boundary_decode_to_provider_error(
                operation: &'static str,
                part: Option<&'static str>,
                error: helpers::WasmValueDecodeError,
            ) -> wit_types::ProviderError {
                let error = helpers::format_wasm_value_decode_error(&error);
                let location = part.map_or_else(
                    || operation.to_string(),
                    |part| format!("{operation} {part}"),
                );
                wit_types::ProviderError::Internal(wit_types::ErrorDetail {
                    message: format!("WASM boundary decode error in {location}: {error}"),
                    resource_id: None,
                    cause: None,
                    provider_name: None,
                    operation: Some(operation.to_string()),
                    status: None,
                    code: None,
                    request_id: None,
                })
            }

            fn boundary_encode_to_provider_error(
                operation: &'static str,
                part: Option<&'static str>,
                error: helpers::WasmValueEncodeError,
            ) -> wit_types::ProviderError {
                let location = part.map_or_else(
                    || operation.to_string(),
                    |part| format!("{operation} {part}"),
                );
                wit_types::ProviderError::Internal(wit_types::ErrorDetail {
                    message: format!("WASM boundary encode error in {location}: {error}"),
                    resource_id: None,
                    cause: None,
                    provider_name: None,
                    operation: Some(operation.to_string()),
                    status: None,
                    code: None,
                    request_id: None,
                })
            }

            fn provider_export_or_trap<T>(
                operation: &'static str,
                result: Result<T, proto::ProviderError>,
            ) -> T {
                result.unwrap_or_else(|error| {
                    panic!("Provider export error in {operation}: {}", error.message)
                })
            }

            fn wit_to_proto_patch_op_kind(k: wit_types::PatchOpKind) -> proto::PatchOpKind {
                match k {
                    wit_types::PatchOpKind::Add => proto::PatchOpKind::Add,
                    wit_types::PatchOpKind::Replace => proto::PatchOpKind::Replace,
                    wit_types::PatchOpKind::Remove => proto::PatchOpKind::Remove,
                }
            }

            fn wit_to_proto_update_request(
                req: wit_types::UpdateRequest,
                proto_id: &proto::ResourceId,
            ) -> Result<proto::UpdateRequest, helpers::WasmValueDecodeError> {
                let from = wit_to_proto_state(proto_id, &req.current)?;
                let ops = req
                    .patch
                    .ops
                    .into_iter()
                    .map(|op| {
                        Ok(proto::PatchOp {
                            kind: wit_to_proto_patch_op_kind(op.kind),
                            key: op.key,
                            value: op.value.as_ref().map(wit_to_proto_value).transpose()?,
                        })
                    })
                    .collect::<Result<Vec<_>, helpers::WasmValueDecodeError>>()?;
                Ok(proto::UpdateRequest {
                    from,
                    patch: proto::UpdatePatch { ops },
                })
            }

            fn wit_to_proto_create_request(
                req: wit_types::CreateRequest,
            ) -> Result<proto::CreateRequest, helpers::WasmValueDecodeError> {
                Ok(proto::CreateRequest {
                    resource: wit_to_proto_resource(&req.res)?,
                })
            }

            fn wit_to_proto_delete_request(
                req: wit_types::DeleteRequest,
            ) -> proto::DeleteRequest {
                proto::DeleteRequest {
                    directives: proto::Directives {
                        force_delete: req.directives.force_delete,
                        create_before_destroy: req.directives.create_before_destroy,
                        prevent_destroy: req.directives.prevent_destroy,
                    },
                }
            }

            fn wit_to_sdk_plan_op(
                op: exports::carina::provider::provider::PlanOp,
            ) -> $crate::PlanOp {
                match op {
                    exports::carina::provider::provider::PlanOp::Create => $crate::PlanOp::Create,
                    exports::carina::provider::provider::PlanOp::Read => $crate::PlanOp::Read,
                    exports::carina::provider::provider::PlanOp::Update => $crate::PlanOp::Update,
                    exports::carina::provider::provider::PlanOp::Delete => $crate::PlanOp::Delete,
                }
            }

            fn sdk_to_wit_binding_pattern(
                pattern: &$crate::BindingPattern,
            ) -> wit_types::BindingPattern {
                match pattern {
                    $crate::BindingPattern::Exact(name) => {
                        wit_types::BindingPattern::Exact(name.clone())
                    }
                    $crate::BindingPattern::ForLoopChildren { base } => {
                        wit_types::BindingPattern::ForLoopChildren(base.clone())
                    }
                    $crate::BindingPattern::AttributeMatch {
                        resource_type,
                        attr,
                        from,
                    } => wit_types::BindingPattern::AttributeMatch(
                        wit_types::AttributeMatchPattern {
                            resource_type: resource_type.clone(),
                            attr: attr.clone(),
                            from: from.clone(),
                        },
                    ),
                }
            }

            struct WasmGuest;

            impl exports::carina::provider::provider::Guest for WasmGuest {
                fn info() -> String {
                    let provider = get_provider().lock().unwrap();
                    let info = $crate::CarinaProvider::info(&*provider);
                    let envelope = $crate::protocol::types::ProviderInfoEnvelope {
                        info,
                        protocol_version: $crate::protocol::PROTOCOL_VERSION,
                    };
                    serde_json::to_string(&envelope)
                        .expect("provider info serialization is infallible")
                }

                fn schemas() -> String {
                    let provider = get_provider().lock().unwrap();
                    let schemas = $crate::CarinaProvider::schemas(&*provider);
                    serde_json::to_string(&schemas)
                        .expect("provider schemas serialization is infallible")
                }

                fn provider_config_attribute_types() -> String {
                    let provider = get_provider().lock().unwrap();
                    let types = $crate::CarinaProvider::provider_config_attribute_types(
                        &*provider,
                    );
                    serde_json::to_string(&types)
                        .expect("provider config attribute type serialization is infallible")
                }

                fn validate_config(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), wit_types::ProviderError> {
                    let map = wit_to_proto_value_map(&attrs).map_err(|error| {
                        boundary_decode_to_provider_error("validate_config", None, error)
                    })?;
                    let provider = get_provider().lock().unwrap();
                    $crate::CarinaProvider::validate_config(&*provider, &map)
                        .map_err(validate_string_to_provider_error)
                }

                fn initialize(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), wit_types::ProviderError> {
                    let map = wit_to_proto_value_map(&attrs).map_err(|error| {
                        boundary_decode_to_provider_error("initialize", None, error)
                    })?;
                    let mut provider = get_provider().lock().unwrap();
                    $crate::CarinaProvider::initialize(&mut *provider, &map)
                        .map_err(validate_string_to_provider_error)
                }

                fn read(
                    id: wit_types::ResourceId,
                    identifier: Option<String>,
                    _request: wit_types::ReadRequest,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    match $crate::CarinaProvider::read(
                        &*provider,
                        &proto_id,
                        identifier.as_deref(),
                        proto::ReadRequest,
                    ) {
                        Ok(state) => proto_to_wit_state(&state)
                            .map_err(|error| {
                                boundary_encode_to_provider_error("read", None, error)
                            }),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn read_data_source(
                    res: wit_types::ResourceDef,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let proto_res = wit_to_proto_resource(&res).map_err(|error| {
                        boundary_decode_to_provider_error("read_data_source", None, error)
                    })?;
                    let provider = get_provider().lock().unwrap();
                    match $crate::CarinaProvider::read_data_source(&*provider, &proto_res) {
                        Ok(state) => proto_to_wit_state(&state).map_err(|error| {
                            boundary_encode_to_provider_error("read_data_source", None, error)
                        }),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn create(
                    id: wit_types::ResourceId,
                    request: wit_types::CreateRequest,
                ) -> Result<wit_types::CreateOutcome, wit_types::ProviderError> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_create_request(request)
                        .map_err(|error| {
                            boundary_decode_to_provider_error("create", None, error)
                        })?;
                    let provider = get_provider().lock().unwrap();
                    match $crate::CarinaProvider::create(&*provider, &proto_id, proto_request) {
                        Ok(outcome) => proto_to_wit_create_outcome(&outcome)
                            .map_err(|error| {
                                boundary_encode_to_provider_error("create", None, error)
                            }),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn update(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::UpdateRequest,
                ) -> Result<wit_types::UpdateOutcome, wit_types::ProviderError> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_update_request(request, &proto_id)
                        .map_err(|error| {
                            boundary_decode_to_provider_error("update", None, error)
                        })?;
                    let provider = get_provider().lock().unwrap();
                    match $crate::CarinaProvider::update(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto_request,
                    ) {
                        Ok(outcome) => proto_to_wit_update_outcome(&outcome)
                            .map_err(|error| {
                                boundary_encode_to_provider_error("update", None, error)
                            }),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn delete(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::DeleteRequest,
                ) -> Result<(), wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_delete_request(request);
                    match $crate::CarinaProvider::delete(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto_request,
                    ) {
                        Ok(()) => Ok(()),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn required_permissions(
                    id: wit_types::ResourceId,
                    operation: exports::carina::provider::provider::PlanOp,
                ) -> Vec<String> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::required_permissions(
                            &*provider,
                            &proto_id,
                            wit_to_sdk_plan_op(operation),
                        )
                    };
                    provider_export_or_trap("required_permissions", result)
                }

                fn satisfier_hint(
                    target_id: wit_types::ResourceId,
                    attr_path: Vec<String>,
                ) -> Vec<wit_types::BindingPattern> {
                    let proto_id = wit_to_proto_resource_id(&target_id);
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::satisfier_hint(&*provider, &proto_id, &attr_path)
                    };
                    provider_export_or_trap("satisfier_hint", result)
                        .iter()
                        .map(sdk_to_wit_binding_pattern)
                        .collect()
                }

                fn provider_config_completions() -> String {
                    let provider = get_provider().lock().unwrap();
                    let completions = $crate::CarinaProvider::config_completions(&*provider);
                    serde_json::to_string(&completions)
                        .expect("provider config completion serialization is infallible")
                }

                fn identity_attributes() -> Vec<String> {
                    let provider = get_provider().lock().unwrap();
                    $crate::CarinaProvider::identity_attributes(&*provider)
                }

                fn validate_custom_type(
                    ty: wit_types::TypeIdentity,
                    value: String,
                ) -> Result<(), wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    $crate::CarinaProvider::validate_custom_type(
                        &*provider,
                        &wit_to_proto_type_identity(&ty),
                        &value,
                    )
                    .map_err(validate_string_to_provider_error)
                }

                fn get_enum_aliases() -> String {
                    let provider = get_provider().lock().unwrap();
                    let aliases = $crate::CarinaProvider::enum_aliases(&*provider);
                    serde_json::to_string(&aliases)
                        .expect("provider enum alias serialization is infallible")
                }

                fn normalize_desired(
                    resources: Vec<wit_types::ResourceDef>,
                ) -> Result<Vec<wit_types::ResourceDef>, wit_types::ProviderError> {
                    let proto_resources = resources
                        .iter()
                        .map(wit_to_proto_resource)
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error("normalize_desired", None, error)
                        })?;
                    // Keep the mutex guard's lexical scope limited to the
                    // provider call; bridge result and encoding work does not
                    // require access to the shared provider.
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::normalize_desired(&*provider, proto_resources)
                    }
                    .map_err(proto_to_wit_provider_error)?;
                    result
                        .iter()
                        .map(proto_to_wit_resource)
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error("normalize_desired", None, error)
                        })
                }

                fn normalize_state(
                    states: Vec<(String, wit_types::State)>,
                ) -> Result<Vec<(String, wit_types::State)>, wit_types::ProviderError> {
                    let proto_states = states
                        .iter()
                        .map(|(key, state)| {
                            let parsed_id = helpers::parse_resource_id_string(key);
                            wit_to_proto_state(&parsed_id, state)
                                .map(|state| (key.clone(), state))
                        })
                        .collect::<Result<HashMap<_, _>, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error("normalize_state", None, error)
                        })?;
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::normalize_state(&*provider, proto_states)
                    }
                    .map_err(proto_to_wit_provider_error)?;
                    result
                        .into_iter()
                        .map(|(key, state)| {
                            proto_to_wit_state(&state).map(|state| (key, state))
                        })
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error("normalize_state", None, error)
                        })
                }

                fn hydrate_read_state(
                    states: Vec<(String, wit_types::State)>,
                    saved_attrs: Vec<(String, Vec<(String, wit_types::Value)>)>,
                ) -> Result<Vec<(String, wit_types::State)>, wit_types::ProviderError> {
                    let mut proto_states = states
                        .iter()
                        .map(|(key, state)| {
                            let parsed_id = helpers::parse_resource_id_string(key);
                            wit_to_proto_state(&parsed_id, state)
                                .map(|state| (key.clone(), state))
                        })
                        .collect::<Result<HashMap<_, _>, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error(
                                "hydrate_read_state",
                                Some("states"),
                                error,
                            )
                        })?;
                    let proto_saved = saved_attrs
                        .iter()
                        .map(|(key, attributes)| {
                            wit_to_proto_value_map(attributes)
                                .map(|attributes| (key.clone(), attributes))
                        })
                        .collect::<Result<HashMap<_, _>, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error(
                                "hydrate_read_state",
                                Some("saved attributes"),
                                error,
                            )
                        })?;
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::hydrate_read_state(
                            &*provider,
                            &mut proto_states,
                            &proto_saved,
                        )
                    }
                    .map_err(proto_to_wit_provider_error)?;
                    proto_states
                        .into_iter()
                        .map(|(key, state)| {
                            proto_to_wit_state(&state).map(|state| (key, state))
                        })
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error(
                                "hydrate_read_state",
                                None,
                                error,
                            )
                        })
                }

                fn merge_default_tags(
                    resources: Vec<wit_types::ResourceDef>,
                    default_tags: Vec<(String, wit_types::Value)>,
                ) -> Result<Vec<wit_types::ResourceDef>, wit_types::ProviderError> {
                    let mut proto_resources = resources
                        .iter()
                        .map(wit_to_proto_resource)
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error(
                                "merge_default_tags",
                                Some("resources"),
                                error,
                            )
                        })?;
                    let proto_tags = wit_to_proto_value_map(&default_tags).map_err(|error| {
                        boundary_decode_to_provider_error(
                            "merge_default_tags",
                            Some("default tags"),
                            error,
                        )
                    })?;
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        let schemas = $crate::CarinaProvider::schemas(&*provider);
                        $crate::CarinaProvider::merge_default_tags(
                            &*provider,
                            &mut proto_resources,
                            &proto_tags,
                            &schemas,
                        )
                    }
                    .map_err(proto_to_wit_provider_error)?;
                    proto_resources
                        .iter()
                        .map(proto_to_wit_resource)
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error("merge_default_tags", None, error)
                        })
                }
            }

            export!(WasmGuest);
        }
    };
    (@internal $provider_type:ty, $world:literal) => {
        mod __carina_wasm_guest {
            wit_bindgen::generate!({
                path: "../carina-plugin-wit/wit",
                world: $world,
            });

            use super::*;
            use $crate::types as proto;
            use $crate::wasm_guest as helpers;
            use std::collections::HashMap;

            // Type aliases for the generated types
            use carina::provider::types as wit_types;

            fn get_provider() -> &'static ::std::sync::Mutex<$provider_type> {
                static PROVIDER: ::std::sync::OnceLock<::std::sync::Mutex<$provider_type>> =
                    ::std::sync::OnceLock::new();
                PROVIDER.get_or_init(|| ::std::sync::Mutex::new(<$provider_type>::default()))
            }

            // -- WIT <-> proto conversion functions --
            // These are local to this module because they reference the locally-generated
            // wit-bindgen types.

            fn wit_to_proto_resource_id(id: &wit_types::ResourceId) -> proto::ResourceId {
                proto::ResourceId {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                    identity: id.identity.clone(),
                }
            }

            fn wit_to_proto_type_identity(
                ty: &wit_types::TypeIdentity,
            ) -> proto::TypeIdentity {
                proto::TypeIdentity {
                    provider: ty.provider.clone(),
                    segments: ty.segments.clone(),
                    kind: ty.kind.clone(),
                }
            }

            fn proto_to_wit_resource_id(id: &proto::ResourceId) -> wit_types::ResourceId {
                wit_types::ResourceId {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                    identity: id.identity.clone(),
                }
            }

            fn wit_to_proto_value(
                v: &wit_types::Value,
            ) -> Result<proto::Value, helpers::WasmValueDecodeError> {
                match v {
                    wit_types::Value::BoolVal(b) => Ok(proto::Value::Bool(*b)),
                    wit_types::Value::IntVal(i) => Ok(proto::Value::Int(*i)),
                    wit_types::Value::FloatVal(f) => Ok(proto::Value::Float(*f)),
                    wit_types::Value::StrVal(s) => Ok(proto::Value::String(s.clone())),
                    wit_types::Value::StringListVal(json) => helpers::decode_string_list_val(json),
                    wit_types::Value::ListVal(json) => helpers::decode_list_val(json),
                    wit_types::Value::MapVal(json) => helpers::decode_map_val(json),
                    // Desired-state round trips (`normalize_desired` and
                    // `merge_default_tags`) are host-sealed and never carry
                    // `SecretVal` (carina#3800). CRUD inputs decode the inner
                    // JSON into an ordinary provider value because
                    // `proto::Value` has no `Secret` arm. Malformed or null
                    // payloads remain boundary errors. Providers MUST NOT log
                    // or persist the plaintext.
                    wit_types::Value::SecretVal(json) => helpers::decode_secret_val(json),
                }
            }

            fn proto_to_wit_value(
                v: &proto::Value,
            ) -> Result<wit_types::Value, helpers::WasmValueEncodeError> {
                Ok(match v {
                    proto::Value::Bool(b) => wit_types::Value::BoolVal(*b),
                    proto::Value::Int(i) => wit_types::Value::IntVal(*i),
                    proto::Value::Float(f) => wit_types::Value::FloatVal(*f),
                    proto::Value::String(s) => wit_types::Value::StrVal(s.clone()),
                    proto::Value::StringList(items) => {
                        wit_types::Value::StringListVal(serde_json::to_string(items).unwrap())
                    }
                    proto::Value::List(_) => {
                        let json = helpers::proto_value_to_json(v)?;
                        wit_types::Value::ListVal(serde_json::to_string(&json).unwrap())
                    }
                    proto::Value::Map(_) => {
                        let json = helpers::proto_value_to_json(v)?;
                        wit_types::Value::MapVal(serde_json::to_string(&json).unwrap())
                    }
                })
            }

            fn wit_to_proto_value_map(
                entries: &[(String, wit_types::Value)],
            ) -> Result<HashMap<String, proto::Value>, helpers::WasmValueDecodeError> {
                entries
                    .iter()
                    .map(|(key, value): &(String, wit_types::Value)| {
                        wit_to_proto_value(value)
                            .map(|value| (key.clone(), value))
                            .map_err(|error| error.prepend_attribute(key))
                    })
                    .collect()
            }

            fn proto_to_wit_value_map(
                map: &HashMap<String, proto::Value>,
            ) -> Result<
                Vec<(String, wit_types::Value)>,
                helpers::WasmValueEncodeError,
            > {
                map.iter()
                    .map(|(key, value)| {
                        proto_to_wit_value(value)
                            .map(|value| (key.clone(), value))
                            .map_err(|error| error.prepend_attribute(key))
                    })
                    .collect()
            }

            fn wit_to_proto_state(
                id: &proto::ResourceId,
                state: &wit_types::State,
            ) -> Result<proto::State, helpers::WasmValueDecodeError> {
                Ok(proto::State {
                    id: id.clone(),
                    identifier: state.identifier.clone(),
                    attributes: wit_to_proto_value_map(&state.attributes)?,
                    exists: state.exists,
                })
            }

            fn proto_to_wit_state(
                state: &proto::State,
            ) -> Result<wit_types::State, helpers::WasmValueEncodeError> {
                Ok(wit_types::State {
                    identifier: state.identifier.clone(),
                    attributes: proto_to_wit_value_map(&state.attributes)?,
                    exists: state.exists,
                })
            }

            fn proto_to_wit_create_outcome(
                outcome: &proto::CreateOutcome,
            ) -> Result<wit_types::CreateOutcome, helpers::WasmValueEncodeError> {
                Ok(match outcome {
                    proto::CreateOutcome::Success { state } => {
                        wit_types::CreateOutcome::Success(proto_to_wit_state(state)?)
                    }
                    proto::CreateOutcome::PartialSuccess { state, diagnostic } => {
                        wit_types::CreateOutcome::PartialSuccess(
                            wit_types::CreatePartialSuccess {
                                state: proto_to_wit_state(state)?,
                                diagnostic: wit_types::PartialReadDiagnostic {
                                    reason: diagnostic.reason.clone(),
                                    missing_attributes: diagnostic.missing_attributes.clone(),
                                },
                            },
                        )
                    }
                })
            }

            fn proto_to_wit_update_outcome(
                outcome: &proto::UpdateOutcome,
            ) -> Result<wit_types::UpdateOutcome, helpers::WasmValueEncodeError> {
                Ok(match outcome {
                    proto::UpdateOutcome::Success { state } => {
                        wit_types::UpdateOutcome::Success(proto_to_wit_state(state)?)
                    }
                    proto::UpdateOutcome::PartialSuccess { state, diagnostic } => {
                        wit_types::UpdateOutcome::PartialSuccess(
                            wit_types::UpdatePartialSuccess {
                                state: proto_to_wit_state(state)?,
                                diagnostic: wit_types::PartialReadDiagnostic {
                                    reason: diagnostic.reason.clone(),
                                    missing_attributes: diagnostic.missing_attributes.clone(),
                                },
                            },
                        )
                    }
                })
            }

            fn wit_to_proto_resource(
                res: &wit_types::ResourceDef,
            ) -> Result<proto::Resource, helpers::WasmValueDecodeError> {
                Ok(proto::Resource {
                    id: wit_to_proto_resource_id(&res.id),
                    attributes: wit_to_proto_value_map(&res.attributes)?,
                    directives: proto::Directives::default(),
                })
            }

            fn proto_to_wit_resource(
                res: &proto::Resource,
            ) -> Result<wit_types::ResourceDef, helpers::WasmValueEncodeError> {
                Ok(wit_types::ResourceDef {
                    id: proto_to_wit_resource_id(&res.id),
                    attributes: proto_to_wit_value_map(&res.attributes)?,
                })
            }

            // -- Guest trait implementation --

            fn proto_to_wit_provider_error(err: proto::ProviderError) -> wit_types::ProviderError {
                let detail = wit_types::ErrorDetail {
                    message: err.message,
                    resource_id: err.resource_id.as_ref().map(proto_to_wit_resource_id),
                    cause: err.cause,
                    provider_name: err.provider_name,
                    operation: err.operation,
                    status: err.status,
                    code: err.code,
                    request_id: err.request_id,
                };
                match err.kind {
                    proto::ProviderErrorKind::InvalidInput => {
                        wit_types::ProviderError::InvalidInput(detail)
                    }
                    proto::ProviderErrorKind::ApiError => {
                        wit_types::ProviderError::ApiError(detail)
                    }
                    proto::ProviderErrorKind::NotFound => {
                        wit_types::ProviderError::NotFound(detail)
                    }
                    proto::ProviderErrorKind::Timeout => {
                        wit_types::ProviderError::Timeout(detail)
                    }
                    proto::ProviderErrorKind::Internal => {
                        wit_types::ProviderError::Internal(detail)
                    }
                }
            }

            fn validate_string_to_provider_error(
                msg: String,
            ) -> wit_types::ProviderError {
                wit_types::ProviderError::InvalidInput(wit_types::ErrorDetail {
                    message: msg,
                    resource_id: None,
                    cause: None,
                    provider_name: None,
                    operation: None,
                    status: None,
                    code: None,
                    request_id: None,
                })
            }

            fn boundary_decode_to_provider_error(
                operation: &'static str,
                part: Option<&'static str>,
                error: helpers::WasmValueDecodeError,
            ) -> wit_types::ProviderError {
                let error = helpers::format_wasm_value_decode_error(&error);
                let location = part.map_or_else(
                    || operation.to_string(),
                    |part| format!("{operation} {part}"),
                );
                wit_types::ProviderError::Internal(wit_types::ErrorDetail {
                    message: format!("WASM boundary decode error in {location}: {error}"),
                    resource_id: None,
                    cause: None,
                    provider_name: None,
                    operation: Some(operation.to_string()),
                    status: None,
                    code: None,
                    request_id: None,
                })
            }

            fn boundary_encode_to_provider_error(
                operation: &'static str,
                part: Option<&'static str>,
                error: helpers::WasmValueEncodeError,
            ) -> wit_types::ProviderError {
                let location = part.map_or_else(
                    || operation.to_string(),
                    |part| format!("{operation} {part}"),
                );
                wit_types::ProviderError::Internal(wit_types::ErrorDetail {
                    message: format!("WASM boundary encode error in {location}: {error}"),
                    resource_id: None,
                    cause: None,
                    provider_name: None,
                    operation: Some(operation.to_string()),
                    status: None,
                    code: None,
                    request_id: None,
                })
            }

            fn provider_export_or_trap<T>(
                operation: &'static str,
                result: Result<T, proto::ProviderError>,
            ) -> T {
                result.unwrap_or_else(|error| {
                    panic!("Provider export error in {operation}: {}", error.message)
                })
            }

            fn wit_to_proto_patch_op_kind(k: wit_types::PatchOpKind) -> proto::PatchOpKind {
                match k {
                    wit_types::PatchOpKind::Add => proto::PatchOpKind::Add,
                    wit_types::PatchOpKind::Replace => proto::PatchOpKind::Replace,
                    wit_types::PatchOpKind::Remove => proto::PatchOpKind::Remove,
                }
            }

            fn wit_to_proto_update_request(
                req: wit_types::UpdateRequest,
                proto_id: &proto::ResourceId,
            ) -> Result<proto::UpdateRequest, helpers::WasmValueDecodeError> {
                let from = wit_to_proto_state(proto_id, &req.current)?;
                let ops = req
                    .patch
                    .ops
                    .into_iter()
                    .map(|op| {
                        Ok(proto::PatchOp {
                            kind: wit_to_proto_patch_op_kind(op.kind),
                            key: op.key,
                            value: op.value.as_ref().map(wit_to_proto_value).transpose()?,
                        })
                    })
                    .collect::<Result<Vec<_>, helpers::WasmValueDecodeError>>()?;
                Ok(proto::UpdateRequest {
                    from,
                    patch: proto::UpdatePatch { ops },
                })
            }

            fn wit_to_proto_create_request(
                req: wit_types::CreateRequest,
            ) -> Result<proto::CreateRequest, helpers::WasmValueDecodeError> {
                Ok(proto::CreateRequest {
                    resource: wit_to_proto_resource(&req.res)?,
                })
            }

            fn wit_to_proto_delete_request(
                req: wit_types::DeleteRequest,
            ) -> proto::DeleteRequest {
                proto::DeleteRequest {
                    directives: proto::Directives {
                        force_delete: req.directives.force_delete,
                        create_before_destroy: req.directives.create_before_destroy,
                        prevent_destroy: req.directives.prevent_destroy,
                    },
                }
            }

            fn wit_to_sdk_plan_op(
                op: exports::carina::provider::provider::PlanOp,
            ) -> $crate::PlanOp {
                match op {
                    exports::carina::provider::provider::PlanOp::Create => $crate::PlanOp::Create,
                    exports::carina::provider::provider::PlanOp::Read => $crate::PlanOp::Read,
                    exports::carina::provider::provider::PlanOp::Update => $crate::PlanOp::Update,
                    exports::carina::provider::provider::PlanOp::Delete => $crate::PlanOp::Delete,
                }
            }

            fn sdk_to_wit_binding_pattern(
                pattern: &$crate::BindingPattern,
            ) -> wit_types::BindingPattern {
                match pattern {
                    $crate::BindingPattern::Exact(name) => {
                        wit_types::BindingPattern::Exact(name.clone())
                    }
                    $crate::BindingPattern::ForLoopChildren { base } => {
                        wit_types::BindingPattern::ForLoopChildren(base.clone())
                    }
                    $crate::BindingPattern::AttributeMatch {
                        resource_type,
                        attr,
                        from,
                    } => wit_types::BindingPattern::AttributeMatch(
                        wit_types::AttributeMatchPattern {
                            resource_type: resource_type.clone(),
                            attr: attr.clone(),
                            from: from.clone(),
                        },
                    ),
                }
            }

            struct WasmGuest;

            impl exports::carina::provider::provider::Guest for WasmGuest {
                fn info() -> String {
                    let provider = get_provider().lock().unwrap();
                    let info = $crate::CarinaProvider::info(&*provider);
                    let envelope = $crate::protocol::types::ProviderInfoEnvelope {
                        info,
                        protocol_version: $crate::protocol::PROTOCOL_VERSION,
                    };
                    serde_json::to_string(&envelope)
                        .expect("provider info serialization is infallible")
                }

                fn schemas() -> String {
                    let provider = get_provider().lock().unwrap();
                    let schemas = $crate::CarinaProvider::schemas(&*provider);
                    serde_json::to_string(&schemas)
                        .expect("provider schemas serialization is infallible")
                }

                fn provider_config_attribute_types() -> String {
                    let provider = get_provider().lock().unwrap();
                    let types = $crate::CarinaProvider::provider_config_attribute_types(
                        &*provider,
                    );
                    serde_json::to_string(&types)
                        .expect("provider config attribute type serialization is infallible")
                }

                fn validate_config(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), wit_types::ProviderError> {
                    let map = wit_to_proto_value_map(&attrs).map_err(|error| {
                        boundary_decode_to_provider_error("validate_config", None, error)
                    })?;
                    let provider = get_provider().lock().unwrap();
                    $crate::CarinaProvider::validate_config(&*provider, &map)
                        .map_err(validate_string_to_provider_error)
                }

                fn initialize(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), wit_types::ProviderError> {
                    let map = wit_to_proto_value_map(&attrs).map_err(|error| {
                        boundary_decode_to_provider_error("initialize", None, error)
                    })?;
                    let mut provider = get_provider().lock().unwrap();
                    $crate::CarinaProvider::initialize(&mut *provider, &map)
                        .map_err(validate_string_to_provider_error)
                }

                fn read(
                    id: wit_types::ResourceId,
                    identifier: Option<String>,
                    _request: wit_types::ReadRequest,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    match $crate::CarinaProvider::read(
                        &*provider,
                        &proto_id,
                        identifier.as_deref(),
                        proto::ReadRequest,
                    ) {
                        Ok(state) => proto_to_wit_state(&state)
                            .map_err(|error| {
                                boundary_encode_to_provider_error("read", None, error)
                            }),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn read_data_source(
                    res: wit_types::ResourceDef,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let proto_res = wit_to_proto_resource(&res).map_err(|error| {
                        boundary_decode_to_provider_error("read_data_source", None, error)
                    })?;
                    let provider = get_provider().lock().unwrap();
                    match $crate::CarinaProvider::read_data_source(&*provider, &proto_res) {
                        Ok(state) => proto_to_wit_state(&state).map_err(|error| {
                            boundary_encode_to_provider_error("read_data_source", None, error)
                        }),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn create(
                    id: wit_types::ResourceId,
                    request: wit_types::CreateRequest,
                ) -> Result<wit_types::CreateOutcome, wit_types::ProviderError> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_create_request(request)
                        .map_err(|error| {
                            boundary_decode_to_provider_error("create", None, error)
                        })?;
                    let provider = get_provider().lock().unwrap();
                    match $crate::CarinaProvider::create(&*provider, &proto_id, proto_request) {
                        Ok(outcome) => proto_to_wit_create_outcome(&outcome)
                            .map_err(|error| {
                                boundary_encode_to_provider_error("create", None, error)
                            }),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn update(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::UpdateRequest,
                ) -> Result<wit_types::UpdateOutcome, wit_types::ProviderError> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_update_request(request, &proto_id)
                        .map_err(|error| {
                            boundary_decode_to_provider_error("update", None, error)
                        })?;
                    let provider = get_provider().lock().unwrap();
                    match $crate::CarinaProvider::update(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto_request,
                    ) {
                        Ok(outcome) => proto_to_wit_update_outcome(&outcome)
                            .map_err(|error| {
                                boundary_encode_to_provider_error("update", None, error)
                            }),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn delete(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::DeleteRequest,
                ) -> Result<(), wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_delete_request(request);
                    match $crate::CarinaProvider::delete(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto_request,
                    ) {
                        Ok(()) => Ok(()),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn required_permissions(
                    id: wit_types::ResourceId,
                    operation: exports::carina::provider::provider::PlanOp,
                ) -> Vec<String> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::required_permissions(
                            &*provider,
                            &proto_id,
                            wit_to_sdk_plan_op(operation),
                        )
                    };
                    provider_export_or_trap("required_permissions", result)
                }

                fn satisfier_hint(
                    target_id: wit_types::ResourceId,
                    attr_path: Vec<String>,
                ) -> Vec<wit_types::BindingPattern> {
                    let proto_id = wit_to_proto_resource_id(&target_id);
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::satisfier_hint(&*provider, &proto_id, &attr_path)
                    };
                    provider_export_or_trap("satisfier_hint", result)
                        .iter()
                        .map(sdk_to_wit_binding_pattern)
                        .collect()
                }

                fn provider_config_completions() -> String {
                    let provider = get_provider().lock().unwrap();
                    let completions = $crate::CarinaProvider::config_completions(&*provider);
                    serde_json::to_string(&completions)
                        .expect("provider config completion serialization is infallible")
                }

                fn identity_attributes() -> Vec<String> {
                    let provider = get_provider().lock().unwrap();
                    $crate::CarinaProvider::identity_attributes(&*provider)
                }

                fn validate_custom_type(
                    ty: wit_types::TypeIdentity,
                    value: String,
                ) -> Result<(), wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    $crate::CarinaProvider::validate_custom_type(
                        &*provider,
                        &wit_to_proto_type_identity(&ty),
                        &value,
                    )
                    .map_err(validate_string_to_provider_error)
                }

                fn get_enum_aliases() -> String {
                    let provider = get_provider().lock().unwrap();
                    let aliases = $crate::CarinaProvider::enum_aliases(&*provider);
                    serde_json::to_string(&aliases)
                        .expect("provider enum alias serialization is infallible")
                }

                fn normalize_desired(
                    resources: Vec<wit_types::ResourceDef>,
                ) -> Result<Vec<wit_types::ResourceDef>, wit_types::ProviderError> {
                    let proto_resources = resources
                        .iter()
                        .map(wit_to_proto_resource)
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error("normalize_desired", None, error)
                        })?;
                    // Keep the mutex guard's lexical scope limited to the
                    // provider call; bridge result and encoding work does not
                    // require access to the shared provider.
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::normalize_desired(&*provider, proto_resources)
                    }
                    .map_err(proto_to_wit_provider_error)?;
                    result
                        .iter()
                        .map(proto_to_wit_resource)
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error("normalize_desired", None, error)
                        })
                }

                fn normalize_state(
                    states: Vec<(String, wit_types::State)>,
                ) -> Result<Vec<(String, wit_types::State)>, wit_types::ProviderError> {
                    let proto_states = states
                        .iter()
                        .map(|(key, state)| {
                            let parsed_id = helpers::parse_resource_id_string(key);
                            wit_to_proto_state(&parsed_id, state)
                                .map(|state| (key.clone(), state))
                        })
                        .collect::<Result<HashMap<_, _>, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error("normalize_state", None, error)
                        })?;
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::normalize_state(&*provider, proto_states)
                    }
                    .map_err(proto_to_wit_provider_error)?;
                    result
                        .into_iter()
                        .map(|(key, state)| {
                            proto_to_wit_state(&state).map(|state| (key, state))
                        })
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error("normalize_state", None, error)
                        })
                }

                fn hydrate_read_state(
                    states: Vec<(String, wit_types::State)>,
                    saved_attrs: Vec<(String, Vec<(String, wit_types::Value)>)>,
                ) -> Result<Vec<(String, wit_types::State)>, wit_types::ProviderError> {
                    let mut proto_states = states
                        .iter()
                        .map(|(key, state)| {
                            let parsed_id = helpers::parse_resource_id_string(key);
                            wit_to_proto_state(&parsed_id, state)
                                .map(|state| (key.clone(), state))
                        })
                        .collect::<Result<HashMap<_, _>, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error(
                                "hydrate_read_state",
                                Some("states"),
                                error,
                            )
                        })?;
                    let proto_saved = saved_attrs
                        .iter()
                        .map(|(key, attributes)| {
                            wit_to_proto_value_map(attributes)
                                .map(|attributes| (key.clone(), attributes))
                        })
                        .collect::<Result<HashMap<_, _>, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error(
                                "hydrate_read_state",
                                Some("saved attributes"),
                                error,
                            )
                        })?;
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        $crate::CarinaProvider::hydrate_read_state(
                            &*provider,
                            &mut proto_states,
                            &proto_saved,
                        )
                    }
                    .map_err(proto_to_wit_provider_error)?;
                    proto_states
                        .into_iter()
                        .map(|(key, state)| {
                            proto_to_wit_state(&state).map(|state| (key, state))
                        })
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error(
                                "hydrate_read_state",
                                None,
                                error,
                            )
                        })
                }

                fn merge_default_tags(
                    resources: Vec<wit_types::ResourceDef>,
                    default_tags: Vec<(String, wit_types::Value)>,
                ) -> Result<Vec<wit_types::ResourceDef>, wit_types::ProviderError> {
                    let mut proto_resources = resources
                        .iter()
                        .map(wit_to_proto_resource)
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_decode_to_provider_error(
                                "merge_default_tags",
                                Some("resources"),
                                error,
                            )
                        })?;
                    let proto_tags = wit_to_proto_value_map(&default_tags).map_err(|error| {
                        boundary_decode_to_provider_error(
                            "merge_default_tags",
                            Some("default tags"),
                            error,
                        )
                    })?;
                    let result = {
                        let provider = get_provider().lock().unwrap();
                        let schemas = $crate::CarinaProvider::schemas(&*provider);
                        $crate::CarinaProvider::merge_default_tags(
                            &*provider,
                            &mut proto_resources,
                            &proto_tags,
                            &schemas,
                        )
                    }
                    .map_err(proto_to_wit_provider_error)?;
                    proto_resources
                        .iter()
                        .map(proto_to_wit_resource)
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error("merge_default_tags", None, error)
                        })
                }
            }

            export!(WasmGuest);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_wit_json_container_values_are_rejected() {
        let errors = [
            decode_string_list_val("not json").unwrap_err(),
            decode_list_val("not json").unwrap_err(),
            decode_map_val("not json").unwrap_err(),
            decode_secret_val("not json").unwrap_err(),
        ];

        for (error, variant) in
            errors
                .into_iter()
                .zip(["string-list-val", "list-val", "map-val", "secret-val"])
        {
            assert!(error.to_string().contains(variant));
            assert!(std::error::Error::source(&error).is_some());
        }

        assert!(decode_list_val("{}").is_err());
        assert!(decode_map_val("[]").is_err());
    }

    #[test]
    fn null_wit_json_container_values_are_rejected() {
        assert!(decode_string_list_val("[null]").is_err());
        assert!(decode_list_val("[null]").is_err());
        assert!(decode_map_val(r#"{"key":null}"#).is_err());
        assert!(decode_secret_val("null").is_err());
    }

    #[test]
    fn malformed_secret_error_does_not_expose_payload() {
        let plaintext = "guest-boundary-secret-do-not-log";
        let error = decode_secret_val(&format!("\"{plaintext}"))
            .expect_err("unterminated secret JSON must fail");

        assert!(!error.to_string().contains(plaintext));
        assert!(!format!("{error:?}").contains(plaintext));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn wrong_container_shapes_do_not_expose_payloads() {
        let plaintext = "guest-wrong-shape-secret-do-not-log";
        let scalar = serde_json::to_string(plaintext).unwrap();
        let errors = [
            decode_string_list_val(&scalar).unwrap_err(),
            decode_list_val(&scalar).unwrap_err(),
            decode_map_val(&scalar).unwrap_err(),
            decode_string_list_val(&format!(r#"[{{"secret":"{plaintext}"}}]"#)).unwrap_err(),
        ];

        for error in errors {
            assert!(!error.to_string().contains(plaintext), "{error}");
            assert!(!format!("{error:?}").contains(plaintext), "{error:?}");
        }
    }

    #[test]
    fn nested_non_finite_float_is_rejected_during_guest_encoding() {
        let value = proto::Value::List(vec![proto::Value::Float(f64::NAN)]);

        let error = proto_value_to_json(&value)
            .expect_err("nested non-finite floats must fail guest boundary encoding");

        assert!(error.to_string().contains("non-finite float"));
    }
}
