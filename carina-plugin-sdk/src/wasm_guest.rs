//! WASM guest-side helpers for the carina-provider WIT interface.
//!
//! Provides type conversion functions between WIT guest types and protocol types.
//! The `export_provider!` macro generates the wit-bindgen bindings and Guest trait
//! implementation in the consumer crate.

use carina_provider_protocol::PERSISTED_SECRET_KEY;
use carina_provider_protocol::types as proto;

/// Decode the version-13 `desired-resource` record into the SDK's typed view.
///
/// This is public only because [`crate::export_provider!`] expands in the
/// provider crate. Provider implementations receive [`crate::DesiredResource`]
/// and never the raw deferred-name list.
#[doc(hidden)]
pub fn decode_desired_resource(
    resource: proto::Resource,
    deferred: Vec<String>,
) -> Result<crate::DesiredResource, crate::DesiredResourceDecodeError> {
    crate::DesiredResource::decode(resource, deferred)
}

/// Visit an opaque normalized resource for guest-boundary encoding.
///
/// This is public only because [`crate::export_provider!`] expands in the
/// provider crate. It deliberately exposes neither the raw resource nor its
/// attribute map.
#[doc(hidden)]
pub fn visit_normalized_resource<T>(
    resource: &crate::NormalizedResource,
    visitor: impl FnOnce(
        &proto::ResourceId,
        std::collections::btree_map::Iter<'_, String, proto::Value>,
    ) -> T,
) -> T {
    visitor(&resource.resource.id, resource.resource.attributes.iter())
}

/// Render one error chain for protocol-v13 metadata's string-only failure
/// channel. Each source is appended at most once even when a legacy outer
/// display already includes its immediate source text.
#[doc(hidden)]
pub fn render_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = Vec::new();
    let mut current = Some(error);
    while let Some(error) = current {
        let text = error.to_string();
        if rendered
            .last()
            .is_none_or(|outer: &String| !outer.ends_with(&text))
        {
            rendered.push(text);
        }
        current = error.source();
    }
    rendered.join(": ")
}

/// Decode a plain WIT managed-resource identifier into the protocol's
/// validated representation before a guest provider can observe the request.
pub fn wit_to_proto_applied_identifier(
    identifier: String,
) -> Result<proto::AppliedIdentifier, proto::EmptyAppliedIdentifier> {
    proto::AppliedIdentifier::new(identifier)
}

/// Resolve the effective directives for a WIT resource record, whose version-13
/// shape carries no authored directives of its own.
#[doc(hidden)]
pub fn resolve_wit_resource_directives(
    resource_type: &str,
    kind: carina_core::schema::SchemaKind,
    schemas: &[carina_core::schema::ValidatedResourceSchema],
) -> proto::Directives {
    let schema = schemas.iter().find(|schema| {
        let schema = schema.as_schema();
        schema.resource_type() == resource_type && schema.kind() == kind
    });
    let schema = match schema {
        Some(schema) => {
            carina_core::provider::ProviderDirectiveSchema::Available(schema.as_schema())
        }
        None => carina_core::provider::ProviderDirectiveSchema::Unavailable,
    };
    let directives = carina_core::provider::ProviderDirectives::resolve(
        &carina_core::resource::Directives::default(),
        schema,
    );
    carina_core::protocol_convert::directives_to_protocol(&directives)
}

// -- JSON conversion helpers --

/// JSON-backed WIT value variant being decoded.
#[derive(Debug, Clone, Copy)]
pub enum WitValueVariant {
    StringListVal,
    ListVal,
    MapVal,
    SecretVal,
    RedactedSecretVal,
}

impl std::fmt::Display for WitValueVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StringListVal => "string-list-val",
            Self::ListVal => "list-val",
            Self::MapVal => "map-val",
            Self::SecretVal => "secret-val",
            Self::RedactedSecretVal => "redacted-secret-val",
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
    MalformedPersistedSecret,
    ReservedPersistedKey,
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
        }
    }
}

impl std::error::Error for WasmValueDecodeError {}

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
                .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
            Ok(proto::Value::Map(entries))
        }
        serde_json::Value::Null => Err(WasmValueDecodeError::new(
            wit_variant,
            WasmValueDecodeErrorKind::Null,
        )),
    }
}

