//! WASM guest-side helpers for the carina-provider WIT interface.
//!
//! Provides type conversion functions between WIT guest types and protocol types.
//! The `export_provider!` macro generates the wit-bindgen bindings and Guest trait
//! implementation in the consumer crate.

use carina_provider_protocol::types as proto;

// -- JSON conversion helpers --

pub fn json_to_proto_value(v: serde_json::Value) -> proto::Value {
    match v {
        serde_json::Value::Bool(b) => proto::Value::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                proto::Value::Int(i)
            } else {
                proto::Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => proto::Value::String(s),
        serde_json::Value::Array(a) => {
            proto::Value::List(a.into_iter().map(json_to_proto_value).collect())
        }
        serde_json::Value::Object(m) => proto::Value::Map(
            m.into_iter()
                .map(|(k, v)| (k, json_to_proto_value(v)))
                .collect(),
        ),
        serde_json::Value::Null => proto::Value::String(String::new()),
    }
}

pub fn proto_value_to_json(v: &proto::Value) -> serde_json::Value {
    match v {
        proto::Value::Bool(b) => serde_json::Value::Bool(*b),
        proto::Value::Int(i) => serde_json::Value::Number((*i).into()),
        proto::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        proto::Value::String(s) => serde_json::Value::String(s.clone()),
        proto::Value::List(items) => {
            serde_json::Value::Array(items.iter().map(proto_value_to_json).collect())
        }
        proto::Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), proto_value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
    }
}

/// JSON-serializable representation for encoding recursive attribute types.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum AttrTypeJson {
    #[serde(rename = "string")]
    String,
    #[serde(rename = "int")]
    Int,
    #[serde(rename = "float")]
    Float,
    #[serde(rename = "bool")]
    Bool,
    #[serde(rename = "string-enum")]
    StringEnum { values: Vec<std::string::String> },
    #[serde(rename = "list")]
    List {
        inner: Box<AttrTypeJson>,
        ordered: bool,
    },
    #[serde(rename = "map")]
    Map { inner: Box<AttrTypeJson> },
    #[serde(rename = "struct")]
    Struct {
        name: std::string::String,
        fields: Vec<StructFieldJson>,
    },
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct StructFieldJson {
    pub name: std::string::String,
    pub field_type: AttrTypeJson,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<std::string::String>,
}

pub fn proto_attr_type_to_json(t: &proto::AttributeType) -> AttrTypeJson {
    match t {
        proto::AttributeType::String => AttrTypeJson::String,
        proto::AttributeType::Int => AttrTypeJson::Int,
        proto::AttributeType::Float => AttrTypeJson::Float,
        proto::AttributeType::Bool => AttrTypeJson::Bool,
        proto::AttributeType::StringEnum { values } => AttrTypeJson::StringEnum {
            values: values.clone(),
        },
        proto::AttributeType::List { inner, ordered } => AttrTypeJson::List {
            inner: Box::new(proto_attr_type_to_json(inner)),
            ordered: *ordered,
        },
        proto::AttributeType::Map { inner } => AttrTypeJson::Map {
            inner: Box::new(proto_attr_type_to_json(inner)),
        },
        proto::AttributeType::Struct { name, fields } => AttrTypeJson::Struct {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|f| StructFieldJson {
                    name: f.name.clone(),
                    field_type: proto_attr_type_to_json(&f.field_type),
                    required: f.required,
                    description: f.description.clone(),
                })
                .collect(),
        },
        proto::AttributeType::Union { .. } => AttrTypeJson::String,
    }
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
                    "wasi:io/poll@0.2.0": ::wasi::io::poll,
                    "wasi:io/error@0.2.0": ::wasi::io::error,
                    "wasi:io/streams@0.2.0": ::wasi::io::streams,
                    "wasi:clocks/monotonic-clock@0.2.0": ::wasi::clocks::monotonic_clock,
                    "wasi:http/types@0.2.0": ::wasi::http::types,
                    "wasi:http/outgoing-handler@0.2.0": ::wasi::http::outgoing_handler,
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
                    name: id.name.clone(),
                }
            }

            fn proto_to_wit_resource_id(id: &proto::ResourceId) -> wit_types::ResourceId {
                wit_types::ResourceId {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                    name: id.name.clone(),
                }
            }

            fn wit_to_proto_value(v: &wit_types::Value) -> proto::Value {
                match v {
                    wit_types::Value::BoolVal(b) => proto::Value::Bool(*b),
                    wit_types::Value::IntVal(i) => proto::Value::Int(*i),
                    wit_types::Value::FloatVal(f) => proto::Value::Float(*f),
                    wit_types::Value::StrVal(s) => proto::Value::String(s.clone()),
                    wit_types::Value::ListVal(json) => {
                        let items: Vec<serde_json::Value> =
                            serde_json::from_str(json).unwrap_or_default();
                        proto::Value::List(
                            items.into_iter().map(helpers::json_to_proto_value).collect(),
                        )
                    }
                    wit_types::Value::MapVal(json) => {
                        let map: serde_json::Map<String, serde_json::Value> =
                            serde_json::from_str(json).unwrap_or_default();
                        proto::Value::Map(
                            map.into_iter()
                                .map(|(k, v)| (k, helpers::json_to_proto_value(v)))
                                .collect(),
                        )
                    }
                }
            }

            fn proto_to_wit_value(v: &proto::Value) -> wit_types::Value {
                match v {
                    proto::Value::Bool(b) => wit_types::Value::BoolVal(*b),
                    proto::Value::Int(i) => wit_types::Value::IntVal(*i),
                    proto::Value::Float(f) => wit_types::Value::FloatVal(*f),
                    proto::Value::String(s) => wit_types::Value::StrVal(s.clone()),
                    proto::Value::List(items) => {
                        let json_items: Vec<serde_json::Value> =
                            items.iter().map(helpers::proto_value_to_json).collect();
                        wit_types::Value::ListVal(serde_json::to_string(&json_items).unwrap())
                    }
                    proto::Value::Map(map) => {
                        let json_map: serde_json::Map<String, serde_json::Value> = map
                            .iter()
                            .map(|(k, v)| (k.clone(), helpers::proto_value_to_json(v)))
                            .collect();
                        wit_types::Value::MapVal(serde_json::to_string(&json_map).unwrap())
                    }
                }
            }

            fn wit_to_proto_value_map(
                entries: &[(String, wit_types::Value)],
            ) -> HashMap<String, proto::Value> {
                entries
                    .iter()
                    .map(|(k, v): &(String, wit_types::Value)| (k.clone(), wit_to_proto_value(v)))
                    .collect()
            }

            fn proto_to_wit_value_map(
                map: &HashMap<String, proto::Value>,
            ) -> Vec<(String, wit_types::Value)> {
                map.iter()
                    .map(|(k, v)| (k.clone(), proto_to_wit_value(v)))
                    .collect()
            }

            fn wit_to_proto_state(
                id: &proto::ResourceId,
                state: &wit_types::State,
            ) -> proto::State {
                proto::State {
                    id: id.clone(),
                    identifier: state.identifier.clone(),
                    attributes: wit_to_proto_value_map(&state.attributes),
                    exists: true,
                }
            }

            fn proto_to_wit_state(state: &proto::State) -> wit_types::State {
                wit_types::State {
                    identifier: state.identifier.clone(),
                    attributes: proto_to_wit_value_map(&state.attributes),
                }
            }

            fn wit_to_proto_resource(res: &wit_types::ResourceDef) -> proto::Resource {
                proto::Resource {
                    id: wit_to_proto_resource_id(&res.id),
                    attributes: wit_to_proto_value_map(&res.attributes),
                    lifecycle: proto::LifecycleConfig::default(),
                }
            }

            fn proto_to_wit_resource(res: &proto::Resource) -> wit_types::ResourceDef {
                wit_types::ResourceDef {
                    id: proto_to_wit_resource_id(&res.id),
                    attributes: proto_to_wit_value_map(&res.attributes),
                }
            }

            fn wit_to_proto_lifecycle(
                _lc: &wit_types::LifecycleConfig,
            ) -> proto::LifecycleConfig {
                proto::LifecycleConfig::default()
            }

            fn proto_to_wit_provider_error(
                e: &proto::ProviderError,
            ) -> wit_types::ProviderError {
                wit_types::ProviderError {
                    message: e.message.clone(),
                    resource_id: e.resource_id.as_ref().map(proto_to_wit_resource_id),
                    is_timeout: e.is_timeout,
                }
            }

            fn proto_to_wit_resource_schema(
                s: &proto::ResourceSchema,
            ) -> wit_types::ResourceSchema {
                wit_types::ResourceSchema {
                    resource_type: s.resource_type.clone(),
                    attributes: s
                        .attributes
                        .values()
                        .map(proto_to_wit_attribute_schema)
                        .collect(),
                    description: s.description.clone(),
                    data_source: s.data_source,
                    name_attribute: s.name_attribute.clone(),
                    force_replace: s.force_replace,
                }
            }

            fn proto_to_wit_attribute_schema(
                a: &proto::AttributeSchema,
            ) -> wit_types::AttributeSchema {
                wit_types::AttributeSchema {
                    name: a.name.clone(),
                    attr_type: proto_to_wit_attribute_type(&a.attr_type),
                    required: a.required,
                    description: a.description.clone(),
                    create_only: a.create_only,
                    read_only: a.read_only,
                    write_only: a.write_only,
                }
            }

            fn proto_to_wit_attribute_type(
                t: &proto::AttributeType,
            ) -> wit_types::AttributeType {
                match t {
                    proto::AttributeType::String => wit_types::AttributeType::StringType,
                    proto::AttributeType::Int => wit_types::AttributeType::IntType,
                    proto::AttributeType::Float => wit_types::AttributeType::FloatType,
                    proto::AttributeType::Bool => wit_types::AttributeType::BoolType,
                    proto::AttributeType::StringEnum { values } => {
                        wit_types::AttributeType::StringEnum(values.clone())
                    }
                    proto::AttributeType::List { inner, ordered } => {
                        let json = helpers::AttrTypeJson::List {
                            inner: Box::new(helpers::proto_attr_type_to_json(inner)),
                            ordered: *ordered,
                        };
                        wit_types::AttributeType::ListType(
                            serde_json::to_string(&json).unwrap(),
                        )
                    }
                    proto::AttributeType::Map { inner } => {
                        let json = helpers::AttrTypeJson::Map {
                            inner: Box::new(helpers::proto_attr_type_to_json(inner)),
                        };
                        wit_types::AttributeType::MapType(
                            serde_json::to_string(&json).unwrap(),
                        )
                    }
                    proto::AttributeType::Struct { name, fields } => {
                        let fields_json: Vec<helpers::StructFieldJson> = fields
                            .iter()
                            .map(|f| helpers::StructFieldJson {
                                name: f.name.clone(),
                                field_type: helpers::proto_attr_type_to_json(&f.field_type),
                                required: f.required,
                                description: f.description.clone(),
                            })
                            .collect();
                        wit_types::AttributeType::StructType(wit_types::StructDef {
                            name: name.clone(),
                            fields: serde_json::to_string(&fields_json).unwrap(),
                        })
                    }
                    proto::AttributeType::Union { .. } => {
                        wit_types::AttributeType::StringType
                    }
                }
            }

            struct WasmGuest;

            impl exports::carina::provider::provider::Guest for WasmGuest {
                fn info() -> wit_types::ProviderInfo {
                    let provider = get_provider().lock().unwrap();
                    let info = $crate::CarinaProvider::info(&*provider);
                    wit_types::ProviderInfo {
                        name: info.name,
                        display_name: info.display_name,
                    }
                }

                fn schemas() -> Vec<wit_types::ResourceSchema> {
                    let provider = get_provider().lock().unwrap();
                    let schemas = $crate::CarinaProvider::schemas(&*provider);
                    schemas.iter().map(proto_to_wit_resource_schema).collect()
                }

                fn validate_config(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), String> {
                    let provider = get_provider().lock().unwrap();
                    let map = wit_to_proto_value_map(&attrs);
                    $crate::CarinaProvider::validate_config(&*provider, &map)
                }

                fn initialize(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), String> {
                    let mut provider = get_provider().lock().unwrap();
                    let map = wit_to_proto_value_map(&attrs);
                    $crate::CarinaProvider::initialize(&mut *provider, &map)
                }

                fn read(
                    id: wit_types::ResourceId,
                    identifier: Option<String>,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    match $crate::CarinaProvider::read(
                        &*provider,
                        &proto_id,
                        identifier.as_deref(),
                    ) {
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(&e)),
                    }
                }

                fn create(
                    res: wit_types::ResourceDef,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_res = wit_to_proto_resource(&res);
                    match $crate::CarinaProvider::create(&*provider, &proto_res) {
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(&e)),
                    }
                }

                fn update(
                    id: wit_types::ResourceId,
                    identifier: String,
                    current: wit_types::State,
                    to: wit_types::ResourceDef,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_from = wit_to_proto_state(&proto_id, &current);
                    let proto_to = wit_to_proto_resource(&to);
                    match $crate::CarinaProvider::update(
                        &*provider,
                        &proto_id,
                        &identifier,
                        &proto_from,
                        &proto_to,
                    ) {
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(&e)),
                    }
                }

                fn delete(
                    id: wit_types::ResourceId,
                    identifier: String,
                    lifecycle: wit_types::LifecycleConfig,
                ) -> Result<(), wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_lc = wit_to_proto_lifecycle(&lifecycle);
                    match $crate::CarinaProvider::delete(
                        &*provider,
                        &proto_id,
                        &identifier,
                        &proto_lc,
                    ) {
                        Ok(()) => Ok(()),
                        Err(e) => Err(proto_to_wit_provider_error(&e)),
                    }
                }

                fn normalize_desired(
                    resources: Vec<wit_types::ResourceDef>,
                ) -> Vec<wit_types::ResourceDef> {
                    let provider = get_provider().lock().unwrap();
                    let proto_resources: Vec<_> =
                        resources.iter().map(wit_to_proto_resource).collect();
                    let result =
                        $crate::CarinaProvider::normalize_desired(&*provider, proto_resources);
                    result.iter().map(proto_to_wit_resource).collect()
                }

                fn normalize_state(
                    states: Vec<(String, wit_types::State)>,
                ) -> Vec<(String, wit_types::State)> {
                    let provider = get_provider().lock().unwrap();
                    let proto_states: HashMap<_, _> = states
                        .iter()
                        .map(|(k, s)| {
                            let dummy_id = proto::ResourceId {
                                provider: String::new(),
                                resource_type: String::new(),
                                name: k.clone(),
                            };
                            (k.clone(), wit_to_proto_state(&dummy_id, s))
                        })
                        .collect();
                    let result =
                        $crate::CarinaProvider::normalize_state(&*provider, proto_states);
                    result
                        .into_iter()
                        .map(|(k, s)| (k, proto_to_wit_state(&s)))
                        .collect()
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
                    name: id.name.clone(),
                }
            }

            fn proto_to_wit_resource_id(id: &proto::ResourceId) -> wit_types::ResourceId {
                wit_types::ResourceId {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                    name: id.name.clone(),
                }
            }

            fn wit_to_proto_value(v: &wit_types::Value) -> proto::Value {
                match v {
                    wit_types::Value::BoolVal(b) => proto::Value::Bool(*b),
                    wit_types::Value::IntVal(i) => proto::Value::Int(*i),
                    wit_types::Value::FloatVal(f) => proto::Value::Float(*f),
                    wit_types::Value::StrVal(s) => proto::Value::String(s.clone()),
                    wit_types::Value::ListVal(json) => {
                        let items: Vec<serde_json::Value> =
                            serde_json::from_str(json).unwrap_or_default();
                        proto::Value::List(
                            items.into_iter().map(helpers::json_to_proto_value).collect(),
                        )
                    }
                    wit_types::Value::MapVal(json) => {
                        let map: serde_json::Map<String, serde_json::Value> =
                            serde_json::from_str(json).unwrap_or_default();
                        proto::Value::Map(
                            map.into_iter()
                                .map(|(k, v)| (k, helpers::json_to_proto_value(v)))
                                .collect(),
                        )
                    }
                }
            }

            fn proto_to_wit_value(v: &proto::Value) -> wit_types::Value {
                match v {
                    proto::Value::Bool(b) => wit_types::Value::BoolVal(*b),
                    proto::Value::Int(i) => wit_types::Value::IntVal(*i),
                    proto::Value::Float(f) => wit_types::Value::FloatVal(*f),
                    proto::Value::String(s) => wit_types::Value::StrVal(s.clone()),
                    proto::Value::List(items) => {
                        let json_items: Vec<serde_json::Value> =
                            items.iter().map(helpers::proto_value_to_json).collect();
                        wit_types::Value::ListVal(serde_json::to_string(&json_items).unwrap())
                    }
                    proto::Value::Map(map) => {
                        let json_map: serde_json::Map<String, serde_json::Value> = map
                            .iter()
                            .map(|(k, v)| (k.clone(), helpers::proto_value_to_json(v)))
                            .collect();
                        wit_types::Value::MapVal(serde_json::to_string(&json_map).unwrap())
                    }
                }
            }

            fn wit_to_proto_value_map(
                entries: &[(String, wit_types::Value)],
            ) -> HashMap<String, proto::Value> {
                entries
                    .iter()
                    .map(|(k, v): &(String, wit_types::Value)| (k.clone(), wit_to_proto_value(v)))
                    .collect()
            }

            fn proto_to_wit_value_map(
                map: &HashMap<String, proto::Value>,
            ) -> Vec<(String, wit_types::Value)> {
                map.iter()
                    .map(|(k, v)| (k.clone(), proto_to_wit_value(v)))
                    .collect()
            }

            fn wit_to_proto_state(
                id: &proto::ResourceId,
                state: &wit_types::State,
            ) -> proto::State {
                proto::State {
                    id: id.clone(),
                    identifier: state.identifier.clone(),
                    attributes: wit_to_proto_value_map(&state.attributes),
                    exists: true,
                }
            }

            fn proto_to_wit_state(state: &proto::State) -> wit_types::State {
                wit_types::State {
                    identifier: state.identifier.clone(),
                    attributes: proto_to_wit_value_map(&state.attributes),
                }
            }

            fn wit_to_proto_resource(res: &wit_types::ResourceDef) -> proto::Resource {
                proto::Resource {
                    id: wit_to_proto_resource_id(&res.id),
                    attributes: wit_to_proto_value_map(&res.attributes),
                    lifecycle: proto::LifecycleConfig::default(),
                }
            }

            fn proto_to_wit_resource(res: &proto::Resource) -> wit_types::ResourceDef {
                wit_types::ResourceDef {
                    id: proto_to_wit_resource_id(&res.id),
                    attributes: proto_to_wit_value_map(&res.attributes),
                }
            }

            fn wit_to_proto_lifecycle(
                _lc: &wit_types::LifecycleConfig,
            ) -> proto::LifecycleConfig {
                proto::LifecycleConfig::default()
            }

            fn proto_to_wit_provider_error(
                e: &proto::ProviderError,
            ) -> wit_types::ProviderError {
                wit_types::ProviderError {
                    message: e.message.clone(),
                    resource_id: e.resource_id.as_ref().map(proto_to_wit_resource_id),
                    is_timeout: e.is_timeout,
                }
            }

            fn proto_to_wit_resource_schema(
                s: &proto::ResourceSchema,
            ) -> wit_types::ResourceSchema {
                wit_types::ResourceSchema {
                    resource_type: s.resource_type.clone(),
                    attributes: s
                        .attributes
                        .values()
                        .map(proto_to_wit_attribute_schema)
                        .collect(),
                    description: s.description.clone(),
                    data_source: s.data_source,
                    name_attribute: s.name_attribute.clone(),
                    force_replace: s.force_replace,
                }
            }

            fn proto_to_wit_attribute_schema(
                a: &proto::AttributeSchema,
            ) -> wit_types::AttributeSchema {
                wit_types::AttributeSchema {
                    name: a.name.clone(),
                    attr_type: proto_to_wit_attribute_type(&a.attr_type),
                    required: a.required,
                    description: a.description.clone(),
                    create_only: a.create_only,
                    read_only: a.read_only,
                    write_only: a.write_only,
                }
            }

            fn proto_to_wit_attribute_type(
                t: &proto::AttributeType,
            ) -> wit_types::AttributeType {
                match t {
                    proto::AttributeType::String => wit_types::AttributeType::StringType,
                    proto::AttributeType::Int => wit_types::AttributeType::IntType,
                    proto::AttributeType::Float => wit_types::AttributeType::FloatType,
                    proto::AttributeType::Bool => wit_types::AttributeType::BoolType,
                    proto::AttributeType::StringEnum { values } => {
                        wit_types::AttributeType::StringEnum(values.clone())
                    }
                    proto::AttributeType::List { inner, ordered } => {
                        let json = helpers::AttrTypeJson::List {
                            inner: Box::new(helpers::proto_attr_type_to_json(inner)),
                            ordered: *ordered,
                        };
                        wit_types::AttributeType::ListType(
                            serde_json::to_string(&json).unwrap(),
                        )
                    }
                    proto::AttributeType::Map { inner } => {
                        let json = helpers::AttrTypeJson::Map {
                            inner: Box::new(helpers::proto_attr_type_to_json(inner)),
                        };
                        wit_types::AttributeType::MapType(
                            serde_json::to_string(&json).unwrap(),
                        )
                    }
                    proto::AttributeType::Struct { name, fields } => {
                        let fields_json: Vec<helpers::StructFieldJson> = fields
                            .iter()
                            .map(|f| helpers::StructFieldJson {
                                name: f.name.clone(),
                                field_type: helpers::proto_attr_type_to_json(&f.field_type),
                                required: f.required,
                                description: f.description.clone(),
                            })
                            .collect();
                        wit_types::AttributeType::StructType(wit_types::StructDef {
                            name: name.clone(),
                            fields: serde_json::to_string(&fields_json).unwrap(),
                        })
                    }
                    proto::AttributeType::Union { .. } => {
                        wit_types::AttributeType::StringType
                    }
                }
            }

            // -- Guest trait implementation --

            struct WasmGuest;

            impl exports::carina::provider::provider::Guest for WasmGuest {
                fn info() -> wit_types::ProviderInfo {
                    let provider = get_provider().lock().unwrap();
                    let info = $crate::CarinaProvider::info(&*provider);
                    wit_types::ProviderInfo {
                        name: info.name,
                        display_name: info.display_name,
                    }
                }

                fn schemas() -> Vec<wit_types::ResourceSchema> {
                    let provider = get_provider().lock().unwrap();
                    let schemas = $crate::CarinaProvider::schemas(&*provider);
                    schemas.iter().map(proto_to_wit_resource_schema).collect()
                }

                fn validate_config(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), String> {
                    let provider = get_provider().lock().unwrap();
                    let map = wit_to_proto_value_map(&attrs);
                    $crate::CarinaProvider::validate_config(&*provider, &map)
                }

                fn initialize(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), String> {
                    let mut provider = get_provider().lock().unwrap();
                    let map = wit_to_proto_value_map(&attrs);
                    $crate::CarinaProvider::initialize(&mut *provider, &map)
                }

                fn read(
                    id: wit_types::ResourceId,
                    identifier: Option<String>,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    match $crate::CarinaProvider::read(
                        &*provider,
                        &proto_id,
                        identifier.as_deref(),
                    ) {
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(&e)),
                    }
                }

                fn create(
                    res: wit_types::ResourceDef,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_res = wit_to_proto_resource(&res);
                    match $crate::CarinaProvider::create(&*provider, &proto_res) {
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(&e)),
                    }
                }

                fn update(
                    id: wit_types::ResourceId,
                    identifier: String,
                    current: wit_types::State,
                    to: wit_types::ResourceDef,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_from = wit_to_proto_state(&proto_id, &current);
                    let proto_to = wit_to_proto_resource(&to);
                    match $crate::CarinaProvider::update(
                        &*provider,
                        &proto_id,
                        &identifier,
                        &proto_from,
                        &proto_to,
                    ) {
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(&e)),
                    }
                }

                fn delete(
                    id: wit_types::ResourceId,
                    identifier: String,
                    lifecycle: wit_types::LifecycleConfig,
                ) -> Result<(), wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_lc = wit_to_proto_lifecycle(&lifecycle);
                    match $crate::CarinaProvider::delete(
                        &*provider,
                        &proto_id,
                        &identifier,
                        &proto_lc,
                    ) {
                        Ok(()) => Ok(()),
                        Err(e) => Err(proto_to_wit_provider_error(&e)),
                    }
                }

                fn normalize_desired(
                    resources: Vec<wit_types::ResourceDef>,
                ) -> Vec<wit_types::ResourceDef> {
                    let provider = get_provider().lock().unwrap();
                    let proto_resources: Vec<_> =
                        resources.iter().map(wit_to_proto_resource).collect();
                    let result =
                        $crate::CarinaProvider::normalize_desired(&*provider, proto_resources);
                    result.iter().map(proto_to_wit_resource).collect()
                }

                fn normalize_state(
                    states: Vec<(String, wit_types::State)>,
                ) -> Vec<(String, wit_types::State)> {
                    let provider = get_provider().lock().unwrap();
                    let proto_states: HashMap<_, _> = states
                        .iter()
                        .map(|(k, s)| {
                            let dummy_id = proto::ResourceId {
                                provider: String::new(),
                                resource_type: String::new(),
                                name: k.clone(),
                            };
                            (k.clone(), wit_to_proto_state(&dummy_id, s))
                        })
                        .collect();
                    let result =
                        $crate::CarinaProvider::normalize_state(&*provider, proto_states);
                    result
                        .into_iter()
                        .map(|(k, s)| (k, proto_to_wit_state(&s)))
                        .collect()
                }
            }

            export!(WasmGuest);
        }
    };
}