fn json_to_proto_persisted_value_for_variant(
    v: serde_json::Value,
    wit_variant: WitValueVariant,
) -> Result<proto::PersistedValue, WasmValueDecodeError> {
    match v {
        serde_json::Value::Bool(value) => Ok(proto::PersistedValue::Bool(value)),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(proto::PersistedValue::Int(value))
            } else if let Some(value) = value.as_f64() {
                Ok(proto::PersistedValue::Float(value))
            } else {
                Err(WasmValueDecodeError::new(
                    wit_variant,
                    WasmValueDecodeErrorKind::UnsupportedNumber,
                ))
            }
        }
        serde_json::Value::String(value) => Ok(proto::PersistedValue::String(value)),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| json_to_proto_persisted_value_for_variant(value, wit_variant))
            .collect::<Result<Vec<_>, _>>()
            .map(proto::PersistedValue::List),
        serde_json::Value::Object(entries) => {
            if entries.contains_key(PERSISTED_SECRET_KEY) {
                if entries.len() != 1 {
                    return Err(WasmValueDecodeError::new(
                        wit_variant,
                        WasmValueDecodeErrorKind::MalformedPersistedSecret,
                    ));
                }
                let Some(serde_json::Value::String(digest)) = entries.get(PERSISTED_SECRET_KEY)
                else {
                    return Err(WasmValueDecodeError::new(
                        wit_variant,
                        WasmValueDecodeErrorKind::MalformedPersistedSecret,
                    ));
                };
                return Ok(proto::PersistedValue::RedactedSecret(digest.clone()));
            }
            let entries = entries
                .into_iter()
                .map(|(key, value)| {
                    let decoded_key = if let Some(key) = key.strip_prefix("$$") {
                        format!("${key}")
                    } else if key.starts_with('$') {
                        return Err(WasmValueDecodeError::new(
                            wit_variant,
                            WasmValueDecodeErrorKind::ReservedPersistedKey,
                        )
                        .prepend_attribute(&key));
                    } else {
                        key.clone()
                    };
                    json_to_proto_persisted_value_for_variant(value, wit_variant)
                        .map(|value| (decoded_key, value))
                        .map_err(|error| error.prepend_attribute(&key))
                })
                .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
            Ok(proto::PersistedValue::Map(entries))
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

pub fn decode_persisted_string_list_val(
    json: &str,
) -> Result<proto::PersistedValue, WasmValueDecodeError> {
    decode_string_list_val(json).map(proto::PersistedValue::from)
}

pub fn decode_persisted_list_val(
    json: &str,
) -> Result<proto::PersistedValue, WasmValueDecodeError> {
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
    json_to_proto_persisted_value_for_variant(value, WitValueVariant::ListVal)
}

pub fn decode_persisted_map_val(json: &str) -> Result<proto::PersistedValue, WasmValueDecodeError> {
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
    json_to_proto_persisted_value_for_variant(value, WitValueVariant::MapVal)
}

pub fn decode_redacted_secret_val(digest: &str) -> proto::PersistedValue {
    proto::PersistedValue::RedactedSecret(digest.to_string())
}

/// Render a decode error for boundaries that cannot transport a typed error.
pub fn format_wasm_value_decode_error(error: &WasmValueDecodeError) -> String {
    error.to_string()
}

/// Failure to encode a proto value into a JSON-backed WIT value.
#[derive(Debug)]
pub struct WasmValueEncodeError {
    kind: WasmValueEncodeErrorKind,
    attribute_path: Vec<String>,
}

#[derive(Debug)]
enum WasmValueEncodeErrorKind {
    NonFiniteFloat(f64),
}

impl WasmValueEncodeError {
    fn non_finite_float(value: f64) -> Self {
        Self {
            kind: WasmValueEncodeErrorKind::NonFiniteFloat(value),
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
        match &self.kind {
            WasmValueEncodeErrorKind::NonFiniteFloat(value) => {
                write!(f, ": non-finite float {value} is not valid JSON")
            }
        }
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

pub fn proto_persisted_value_to_json(
    v: &proto::PersistedValue,
) -> Result<serde_json::Value, WasmValueEncodeError> {
    match v {
        proto::PersistedValue::Bool(value) => Ok(serde_json::Value::Bool(*value)),
        proto::PersistedValue::Int(value) => Ok(serde_json::Value::Number((*value).into())),
        proto::PersistedValue::Float(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| WasmValueEncodeError::non_finite_float(*value)),
        proto::PersistedValue::String(value) => Ok(serde_json::Value::String(value.clone())),
        proto::PersistedValue::RedactedSecret(digest) => Ok(serde_json::json!({
            PERSISTED_SECRET_KEY: digest,
        })),
        proto::PersistedValue::StringList(values) => Ok(serde_json::Value::Array(
            values
                .iter()
                .map(|value| serde_json::Value::String(value.clone()))
                .collect(),
        )),
        proto::PersistedValue::List(values) => values
            .iter()
            .map(proto_persisted_value_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        proto::PersistedValue::Map(values) => values
            .iter()
            .map(|(key, value)| {
                let encoded_key = if key.starts_with('$') {
                    format!("${key}")
                } else {
                    key.clone()
                };
                proto_persisted_value_to_json(value)
                    .map(|value| (encoded_key, value))
                    .map_err(|error| error.prepend_attribute(key))
            })
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(serde_json::Value::Object),
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
        $crate::export_provider!(@internal $provider_type, {
            path: "../carina-plugin-wit/wit",
            world: "carina-provider",
        });
    };
    ($provider_type:ty, http) => {
        $crate::export_provider!(@internal $provider_type, {
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
    };
    (@internal $provider_type:ty, { $($bindgen:tt)* }) => {
        mod __carina_wasm_guest {
            wit_bindgen::generate!({
                $($bindgen)*
            });

            use super::*;
            use $crate::types as proto;
            use $crate::wasm_guest as helpers;
            use std::collections::{BTreeMap, HashMap};

            // Type aliases for the generated types
            use carina::provider::types as wit_types;

            fn get_provider() -> &'static ::std::sync::Mutex<$provider_type> {
                static PROVIDER: ::std::sync::OnceLock<::std::sync::Mutex<$provider_type>> =
                    ::std::sync::OnceLock::new();
                PROVIDER.get_or_init(|| ::std::sync::Mutex::new(<$provider_type>::default()))
            }

            fn get_provider_schema_cache() -> Result<
                &'static $crate::ProviderSchemaCache,
                &'static $crate::core::schema::ProviderSchemaValidationError,
            > {
                static CACHE: ::std::sync::OnceLock<Result<
                    $crate::ProviderSchemaCache,
                    $crate::core::schema::ProviderSchemaValidationError,
                >> = ::std::sync::OnceLock::new();
                CACHE.get_or_init(|| {
                    let provider = get_provider().lock().unwrap();
                    $crate::ProviderSchemaCache::from_provider(&*provider)
                })
                .as_ref()
            }

            fn schema_metadata_error_json(
                operation: &str,
                error: &(dyn ::std::error::Error + 'static),
            ) -> String {
                // Protocol v13 metadata exports return a bare string rather
                // than `result`. A JSON string is deliberately the wrong
                // top-level shape for both schema records and config maps, so
                // the host reports its normal typed schema-decode failure
                // without trapping the guest.
                serde_json::Value::String(format!(
                    "{operation}: {}",
                    helpers::render_error_chain(error)
                ))
                .to_string()
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
                    // Desired-state `normalize_desired` round trips are
                    // host-sealed and never carry `SecretVal` (carina#3800).
                    // CRUD inputs decode the inner JSON into an ordinary
                    // provider value because
                    // `proto::Value` has no `Secret` arm. Malformed or null
                    // payloads remain boundary errors. Providers MUST NOT log
                    // or persist the plaintext.
                    wit_types::Value::SecretVal(json) => helpers::decode_secret_val(json),
                }
            }

            fn wit_to_proto_persisted_value(
                v: &wit_types::PersistedValue,
            ) -> Result<proto::PersistedValue, helpers::WasmValueDecodeError> {
                match v {
                    wit_types::PersistedValue::BoolVal(value) => {
                        Ok(proto::PersistedValue::Bool(*value))
                    }
                    wit_types::PersistedValue::IntVal(value) => {
                        Ok(proto::PersistedValue::Int(*value))
                    }
                    wit_types::PersistedValue::FloatVal(value) => {
                        Ok(proto::PersistedValue::Float(*value))
                    }
                    wit_types::PersistedValue::StrVal(value) => {
                        Ok(proto::PersistedValue::String(value.clone()))
                    }
                    wit_types::PersistedValue::RedactedSecretVal(digest) => {
                        Ok(helpers::decode_redacted_secret_val(digest))
                    }
                    wit_types::PersistedValue::StringListVal(json) => {
                        helpers::decode_persisted_string_list_val(json)
                    }
                    wit_types::PersistedValue::ListVal(json) => {
                        helpers::decode_persisted_list_val(json)
                    }
                    wit_types::PersistedValue::MapVal(json) => {
                        helpers::decode_persisted_map_val(json)
                    }
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

            fn proto_to_wit_persisted_value(
                v: &proto::PersistedValue,
            ) -> Result<wit_types::PersistedValue, helpers::WasmValueEncodeError> {
                Ok(match v {
                    proto::PersistedValue::Bool(value) => {
                        wit_types::PersistedValue::BoolVal(*value)
                    }
                    proto::PersistedValue::Int(value) => {
                        wit_types::PersistedValue::IntVal(*value)
                    }
                    proto::PersistedValue::Float(value) => {
                        wit_types::PersistedValue::FloatVal(*value)
                    }
                    proto::PersistedValue::String(value) => {
                        wit_types::PersistedValue::StrVal(value.clone())
                    }
                    proto::PersistedValue::RedactedSecret(digest) => {
                        wit_types::PersistedValue::RedactedSecretVal(digest.clone())
                    }
                    proto::PersistedValue::StringList(values) => {
                        wit_types::PersistedValue::StringListVal(
                            serde_json::to_string(values).unwrap(),
                        )
                    }
                    proto::PersistedValue::List(_) => {
                        let json = helpers::proto_persisted_value_to_json(v)?;
                        wit_types::PersistedValue::ListVal(serde_json::to_string(&json).unwrap())
                    }
                    proto::PersistedValue::Map(_) => {
                        let json = helpers::proto_persisted_value_to_json(v)?;
                        wit_types::PersistedValue::MapVal(serde_json::to_string(&json).unwrap())
                    }
                })
            }

            fn wit_to_proto_value_map(
                entries: &[(String, wit_types::Value)],
            ) -> Result<BTreeMap<String, proto::Value>, helpers::WasmValueDecodeError> {
                entries
                    .iter()
                    .map(|(key, value): &(String, wit_types::Value)| {
                        wit_to_proto_value(value)
                            .map(|value| (key.clone(), value))
                            .map_err(|error| error.prepend_attribute(key))
                    })
                    .collect()
            }

            fn wit_to_proto_persisted_value_map(
                entries: &[(String, wit_types::PersistedValue)],
            ) -> Result<BTreeMap<String, proto::PersistedValue>, helpers::WasmValueDecodeError> {
                entries
                    .iter()
                    .map(|(key, value)| {
                        wit_to_proto_persisted_value(value)
                            .map(|value| (key.clone(), value))
                            .map_err(|error| error.prepend_attribute(key))
                    })
                    .collect()
            }

            fn proto_to_wit_value_map(
                map: &BTreeMap<String, proto::Value>,
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

            fn proto_to_wit_persisted_value_map(
                map: &BTreeMap<String, proto::PersistedValue>,
            ) -> Result<
                Vec<(String, wit_types::PersistedValue)>,
                helpers::WasmValueEncodeError,
            > {
                map.iter()
                    .map(|(key, value)| {
                        proto_to_wit_persisted_value(value)
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

            fn wit_to_proto_persisted_state(
                id: &proto::ResourceId,
                state: &wit_types::PersistedState,
            ) -> Result<proto::PersistedState, helpers::WasmValueDecodeError> {
                Ok(proto::PersistedState {
                    id: id.clone(),
                    identifier: state.identifier.clone(),
                    attributes: wit_to_proto_persisted_value_map(&state.attributes)?,
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

            fn proto_to_wit_persisted_state(
                state: &proto::PersistedState,
            ) -> Result<wit_types::PersistedState, helpers::WasmValueEncodeError> {
                Ok(wit_types::PersistedState {
                    identifier: state.identifier.clone(),
                    attributes: proto_to_wit_persisted_value_map(&state.attributes)?,
                    exists: state.exists,
                })
            }

            fn proto_to_wit_applied_state(
                state: &proto::AppliedState,
            ) -> Result<wit_types::AppliedState, helpers::WasmValueEncodeError> {
                let proto::AppliedState {
                    id: _protocol_id,
                    identifier,
                    attributes,
                } = state;
                Ok(wit_types::AppliedState {
                    identifier: identifier.as_str().to_string(),
                    attributes: proto_to_wit_value_map(attributes)?,
                })
            }

            fn proto_to_wit_read_outcome(
                outcome: &proto::ReadOutcome,
            ) -> Result<wit_types::ReadOutcome, helpers::WasmValueEncodeError> {
                Ok(match outcome {
                    proto::ReadOutcome::NotFound { id: _protocol_id } => {
                        wit_types::ReadOutcome::NotFound
                    }
                    proto::ReadOutcome::Found { state } => {
                        wit_types::ReadOutcome::Found(proto_to_wit_applied_state(state)?)
                    }
                })
            }

            fn proto_to_wit_applied_persisted_state(
                state: &proto::AppliedPersistedState,
            ) -> Result<wit_types::AppliedPersistedState, helpers::WasmValueEncodeError> {
                let proto::AppliedPersistedState {
                    id: _protocol_id,
                    identifier,
                    attributes,
                } = state;
                Ok(wit_types::AppliedPersistedState {
                    identifier: identifier.as_str().to_string(),
                    attributes: proto_to_wit_persisted_value_map(attributes)?,
                })
            }

            fn proto_to_wit_update_outcome(
                outcome: &proto::UpdateOutcome,
            ) -> Result<wit_types::UpdateOutcome, helpers::WasmValueEncodeError> {
                Ok(match outcome {
                    proto::UpdateOutcome::Success { state } => {
                        wit_types::UpdateOutcome::Success(proto_to_wit_applied_persisted_state(
                            state,
                        )?)
                    }
                    proto::UpdateOutcome::PartialSuccess { state, diagnostic } => {
                        wit_types::UpdateOutcome::PartialSuccess(
                            wit_types::UpdatePartialSuccess {
                                state: proto_to_wit_applied_persisted_state(state)?,
                                diagnostic: wit_types::PartialReadDiagnostic {
                                    reason: diagnostic.reason.clone(),
                                    missing_attributes: diagnostic.missing_attributes.clone(),
                                },
                            },
                        )
                    }
                })
            }

            fn sdk_to_wit_pending<T: serde::Serialize>(
                pending: &$crate::Pending<T>,
            ) -> Result<wit_types::PendingOperation, serde_json::Error> {
                Ok(wit_types::PendingOperation {
                    token: serde_json::to_string(&pending.token)?,
                    poll_after_secs: pending.poll_after_secs,
                })
            }

            fn wit_to_sdk_pending<T: serde::de::DeserializeOwned>(
                pending: wit_types::PendingOperation,
            ) -> Result<$crate::Pending<T>, serde_json::Error> {
                Ok($crate::Pending {
                    token: serde_json::from_str(&pending.token)?,
                    poll_after_secs: pending.poll_after_secs,
                })
            }

            fn proto_to_wit_sent(sent: &proto::Sent) -> wit_types::SentAttributes {
                match sent {
                    proto::Sent::AllRequestAttributes => wit_types::SentAttributes::All,
                    proto::Sent::Only(attributes) => {
                        wit_types::SentAttributes::Only(attributes.iter().cloned().collect())
                    }
                }
            }

            fn wit_to_proto_sent(sent: wit_types::SentAttributes) -> proto::Sent {
                match sent {
                    wit_types::SentAttributes::All => proto::Sent::AllRequestAttributes,
                    wit_types::SentAttributes::Only(attributes) => {
                        proto::Sent::only(attributes)
                    }
                }
            }

            fn proto_to_wit_partial_read_diagnostic(
                diagnostic: proto::PartialReadDiagnostic,
            ) -> wit_types::PartialReadDiagnostic {
                let proto::PartialReadDiagnostic {
                    reason,
                    missing_attributes,
                } = diagnostic;
                wit_types::PartialReadDiagnostic {
                    reason,
                    missing_attributes,
                }
            }

            fn proto_to_wit_reference_scope(
                scope: proto::ReferenceScope,
            ) -> wit_types::ReferenceScope {
                match scope {
                    proto::ReferenceScope::Available(values) => wit_types::ReferenceScope::Available(
                        wit_types::ReferenceScopeValues {
                            partition: values.partition().to_string(),
                            region: values.region().to_string(),
                            account: values.account().to_string(),
                        },
                    ),
                    proto::ReferenceScope::Unavailable => wit_types::ReferenceScope::Unavailable,
                }
            }

            fn proto_to_wit_created_state(
                identifier: proto::AppliedIdentifier,
                attributes: BTreeMap<String, proto::Value>,
            ) -> Result<wit_types::AppliedState, helpers::WasmValueEncodeError> {
                Ok(wit_types::AppliedState {
                    identifier: identifier.into_inner(),
                    attributes: proto_to_wit_value_map(&attributes)?,
                })
            }

            enum CreateStepEncodeFailure {
                /// A dispatched continuation failed token encoding. An
                /// initial create without an identifier must trap so the host
                /// classifies its per-call egress record; a poll can instead
                /// reuse its last decodable token. Once an identifier exists,
                /// the failure becomes `Created` plus `ReadFailed`.
                PendingToken {
                    identifier: Option<String>,
                    sent: wit_types::SentAttributes,
                    poll_after_secs: u32,
                    error: wit_types::ProviderError,
                },
                /// A finished result established an identifier; only its
                /// returned attributes failed to cross the boundary.
                CreatedAttributes(wit_types::CreatedFailure),
            }

            impl CreateStepEncodeFailure {
                fn created_failure(
                    identifier: proto::AppliedIdentifier,
                    sent: wit_types::SentAttributes,
                    error: wit_types::ProviderError,
                ) -> wit_types::CreatedFailure {
                    wit_types::CreatedFailure {
                        identifier: identifier.into_inner(),
                        read: wit_types::Observation::ReadFailed(error.clone()),
                        sent,
                        error,
                    }
                }

                fn into_create_verdict(self) -> wit_types::CreateVerdict {
                    match self {
                        Self::PendingToken {
                            identifier,
                            sent,
                            poll_after_secs: _poll_after_secs,
                            error,
                        } => match identifier {
                            Some(identifier) => {
                                wit_types::CreateVerdict::Created(wit_types::CreatedFailure {
                                    identifier,
                                    read: wit_types::Observation::ReadFailed(error.clone()),
                                    sent,
                                    error,
                                })
                            }
                            None => panic!(
                                "WASM boundary failed to encode a dispatched create continuation token without an identifier; refusing to report the create as untracked"
                            ),
                        },
                        Self::CreatedAttributes(created) => {
                            wit_types::CreateVerdict::Created(created)
                        }
                    }
                }

                fn into_poll_verdict(
                    self,
                    previous: wit_types::PendingCreate,
                ) -> wit_types::PollVerdict {
                    match self {
                        Self::PendingToken {
                            identifier,
                            sent,
                            poll_after_secs,
                            error,
                        } => {
                            let wit_types::PendingCreate {
                                operation,
                                identifier: _previous_identifier,
                                sent: _previous_sent,
                            } = previous;
                            let wit_types::PendingOperation {
                                token,
                                poll_after_secs: _previous_poll_after_secs,
                            } = operation;
                            wit_types::PollVerdict::InFlight(wit_types::CreateInFlight {
                                pending: wit_types::PendingCreate {
                                    operation: wit_types::PendingOperation {
                                        token,
                                        poll_after_secs,
                                    },
                                    identifier,
                                    sent,
                                },
                                error,
                            })
                        }
                        Self::CreatedAttributes(created) => {
                            wit_types::PollVerdict::Created(created)
                        }
                    }
                }
            }

            enum ResumeStepEncodeFailure {
                /// A resume reuses its last decodable token while retaining
                /// any identifier and poll interval returned by this call.
                PendingToken {
                    identifier: Option<String>,
                    poll_after_secs: u32,
                    error: wit_types::ProviderError,
                },
                Created(wit_types::ResumedFailure),
                /// A resent create still has the persisted pre-call resume
                /// continuation available if its replacement token cannot be
                /// encoded.
                Resent(CreateStepEncodeFailure),
            }

            impl ResumeStepEncodeFailure {
                fn created(
                    identifier: proto::AppliedIdentifier,
                    error: wit_types::ProviderError,
                ) -> Self {
                    Self::Created(wit_types::ResumedFailure {
                        identifier: identifier.into_inner(),
                        read: wit_types::Observation::ReadFailed(error.clone()),
                        error,
                    })
                }

                fn into_resume_verdict(
                    self,
                    previous: wit_types::ResumePending,
                ) -> wit_types::ResumeVerdict {
                    match self {
                        Self::PendingToken {
                            identifier,
                            poll_after_secs,
                            error,
                        } => {
                            let wit_types::ResumePending {
                                operation,
                                identifier: _previous_identifier,
                            } = previous;
                            let wit_types::PendingOperation {
                                token,
                                poll_after_secs: _previous_poll_after_secs,
                            } = operation;
                            wit_types::ResumeVerdict::InFlight(wit_types::ResumeInFlight {
                                pending: wit_types::ResumePending {
                                    operation: wit_types::PendingOperation {
                                        token,
                                        poll_after_secs,
                                    },
                                    identifier,
                                },
                                error,
                            })
                        }
                        Self::Created(created) => wit_types::ResumeVerdict::Created(created),
                        Self::Resent(error) => {
                            error.into_resent_resume_verdict(previous)
                        }
                    }
                }
            }

            impl CreateStepEncodeFailure {
                fn into_resent_resume_verdict(
                    self,
                    previous: wit_types::ResumePending,
                ) -> wit_types::ResumeVerdict {
                    match self {
                        Self::PendingToken {
                            identifier: None,
                            sent: _sent,
                            poll_after_secs: _poll_after_secs,
                            error,
                        } => wit_types::ResumeVerdict::InFlight(
                            wit_types::ResumeInFlight {
                                pending: previous,
                                error,
                            },
                        ),
                        failure => wit_types::ResumeVerdict::Resent(
                            failure.into_create_verdict(),
                        ),
                    }
                }
            }

            fn sdk_to_wit_pending_create<T: serde::Serialize>(
                pending: proto::PendingCreate<T>,
                operation: &'static str,
            ) -> Result<wit_types::PendingCreate, CreateStepEncodeFailure> {
                let (token, identifier, sent, poll_after_secs) = pending.into_parts();
                let sent = proto_to_wit_sent(&sent);
                let token = match serde_json::to_string(&token) {
                    Ok(token) => token,
                    Err(error) => {
                        let error = boundary_token_encode_to_provider_error(operation, error);
                        return Err(CreateStepEncodeFailure::PendingToken {
                            identifier: identifier.map(proto::AppliedIdentifier::into_inner),
                            sent,
                            poll_after_secs,
                            error,
                        });
                    }
                };
                Ok(wit_types::PendingCreate {
                    operation: wit_types::PendingOperation {
                        token,
                        poll_after_secs,
                    },
                    identifier: identifier.map(proto::AppliedIdentifier::into_inner),
                    sent,
                })
            }

            fn sdk_to_wit_resume_pending<T: serde::Serialize>(
                pending: proto::ResumePending<T>,
                operation: &'static str,
            ) -> Result<wit_types::ResumePending, ResumeStepEncodeFailure> {
                let (token, identifier, poll_after_secs) = pending.into_parts();
                let identifier = identifier.map(proto::AppliedIdentifier::into_inner);
                let token = serde_json::to_string(&token).map_err(|error| {
                    ResumeStepEncodeFailure::PendingToken {
                        identifier: identifier.clone(),
                        poll_after_secs,
                        error: boundary_token_encode_to_provider_error(operation, error),
                    }
                })?;
                Ok(wit_types::ResumePending {
                    operation: wit_types::PendingOperation {
                        token,
                        poll_after_secs,
                    },
                    identifier,
                })
            }

            fn sdk_to_wit_create_step<T: serde::Serialize>(
                step: $crate::ProtocolCreateStep<T>,
                operation: &'static str,
            ) -> Result<wit_types::CreateStep, CreateStepEncodeFailure> {
                sdk_to_wit_protocol_create_step(step, operation)
            }

            fn sdk_to_wit_protocol_create_step<T: serde::Serialize>(
                step: proto::CreateStep<
                    BTreeMap<String, proto::Value>,
                    proto::PartialReadDiagnostic,
                    T,
                >,
                operation: &'static str,
            ) -> Result<wit_types::CreateStep, CreateStepEncodeFailure> {
                match step {
                    proto::CreateStep::Success {
                        identifier,
                        attributes,
                    } => match proto_to_wit_created_state(identifier.clone(), attributes) {
                        Ok(state) => Ok(wit_types::CreateStep::Success(state)),
                        Err(error) => {
                            let error =
                                boundary_encode_to_provider_error(operation, None, error);
                            Err(CreateStepEncodeFailure::CreatedAttributes(
                                CreateStepEncodeFailure::created_failure(
                                    identifier,
                                    wit_types::SentAttributes::All,
                                    error,
                                ),
                            ))
                        }
                    },
                    proto::CreateStep::PartialSuccess {
                        identifier,
                        attributes,
                        diagnostic,
                    } => match proto_to_wit_created_state(identifier.clone(), attributes) {
                        Ok(state) => Ok(wit_types::CreateStep::PartialSuccess(
                                wit_types::CreatePartialSuccess {
                                    state,
                                    diagnostic: proto_to_wit_partial_read_diagnostic(diagnostic),
                                },
                            )),
                        Err(error) => {
                            let error =
                                boundary_encode_to_provider_error(operation, None, error);
                            Err(CreateStepEncodeFailure::CreatedAttributes(
                                CreateStepEncodeFailure::created_failure(
                                    identifier,
                                    wit_types::SentAttributes::All,
                                    error,
                                ),
                            ))
                        }
                    },
                    proto::CreateStep::Pending(pending) => sdk_to_wit_pending_create(
                        pending,
                        operation,
                    )
                    .map(wit_types::CreateStep::Pending),
                }
            }

            fn proto_to_wit_observation(
                observation: proto::Observation<proto::ReadOutcome, proto::ProviderError>,
                operation: &'static str,
            ) -> wit_types::Observation {
                match observation {
                    proto::Observation::Read(read) => match proto_to_wit_read_outcome(&read) {
                        Ok(read) => wit_types::Observation::Read(read),
                        Err(error) => wit_types::Observation::ReadFailed(
                            boundary_encode_to_provider_error(operation, Some("observation"), error),
                        ),
                    },
                    proto::Observation::ReadFailed(error) => {
                        wit_types::Observation::ReadFailed(proto_to_wit_provider_error(error))
                    }
                }
            }

            fn sdk_to_wit_created_failure(
                created: proto::CreatedFailure<proto::ReadOutcome, proto::ProviderError>,
                operation: &'static str,
            ) -> wit_types::CreatedFailure {
                let (identifier, observation, sent, error) = created.into_parts();
                wit_types::CreatedFailure {
                    identifier: identifier.into_inner(),
                    read: proto_to_wit_observation(observation, operation),
                    sent: proto_to_wit_sent(&sent),
                    error: proto_to_wit_provider_error(error),
                }
            }

            fn sdk_to_wit_resumed_failure(
                created: proto::ResumedFailure<proto::ReadOutcome, proto::ProviderError>,
                operation: &'static str,
            ) -> wit_types::ResumedFailure {
                let (identifier, observation, error) = created.into_parts();
                wit_types::ResumedFailure {
                    identifier: identifier.into_inner(),
                    read: proto_to_wit_observation(observation, operation),
                    error: proto_to_wit_provider_error(error),
                }
            }

            fn sdk_to_wit_create_verdict<T: serde::Serialize>(
                verdict: proto::CreateVerdict<proto::ReadOutcome, T, proto::ProviderError>,
                operation: &'static str,
            ) -> Result<wit_types::CreateVerdict, CreateStepEncodeFailure> {
                match verdict {
                    proto::CreateVerdict::Untracked(error) => Ok(
                        wit_types::CreateVerdict::Untracked(proto_to_wit_provider_error(error)),
                    ),
                    proto::CreateVerdict::Created(created) => Ok(
                        wit_types::CreateVerdict::Created(sdk_to_wit_created_failure(
                            created, operation,
                        )),
                    ),
                    proto::CreateVerdict::Discarded(error) => Ok(
                        wit_types::CreateVerdict::Discarded(proto_to_wit_provider_error(error)),
                    ),
                    proto::CreateVerdict::InFlight(in_flight) => {
                        let proto::CreateInFlight { pending, error } = in_flight;
                        sdk_to_wit_pending_create(pending, operation).map(|pending| {
                            wit_types::CreateVerdict::InFlight(wit_types::CreateInFlight {
                                    pending,
                                    error: proto_to_wit_provider_error(error),
                                })
                        })
                    }
                }
            }

            fn sdk_to_wit_poll_verdict<T: serde::Serialize>(
                verdict: $crate::ProtocolPollVerdict<T>,
                operation: &'static str,
                previous_pending: wit_types::PendingCreate,
            ) -> wit_types::PollVerdict {
                match verdict {
                    proto::PollVerdict::Untracked(error) => {
                        wit_types::PollVerdict::Untracked(proto_to_wit_provider_error(error))
                    }
                    proto::PollVerdict::Created(created) => wit_types::PollVerdict::Created(
                        sdk_to_wit_created_failure(created, operation),
                    ),
                    proto::PollVerdict::Discarded(error) => {
                        wit_types::PollVerdict::Discarded(proto_to_wit_provider_error(error))
                    }
                    proto::PollVerdict::Undetermined(error) => {
                        wit_types::PollVerdict::Undetermined(proto_to_wit_provider_error(error))
                    }
                    proto::PollVerdict::InFlight(in_flight) => {
                        let proto::CreateInFlight { pending, error } = in_flight;
                        match sdk_to_wit_pending_create(pending, operation) {
                            Ok(pending) => {
                                wit_types::PollVerdict::InFlight(wit_types::CreateInFlight {
                                    pending,
                                    error: proto_to_wit_provider_error(error),
                                })
                            }
                            Err(failure) => failure.into_poll_verdict(previous_pending),
                        }
                    }
                }
            }

            fn sdk_to_wit_resume_step<T: serde::Serialize>(
                step: $crate::ProtocolResumeStep<T>,
                operation: &'static str,
            ) -> Result<wit_types::ResumeStep, ResumeStepEncodeFailure> {
                match step {
                    proto::ResumeStep::Completed {
                        identifier,
                        attributes,
                    } => match proto_to_wit_created_state(identifier.clone(), attributes) {
                        Ok(state) => Ok(wit_types::ResumeStep::Completed(state)),
                        Err(error) => {
                            let error =
                                boundary_encode_to_provider_error(operation, None, error);
                            Err(ResumeStepEncodeFailure::created(identifier, error))
                        }
                    },
                    proto::ResumeStep::Pending(pending) => {
                        sdk_to_wit_resume_pending(pending, operation)
                            .map(wit_types::ResumeStep::Pending)
                    }
                    proto::ResumeStep::Resent(step) => {
                        sdk_to_wit_protocol_create_step(step, operation)
                            .map(wit_types::ResumeStep::Resent)
                            .map_err(ResumeStepEncodeFailure::Resent)
                    }
                }
            }

            fn sdk_to_wit_resume_verdict<T: serde::Serialize>(
                verdict: $crate::ProtocolResumeVerdict<T>,
                operation: &'static str,
                previous_pending: wit_types::ResumePending,
            ) -> wit_types::ResumeVerdict {
                match verdict {
                    proto::ResumeVerdict::Untracked(error) => {
                        wit_types::ResumeVerdict::Untracked(proto_to_wit_provider_error(error))
                    }
                    proto::ResumeVerdict::Created(created) => wit_types::ResumeVerdict::Created(
                        sdk_to_wit_resumed_failure(created, operation),
                    ),
                    proto::ResumeVerdict::Discarded(error) => {
                        wit_types::ResumeVerdict::Discarded(proto_to_wit_provider_error(error))
                    }
                    proto::ResumeVerdict::Undetermined(error) => {
                        wit_types::ResumeVerdict::Undetermined(proto_to_wit_provider_error(error))
                    }
                    proto::ResumeVerdict::InFlight(in_flight) => {
                        let proto::ResumeInFlight { pending, error } = in_flight;
                        match sdk_to_wit_resume_pending(pending, operation) {
                            Ok(pending) => {
                                wit_types::ResumeVerdict::InFlight(wit_types::ResumeInFlight {
                                    pending,
                                    error: proto_to_wit_provider_error(error),
                                })
                            }
                            Err(failure) => failure.into_resume_verdict(previous_pending),
                        }
                    }
                    proto::ResumeVerdict::Resent(verdict) => {
                        match sdk_to_wit_create_verdict(verdict, operation) {
                            Ok(verdict) => wit_types::ResumeVerdict::Resent(verdict),
                            Err(failure) => {
                                failure.into_resent_resume_verdict(previous_pending)
                            }
                        }
                    }
                }
            }

            fn decode_create_identifier(
                operation: &'static str,
                identifier: Option<String>,
            ) -> Result<Option<proto::AppliedIdentifier>, wit_types::ProviderError> {
                identifier
                    .map(proto::AppliedIdentifier::new)
                    .transpose()
                    .map_err(|error| {
                        boundary_applied_identifier_decode_to_provider_error(operation, error)
                    })
            }

            fn wit_to_sdk_poll<T: serde::de::DeserializeOwned>(
                pending: wit_types::PendingCreate,
                request: &proto::CreateRequest,
            ) -> Result<(Option<proto::AppliedIdentifier>, proto::Sent, T), wit_types::ProviderError> {
                let wit_types::PendingCreate {
                    operation,
                    identifier,
                    sent,
                } = pending;
                let wit_types::PendingOperation {
                    token,
                    poll_after_secs: _poll_after_secs,
                } = operation;
                let sent = wit_to_proto_sent(sent);
                let request_attributes = request
                    .resource
                    .attributes
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>();
                if !sent.is_valid_for(&request_attributes) {
                    return Err(wit_types::ProviderError::Internal(wit_types::ErrorDetail {
                        message: "WASM boundary rejected pending create with sent attributes outside the request".to_string(),
                        resource_id: None,
                        cause: None,
                        provider_name: None,
                        operation: Some("poll_create".to_string()),
                        status: None,
                        code: None,
                        request_id: None,
                    }));
                }
                let token = serde_json::from_str(&token).map_err(|error| {
                    boundary_token_decode_to_provider_error("poll_create", error)
                })?;
                let identifier = decode_create_identifier("poll_create", identifier)?;
                Ok((identifier, sent, token))
            }

            fn wit_to_sdk_resume<T: serde::de::DeserializeOwned>(
                pending: wit_types::ResumePending,
            ) -> Result<(Option<proto::AppliedIdentifier>, T), wit_types::ProviderError> {
                let wit_types::ResumePending {
                    operation,
                    identifier,
                } = pending;
                let wit_types::PendingOperation {
                    token,
                    poll_after_secs: _poll_after_secs,
                } = operation;
                let token = serde_json::from_str(&token).map_err(|error| {
                    boundary_token_decode_to_provider_error("resume_create", error)
                })?;
                let identifier = decode_create_identifier("resume_create", identifier)?;
                Ok((identifier, token))
            }

            fn sdk_to_wit_update_outcome<T: serde::Serialize>(
                outcome: &$crate::UpdateOutcome<T>,
                operation: &'static str,
            ) -> Result<wit_types::UpdateOutcome, wit_types::ProviderError> {
                match outcome {
                    $crate::UpdateOutcome::Done(outcome) => {
                        proto_to_wit_update_outcome(outcome).map_err(|error| {
                            boundary_encode_to_provider_error(operation, None, error)
                        })
                    }
                    $crate::UpdateOutcome::Pending(pending) => sdk_to_wit_pending(pending)
                        .map(wit_types::UpdateOutcome::Pending)
                        .map_err(|error| {
                            boundary_token_encode_to_provider_error(operation, error)
                        }),
                }
            }

            fn sdk_to_wit_delete_outcome<T: serde::Serialize>(
                outcome: &$crate::DeleteOutcome<T>,
                operation: &'static str,
            ) -> Result<wit_types::DeleteOutcome, wit_types::ProviderError> {
                match outcome {
                    $crate::DeleteOutcome::Deleted => Ok(wit_types::DeleteOutcome::Deleted),
                    $crate::DeleteOutcome::Pending(pending) => sdk_to_wit_pending(pending)
                        .map(wit_types::DeleteOutcome::Pending)
                        .map_err(|error| {
                            boundary_token_encode_to_provider_error(operation, error)
                        }),
                }
            }

            fn provider_directives_for_resource(
                id: &wit_types::ResourceId,
                kind: $crate::core::schema::SchemaKind,
            ) -> Result<proto::Directives, &'static $crate::core::schema::ProviderSchemaValidationError>
            {
                let cache = get_provider_schema_cache()?;
                Ok(helpers::resolve_wit_resource_directives(
                    &id.resource_type,
                    kind,
                    cache.schemas(),
                ))
            }

            fn wit_to_proto_resource(
                res: &wit_types::ResourceDef,
                directives: proto::Directives,
            ) -> Result<proto::Resource, helpers::WasmValueDecodeError> {
                Ok(proto::Resource {
                    id: wit_to_proto_resource_id(&res.id),
                    attributes: wit_to_proto_value_map(&res.attributes)?,
                    directives,
                })
            }

            fn normalized_to_wit_resource(
                res: &$crate::NormalizedResource,
            ) -> Result<wit_types::ResourceDef, helpers::WasmValueEncodeError> {
                helpers::visit_normalized_resource(res, |id, attributes| {
                    let attributes = attributes
                        .map(|(key, value)| {
                            proto_to_wit_value(value)
                                .map(|value| (key.clone(), value))
                                .map_err(|error| error.prepend_attribute(key))
                        })
                        .collect::<Result<_, _>>()?;
                    Ok(wit_types::ResourceDef {
                        id: proto_to_wit_resource_id(id),
                        attributes,
                    })
                })
            }

            // -- Guest trait implementation --

            /// Trust-boundary encoder for a complete protocol error record.
            ///
            /// A core provider produces this exact record with
            /// `ProviderError::to_wire`; guest implementations may also build
            /// it directly. Its runtime message and flattened cause are
            /// untrusted data and are passed through unchanged for the host's
            /// sole sanctioned `ProviderError::from_wire` decode path.
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

            fn desired_resource_decode_to_provider_error(
                error: $crate::DesiredResourceDecodeError,
            ) -> wit_types::ProviderError {
                wit_types::ProviderError::Internal(wit_types::ErrorDetail {
                    message: format!(
                        "WASM boundary decode error in normalize_desired: {error}"
                    ),
                    resource_id: None,
                    cause: None,
                    provider_name: None,
                    operation: Some("normalize_desired".to_string()),
                    status: None,
                    code: None,
                    request_id: None,
                })
            }

            fn provider_schema_resolution_to_provider_error(
                operation: &'static str,
                error: &$crate::core::schema::ProviderSchemaValidationError,
            ) -> wit_types::ProviderError {
                wit_types::ProviderError::Internal(wit_types::ErrorDetail {
                    message: format!(
                        "WASM provider schema resolution failed in {operation}: {}",
                        helpers::render_error_chain(error),
                    ),
                    resource_id: None,
                    cause: None,
                    provider_name: None,
                    operation: Some(operation.to_string()),
                    status: None,
                    code: None,
                    request_id: None,
                })
            }

            fn boundary_applied_identifier_decode_to_provider_error(
                operation: &'static str,
                error: proto::EmptyAppliedIdentifier,
            ) -> wit_types::ProviderError {
                wit_types::ProviderError::InvalidInput(wit_types::ErrorDetail {
                    message: format!(
                        "WASM boundary decode error in {operation} identifier: {error}"
                    ),
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

            fn boundary_token_decode_to_provider_error(
                operation: &'static str,
                error: serde_json::Error,
            ) -> wit_types::ProviderError {
                wit_types::ProviderError::Internal(wit_types::ErrorDetail {
                    message: format!("WASM boundary decode error in {operation} pending token"),
                    resource_id: None,
                    cause: Some(format!(
                        "serde_json {:?} error at line {}, column {}",
                        error.classify(),
                        error.line(),
                        error.column()
                    )),
                    provider_name: None,
                    operation: Some(operation.to_string()),
                    status: None,
                    code: None,
                    request_id: None,
                })
            }

            fn boundary_token_encode_to_provider_error(
                operation: &'static str,
                error: serde_json::Error,
            ) -> wit_types::ProviderError {
                wit_types::ProviderError::Internal(wit_types::ErrorDetail {
                    message: format!("WASM boundary encode error in {operation} pending token"),
                    resource_id: None,
                    cause: Some(format!(
                        "serde_json {:?} error at line {}, column {}",
                        error.classify(),
                        error.line(),
                        error.column()
                    )),
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

            fn wit_to_proto_patch_op(
                op: wit_types::PatchOp,
            ) -> Result<proto::PatchOp, helpers::WasmValueDecodeError> {
                Ok(match op {
                    wit_types::PatchOp::Add(value) => proto::PatchOp::Add {
                        key: value.key,
                        value: wit_to_proto_value(&value.value)?,
                    },
                    wit_types::PatchOp::Replace(value) => proto::PatchOp::Replace {
                        key: value.key,
                        value: wit_to_proto_value(&value.value)?,
                    },
                    wit_types::PatchOp::Reassert(value) => proto::PatchOp::Reassert {
                        key: value.key,
                        value: wit_to_proto_value(&value.value)?,
                    },
                    wit_types::PatchOp::Remove(key) => proto::PatchOp::Remove { key },
                })
            }

            fn wit_to_proto_update_request(
                req: wit_types::UpdateRequest,
                proto_id: &proto::ResourceId,
            ) -> Result<proto::UpdateRequest, helpers::WasmValueDecodeError> {
                let from = wit_to_proto_persisted_state(proto_id, &req.current)?;
                let ops = req
                    .patch
                    .ops
                    .into_iter()
                    .map(wit_to_proto_patch_op)
                    .collect::<Result<Vec<_>, helpers::WasmValueDecodeError>>()?;
                Ok(proto::UpdateRequest {
                    from,
                    patch: proto::UpdatePatch { ops },
                })
            }

            fn wit_to_proto_create_request(
                req: wit_types::CreateRequest,
                directives: proto::Directives,
            ) -> Result<proto::CreateRequest, helpers::WasmValueDecodeError> {
                Ok(proto::CreateRequest {
                    resource: wit_to_proto_resource(&req.res, directives)?,
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
                    let cache = match get_provider_schema_cache() {
                        Ok(cache) => cache,
                        Err(error) => {
                            return schema_metadata_error_json(
                                "schema cache validation failed",
                                error,
                            );
                        }
                    };
                    match $crate::provider_schema_to_json(cache.witness()) {
                        Ok(json) => json,
                        Err(error) => {
                            schema_metadata_error_json("schema serialization failed", &error)
                        }
                    }
                }

                fn provider_config_attribute_types() -> String {
                    let cache = match get_provider_schema_cache() {
                        Ok(cache) => cache,
                        Err(error) => {
                            return schema_metadata_error_json(
                                "schema cache validation failed",
                                error,
                            );
                        }
                    };
                    match $crate::provider_config_schema_to_json(cache.config()) {
                        Ok(json) => json,
                        Err(error) => schema_metadata_error_json(
                            "provider config schema serialization failed",
                            &error,
                        ),
                    }
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
                ) -> Result<wit_types::InitializeResult, wit_types::ProviderError> {
                    let map = wit_to_proto_value_map(&attrs).map_err(|error| {
                        boundary_decode_to_provider_error("initialize", None, error)
                    })?;
                    let mut provider = get_provider().lock().unwrap();
                    $crate::CarinaProvider::initialize(&mut *provider, &map)
                        .map(|reference_scope| wit_types::InitializeResult {
                            reference_scope: proto_to_wit_reference_scope(reference_scope),
                        })
                        .map_err(validate_string_to_provider_error)
                }

                fn read(
                    id: wit_types::ResourceId,
                    identifier: String,
                    _request: wit_types::ReadRequest,
                ) -> Result<wit_types::ReadOutcome, wit_types::ProviderError> {
                    let identifier = helpers::wit_to_proto_applied_identifier(identifier).map_err(|error| {
                        boundary_applied_identifier_decode_to_provider_error("read", error)
                    })?;
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    match $crate::CarinaProvider::read(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto::ReadRequest,
                    ) {
                        Ok(outcome) => proto_to_wit_read_outcome(&outcome)
                            .map_err(|error| {
                                boundary_encode_to_provider_error("read", None, error)
                            }),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn read_data_source(
                    res: wit_types::ResourceDef,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let directives = provider_directives_for_resource(
                        &res.id,
                        $crate::core::schema::SchemaKind::DataSource,
                    )
                    .map_err(|error| {
                        provider_schema_resolution_to_provider_error(
                            "read_data_source",
                            error,
                        )
                    })?;
                    let proto_res = wit_to_proto_resource(&res, directives).map_err(|error| {
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
                ) -> Result<wit_types::CreateStep, wit_types::CreateVerdict> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let directives = match provider_directives_for_resource(
                        &request.res.id,
                        $crate::core::schema::SchemaKind::Resource,
                    ) {
                        Ok(directives) => directives,
                        Err(error) => {
                            return Err(wit_types::CreateVerdict::Untracked(
                                provider_schema_resolution_to_provider_error("create", error),
                            ));
                        }
                    };
                    let proto_request = match wit_to_proto_create_request(request, directives) {
                        Ok(request) => request,
                        Err(error) => {
                            return Err(wit_types::CreateVerdict::Untracked(
                                boundary_decode_to_provider_error("create", None, error),
                            ));
                        }
                    };
                    let provider = get_provider().lock().unwrap();
                    match $crate::dispatch_create(
                        &*provider,
                        &proto_id,
                        proto_request,
                    ) {
                        Ok(step) => sdk_to_wit_create_step(step, "create")
                            .map_err(CreateStepEncodeFailure::into_create_verdict),
                        Err(verdict) => Err(match sdk_to_wit_create_verdict(*verdict, "create") {
                            Ok(verdict) => verdict,
                            Err(failure) => failure.into_create_verdict(),
                        }),
                    }
                }

                fn poll_create(
                    id: wit_types::ResourceId,
                    request: wit_types::CreateRequest,
                    pending: wit_types::PendingCreate,
                ) -> Result<wit_types::CreateStep, wit_types::PollVerdict> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let previous_pending = pending.clone();
                    let directives = match provider_directives_for_resource(
                        &request.res.id,
                        $crate::core::schema::SchemaKind::Resource,
                    ) {
                        Ok(directives) => directives,
                        Err(error) => {
                            return Err(wit_types::PollVerdict::Undetermined(
                                provider_schema_resolution_to_provider_error(
                                    "poll_create",
                                    error,
                                ),
                            ));
                        }
                    };
                    let proto_request = match wit_to_proto_create_request(request, directives) {
                        Ok(request) => request,
                        Err(error) => {
                            return Err(wit_types::PollVerdict::Undetermined(
                                boundary_decode_to_provider_error("poll_create", None, error),
                            ));
                        }
                    };
                    let (identifier, sent, token) = match wit_to_sdk_poll::<
                        <$provider_type as $crate::CarinaProvider>::CreateToken,
                    >(pending, &proto_request) {
                        Ok(poll) => poll,
                        Err(error) => {
                            return Err(wit_types::PollVerdict::Undetermined(error));
                        }
                    };
                    let provider = get_provider().lock().unwrap();
                    match $crate::dispatch_poll_create(
                        &*provider,
                        &proto_id,
                        proto_request,
                        identifier,
                        sent,
                        token,
                    ) {
                        Ok(step) => sdk_to_wit_create_step(step, "poll_create")
                            .map_err(|failure| {
                                failure.into_poll_verdict(previous_pending)
                            }),
                        Err(verdict) => Err(sdk_to_wit_poll_verdict(
                            *verdict,
                            "poll_create",
                            previous_pending,
                        )),
                    }
                }

                fn resume_create(
                    id: wit_types::ResourceId,
                    request: wit_types::CreateRequest,
                    pending: wit_types::ResumePending,
                ) -> Result<wit_types::ResumeStep, wit_types::ResumeVerdict> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let previous_pending = pending.clone();
                    let directives = match provider_directives_for_resource(
                        &request.res.id,
                        $crate::core::schema::SchemaKind::Resource,
                    ) {
                        Ok(directives) => directives,
                        Err(error) => {
                            return Err(wit_types::ResumeVerdict::Undetermined(
                                provider_schema_resolution_to_provider_error(
                                    "resume_create",
                                    error,
                                ),
                            ));
                        }
                    };
                    let proto_request = match wit_to_proto_create_request(request, directives) {
                        Ok(request) => request,
                        Err(error) => {
                            return Err(wit_types::ResumeVerdict::Undetermined(
                                boundary_decode_to_provider_error("resume_create", None, error),
                            ));
                        }
                    };
                    let (identifier, token) = match wit_to_sdk_resume::<
                        <$provider_type as $crate::CarinaProvider>::CreateToken,
                    >(pending) {
                        Ok(resume) => resume,
                        Err(error) => {
                            return Err(wit_types::ResumeVerdict::Undetermined(error));
                        }
                    };
                    let provider = get_provider().lock().unwrap();
                    match $crate::dispatch_resume_create(
                        &*provider,
                        &proto_id,
                        proto_request,
                        identifier,
                        token,
                    ) {
                        Ok(step) => sdk_to_wit_resume_step(step, "resume_create")
                            .map_err(|failure| {
                                failure.into_resume_verdict(previous_pending)
                            }),
                        Err(verdict) => Err(sdk_to_wit_resume_verdict(
                            *verdict,
                            "resume_create",
                            previous_pending,
                        )),
                    }
                }

                fn update(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::UpdateRequest,
                ) -> Result<wit_types::UpdateOutcome, wit_types::ProviderError> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let identifier = helpers::wit_to_proto_applied_identifier(identifier).map_err(|error| {
                        boundary_applied_identifier_decode_to_provider_error("update", error)
                    })?;
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
                        Ok(outcome) => sdk_to_wit_update_outcome(&outcome, "update"),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn poll_update(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::UpdateRequest,
                    pending: wit_types::PendingOperation,
                ) -> Result<wit_types::UpdateOutcome, wit_types::ProviderError> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let identifier = helpers::wit_to_proto_applied_identifier(identifier).map_err(|error| {
                        boundary_applied_identifier_decode_to_provider_error("poll_update", error)
                    })?;
                    let proto_request = wit_to_proto_update_request(request, &proto_id)
                        .map_err(|error| {
                            boundary_decode_to_provider_error("poll_update", None, error)
                        })?;
                    let pending = wit_to_sdk_pending::<
                        <$provider_type as $crate::CarinaProvider>::UpdateToken,
                    >(pending)
                    .map_err(|error| {
                        boundary_token_decode_to_provider_error("poll_update", error)
                    })?;
                    let provider = get_provider().lock().unwrap();
                    match $crate::CarinaProvider::poll_update(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto_request,
                        pending,
                    ) {
                        Ok(outcome) => sdk_to_wit_update_outcome(&outcome, "poll_update"),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn delete(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::DeleteRequest,
                ) -> Result<wit_types::DeleteOutcome, wit_types::ProviderError> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let identifier = helpers::wit_to_proto_applied_identifier(identifier).map_err(|error| {
                        boundary_applied_identifier_decode_to_provider_error("delete", error)
                    })?;
                    let proto_request = wit_to_proto_delete_request(request);
                    let provider = get_provider().lock().unwrap();
                    match $crate::CarinaProvider::delete(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto_request,
                    ) {
                        Ok(outcome) => sdk_to_wit_delete_outcome(&outcome, "delete"),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn poll_delete(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::DeleteRequest,
                    pending: wit_types::PendingOperation,
                ) -> Result<wit_types::DeleteOutcome, wit_types::ProviderError> {
                    let proto_id = wit_to_proto_resource_id(&id);
                    let identifier = helpers::wit_to_proto_applied_identifier(identifier).map_err(|error| {
                        boundary_applied_identifier_decode_to_provider_error("poll_delete", error)
                    })?;
                    let proto_request = wit_to_proto_delete_request(request);
                    let pending = wit_to_sdk_pending::<
                        <$provider_type as $crate::CarinaProvider>::DeleteToken,
                    >(pending)
                    .map_err(|error| {
                        boundary_token_decode_to_provider_error("poll_delete", error)
                    })?;
                    let provider = get_provider().lock().unwrap();
                    match $crate::CarinaProvider::poll_delete(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto_request,
                        pending,
                    ) {
                        Ok(outcome) => sdk_to_wit_delete_outcome(&outcome, "poll_delete"),
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
                    let cache = get_provider_schema_cache()
                        .map_err(|error| {
                            validate_string_to_provider_error(helpers::render_error_chain(error))
                        })?;
                    $crate::dispatch_custom_type_validation(
                        cache,
                        &wit_to_proto_type_identity(&ty),
                        &value,
                    )
                    .map_err(validate_string_to_provider_error)
                }

                fn normalize_desired(
                    resources: Vec<exports::carina::provider::provider::DesiredResource>,
                ) -> Result<Vec<wit_types::ResourceDef>, wit_types::ProviderError> {
                    let proto_resources = resources
                        .into_iter()
                        .map(|desired| {
                            let exports::carina::provider::provider::DesiredResource {
                                resource,
                                deferred,
                            } = desired;
                            let directives = provider_directives_for_resource(
                                &resource.id,
                                $crate::core::schema::SchemaKind::Resource,
                            )
                            .map_err(|error| {
                                provider_schema_resolution_to_provider_error(
                                    "normalize_desired",
                                    error,
                                )
                            })?;
                            let resource = wit_to_proto_resource(&resource, directives).map_err(|error| {
                                boundary_decode_to_provider_error(
                                    "normalize_desired",
                                    None,
                                    error,
                                )
                            })?;
                            helpers::decode_desired_resource(resource, deferred)
                                .map_err(desired_resource_decode_to_provider_error)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
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
                        .map(normalized_to_wit_resource)
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error("normalize_desired", None, error)
                        })
                }

                fn normalize_state(
                    states: Vec<(String, wit_types::PersistedState)>,
                ) -> Result<Vec<(String, wit_types::PersistedState)>, wit_types::ProviderError> {
                    let proto_states = states
                        .iter()
                        .map(|(key, state)| {
                            let parsed_id = helpers::parse_resource_id_string(key);
                            wit_to_proto_persisted_state(&parsed_id, state)
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
                            proto_to_wit_persisted_state(&state).map(|state| (key, state))
                        })
                        .collect::<Result<_, _>>()
                        .map_err(|error| {
                            boundary_encode_to_provider_error("normalize_state", None, error)
                        })
                }

                fn hydrate_read_state(
                    states: Vec<(String, wit_types::PersistedState)>,
                    saved_attrs: Vec<(String, Vec<(String, wit_types::PersistedValue)>)>,
                ) -> Result<Vec<(String, wit_types::PersistedState)>, wit_types::ProviderError> {
                    let mut proto_states = states
                        .iter()
                        .map(|(key, state)| {
                            let parsed_id = helpers::parse_resource_id_string(key);
                            wit_to_proto_persisted_state(&parsed_id, state)
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
                            wit_to_proto_persisted_value_map(attributes)
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
                            proto_to_wit_persisted_state(&state).map(|state| (key, state))
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

            }

            export!(WasmGuest);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_metadata_error_chain_keeps_the_typed_conversion_detail_once() {
        let identity = carina_core::schema::TypeIdentity::new(Some("test"), ["widget"], "Mode");
        let error = crate::SchemaSerializationError::Convert(
            carina_core::protocol_convert::ProtocolConvertError::OpenEnumWithoutNamespace {
                identity,
            },
        );

        let rendered = render_error_chain(&error);

        assert!(
            rendered.contains("schema protocol conversion failed"),
            "{rendered}"
        );
        assert!(rendered.contains("has no namespace"), "{rendered}");
        assert_eq!(
            rendered.matches("test.widget.Mode").count(),
            1,
            "{rendered}"
        );
    }

    #[test]
    fn mutation_identifier_decode_rejects_empty_wit_string() {
        assert!(matches!(
            wit_to_proto_applied_identifier(String::new()),
            Err(proto::EmptyAppliedIdentifier)
        ));
    }

    #[test]
    fn wit_resource_protocol_directives_apply_the_schema_default() {
        let schemas = vec![
            carina_core::schema::ResourceSchema::new("test.Protected")
                .default_prevent_destroy()
                .validate()
                .expect("schema must validate"),
        ];

        let directives = resolve_wit_resource_directives(
            "test.Protected",
            carina_core::schema::SchemaKind::Resource,
            &schemas,
        );

        assert!(directives.prevent_destroy);
    }

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
            let rendered = error.to_string();
            assert!(rendered.contains(variant), "{rendered}");
            assert_eq!(rendered.matches("invalid JSON").count(), 1, "{rendered}");
            assert!(rendered.contains("line 1 column"), "{rendered}");
            assert!(std::error::Error::source(&error).is_none());
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
        let rendered = error.to_string();
        assert_eq!(rendered.matches("invalid JSON").count(), 1, "{rendered}");
        assert!(rendered.contains("line 1 column"), "{rendered}");
        assert!(std::error::Error::source(&error).is_none());
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

    #[test]
    fn persisted_json_policy_is_structural_and_restores_escaped_user_keys() {
        let digest = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0MTIzNA$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let marker = format!("_secret:{digest}");
        let json = serde_json::json!([
            marker,
            {"$secret": digest},
            {"$$secret": "user"}
        ]);
        let decoded = decode_persisted_list_val(&json.to_string()).expect("decode persisted list");

        assert_eq!(
            decoded,
            proto::PersistedValue::List(vec![
                proto::PersistedValue::String(marker),
                proto::PersistedValue::RedactedSecret(digest.to_string()),
                proto::PersistedValue::Map(std::collections::BTreeMap::from([(
                    "$secret".to_string(),
                    proto::PersistedValue::String("user".to_string()),
                )])),
            ])
        );
    }

    #[test]
    fn persisted_json_encoding_tags_only_typed_redacted_secrets() {
        let digest = "argon2-legacy:73616c74:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let marker = format!("_secret:{digest}");
        let value = proto::PersistedValue::Map(std::collections::BTreeMap::from([
            (
                "plain".to_string(),
                proto::PersistedValue::String(marker.clone()),
            ),
            (
                "typed".to_string(),
                proto::PersistedValue::RedactedSecret(digest.to_string()),
            ),
            (
                "$secret".to_string(),
                proto::PersistedValue::String("user".to_string()),
            ),
        ]));
        let encoded = proto_persisted_value_to_json(&value).expect("encode persisted value");

        assert_eq!(encoded["plain"], marker);
        assert_eq!(encoded["typed"], serde_json::json!({"$secret": digest}));
        assert_eq!(encoded["$$secret"], "user");
        let ordinary = proto::Value::Map(std::collections::BTreeMap::from([(
            "$secret".to_string(),
            proto::Value::String(digest.to_string()),
        )]));
        assert_eq!(
            proto_value_to_json(&ordinary).expect("encode ordinary map"),
            serde_json::json!({"$secret": digest}),
            "the ordinary type treats a $secret key as user data",
        );
    }

    #[test]
    fn malformed_persisted_json_tags_and_reserved_keys_are_rejected() {
        for json in [
            serde_json::json!([{"$secret": 42}]),
            serde_json::json!([{"$secret": "digest", "extra": true}]),
            serde_json::json!([{"$reserved": "value"}]),
        ] {
            assert!(decode_persisted_list_val(&json.to_string()).is_err());
        }
    }
}
