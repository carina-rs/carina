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

/// Parse a ResourceId string (provider.resource_type.name) into a proto::ResourceId.
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
                    name: id.name.clone(),
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
                    // The host marks an attribute as a secret with this
                    // variant (carina#2390). `proto::Value` does not yet
                    // carry a `Secret` arm, so we decode the JSON-encoded
                    // inner value here and surface it to the provider as
                    // an opaque value — providers MUST NOT log or persist
                    // values that arrived in attributes the host marked
                    // as secret. Adding `proto::Value::Secret` to preserve
                    // the signal end-to-end is tracked separately.
                    wit_types::Value::SecretVal(json) => {
                        let inner: serde_json::Value =
                            serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
                        helpers::json_to_proto_value(inner)
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
                    exists: state.exists,
                }
            }

            fn proto_to_wit_state(state: &proto::State) -> wit_types::State {
                wit_types::State {
                    identifier: state.identifier.clone(),
                    attributes: proto_to_wit_value_map(&state.attributes),
                    exists: state.exists,
                }
            }

            fn proto_to_wit_create_outcome(
                outcome: &proto::CreateOutcome,
            ) -> wit_types::CreateOutcome {
                match outcome {
                    proto::CreateOutcome::Success { state } => {
                        wit_types::CreateOutcome::Success(proto_to_wit_state(state))
                    }
                    proto::CreateOutcome::PartialSuccess { state, diagnostic } => {
                        wit_types::CreateOutcome::PartialSuccess(
                            wit_types::CreatePartialSuccess {
                                state: proto_to_wit_state(state),
                                diagnostic: wit_types::PartialReadDiagnostic {
                                    reason: diagnostic.reason.clone(),
                                    missing_attributes: diagnostic.missing_attributes.clone(),
                                },
                            },
                        )
                    }
                }
            }

            fn proto_to_wit_update_outcome(
                outcome: &proto::UpdateOutcome,
            ) -> wit_types::UpdateOutcome {
                match outcome {
                    proto::UpdateOutcome::Success { state } => {
                        wit_types::UpdateOutcome::Success(proto_to_wit_state(state))
                    }
                    proto::UpdateOutcome::PartialSuccess { state, diagnostic } => {
                        wit_types::UpdateOutcome::PartialSuccess(
                            wit_types::UpdatePartialSuccess {
                                state: proto_to_wit_state(state),
                                diagnostic: wit_types::PartialReadDiagnostic {
                                    reason: diagnostic.reason.clone(),
                                    missing_attributes: diagnostic.missing_attributes.clone(),
                                },
                            },
                        )
                    }
                }
            }

            fn wit_to_proto_resource(res: &wit_types::ResourceDef) -> proto::Resource {
                proto::Resource {
                    id: wit_to_proto_resource_id(&res.id),
                    attributes: wit_to_proto_value_map(&res.attributes),
                    directives: proto::Directives::default(),
                }
            }

            fn proto_to_wit_resource(res: &proto::Resource) -> wit_types::ResourceDef {
                wit_types::ResourceDef {
                    id: proto_to_wit_resource_id(&res.id),
                    attributes: proto_to_wit_value_map(&res.attributes),
                }
            }

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
            ) -> proto::UpdateRequest {
                let from = wit_to_proto_state(proto_id, &req.current);
                let ops = req
                    .patch
                    .ops
                    .into_iter()
                    .map(|op| proto::PatchOp {
                        kind: wit_to_proto_patch_op_kind(op.kind),
                        key: op.key,
                        value: op.value.as_ref().map(wit_to_proto_value),
                    })
                    .collect();
                proto::UpdateRequest {
                    from,
                    patch: proto::UpdatePatch { ops },
                }
            }

            fn wit_to_proto_create_request(
                req: wit_types::CreateRequest,
            ) -> proto::CreateRequest {
                proto::CreateRequest {
                    resource: wit_to_proto_resource(&req.res),
                }
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
                    serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".to_string())
                }

                fn schemas() -> String {
                    let provider = get_provider().lock().unwrap();
                    let schemas = $crate::CarinaProvider::schemas(&*provider);
                    serde_json::to_string(&schemas).unwrap_or_else(|_| "[]".to_string())
                }

                fn provider_config_attribute_types() -> String {
                    let provider = get_provider().lock().unwrap();
                    let types = $crate::CarinaProvider::provider_config_attribute_types(
                        &*provider,
                    );
                    serde_json::to_string(&types).unwrap_or_else(|_| "{}".to_string())
                }

                fn validate_config(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let map = wit_to_proto_value_map(&attrs);
                    $crate::CarinaProvider::validate_config(&*provider, &map)
                        .map_err(validate_string_to_provider_error)
                }

                fn initialize(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), wit_types::ProviderError> {
                    let mut provider = get_provider().lock().unwrap();
                    let map = wit_to_proto_value_map(&attrs);
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
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn read_data_source(
                    res: wit_types::ResourceDef,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_res = wit_to_proto_resource(&res);
                    match $crate::CarinaProvider::read_data_source(&*provider, &proto_res) {
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn create(
                    id: wit_types::ResourceId,
                    request: wit_types::CreateRequest,
                ) -> Result<wit_types::CreateOutcome, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_create_request(request);
                    match $crate::CarinaProvider::create(&*provider, &proto_id, proto_request) {
                        Ok(outcome) => Ok(proto_to_wit_create_outcome(&outcome)),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn update(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::UpdateRequest,
                ) -> Result<wit_types::UpdateOutcome, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_update_request(request, &proto_id);
                    match $crate::CarinaProvider::update(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto_request,
                    ) {
                        Ok(outcome) => Ok(proto_to_wit_update_outcome(&outcome)),
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
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    $crate::CarinaProvider::required_permissions(
                        &*provider,
                        &proto_id,
                        wit_to_sdk_plan_op(operation),
                    )
                }

                fn satisfier_hint(
                    target_id: wit_types::ResourceId,
                    attr_path: Vec<String>,
                ) -> Vec<wit_types::BindingPattern> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&target_id);
                    $crate::CarinaProvider::satisfier_hint(&*provider, &proto_id, &attr_path)
                        .iter()
                        .map(sdk_to_wit_binding_pattern)
                        .collect()
                }

                fn provider_config_completions() -> String {
                    let provider = get_provider().lock().unwrap();
                    let completions = $crate::CarinaProvider::config_completions(&*provider);
                    serde_json::to_string(&completions).unwrap_or_else(|_| "{}".to_string())
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
                    serde_json::to_string(&aliases).unwrap_or_else(|_| "{}".to_string())
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
                            let parsed_id = helpers::parse_resource_id_string(k);
                            (k.clone(), wit_to_proto_state(&parsed_id, s))
                        })
                        .collect();
                    let result =
                        $crate::CarinaProvider::normalize_state(&*provider, proto_states);
                    result
                        .into_iter()
                        .map(|(k, s)| (k, proto_to_wit_state(&s)))
                        .collect()
                }

                fn hydrate_read_state(
                    states: Vec<(String, wit_types::State)>,
                    saved_attrs: Vec<(String, Vec<(String, wit_types::Value)>)>,
                ) -> Vec<(String, wit_types::State)> {
                    let provider = get_provider().lock().unwrap();
                    let mut proto_states: HashMap<String, proto::State> = states
                        .iter()
                        .map(|(k, s)| {
                            let parsed_id = helpers::parse_resource_id_string(k);
                            (k.clone(), wit_to_proto_state(&parsed_id, s))
                        })
                        .collect();
                    let proto_saved: HashMap<String, HashMap<String, proto::Value>> = saved_attrs
                        .iter()
                        .map(|(k, attrs)| (k.clone(), wit_to_proto_value_map(attrs)))
                        .collect();
                    $crate::CarinaProvider::hydrate_read_state(
                        &*provider,
                        &mut proto_states,
                        &proto_saved,
                    );
                    proto_states
                        .into_iter()
                        .map(|(k, s)| (k, proto_to_wit_state(&s)))
                        .collect()
                }

                fn merge_default_tags(
                    resources: Vec<wit_types::ResourceDef>,
                    default_tags: Vec<(String, wit_types::Value)>,
                ) -> Vec<wit_types::ResourceDef> {
                    let provider = get_provider().lock().unwrap();
                    let mut proto_resources: Vec<_> =
                        resources.iter().map(wit_to_proto_resource).collect();
                    let proto_tags: HashMap<String, proto::Value> = default_tags
                        .iter()
                        .map(|(k, v)| (k.clone(), wit_to_proto_value(v)))
                        .collect();
                    let schemas = $crate::CarinaProvider::schemas(&*provider);
                    $crate::CarinaProvider::merge_default_tags(
                        &*provider,
                        &mut proto_resources,
                        &proto_tags,
                        &schemas,
                    );
                    proto_resources.iter().map(proto_to_wit_resource).collect()
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
                    // The host marks an attribute as a secret with this
                    // variant (carina#2390). `proto::Value` does not yet
                    // carry a `Secret` arm, so we decode the JSON-encoded
                    // inner value here and surface it to the provider as
                    // an opaque value — providers MUST NOT log or persist
                    // values that arrived in attributes the host marked
                    // as secret. Adding `proto::Value::Secret` to preserve
                    // the signal end-to-end is tracked separately.
                    wit_types::Value::SecretVal(json) => {
                        let inner: serde_json::Value =
                            serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
                        helpers::json_to_proto_value(inner)
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
                    exists: state.exists,
                }
            }

            fn proto_to_wit_state(state: &proto::State) -> wit_types::State {
                wit_types::State {
                    identifier: state.identifier.clone(),
                    attributes: proto_to_wit_value_map(&state.attributes),
                    exists: state.exists,
                }
            }

            fn proto_to_wit_create_outcome(
                outcome: &proto::CreateOutcome,
            ) -> wit_types::CreateOutcome {
                match outcome {
                    proto::CreateOutcome::Success { state } => {
                        wit_types::CreateOutcome::Success(proto_to_wit_state(state))
                    }
                    proto::CreateOutcome::PartialSuccess { state, diagnostic } => {
                        wit_types::CreateOutcome::PartialSuccess(
                            wit_types::CreatePartialSuccess {
                                state: proto_to_wit_state(state),
                                diagnostic: wit_types::PartialReadDiagnostic {
                                    reason: diagnostic.reason.clone(),
                                    missing_attributes: diagnostic.missing_attributes.clone(),
                                },
                            },
                        )
                    }
                }
            }

            fn proto_to_wit_update_outcome(
                outcome: &proto::UpdateOutcome,
            ) -> wit_types::UpdateOutcome {
                match outcome {
                    proto::UpdateOutcome::Success { state } => {
                        wit_types::UpdateOutcome::Success(proto_to_wit_state(state))
                    }
                    proto::UpdateOutcome::PartialSuccess { state, diagnostic } => {
                        wit_types::UpdateOutcome::PartialSuccess(
                            wit_types::UpdatePartialSuccess {
                                state: proto_to_wit_state(state),
                                diagnostic: wit_types::PartialReadDiagnostic {
                                    reason: diagnostic.reason.clone(),
                                    missing_attributes: diagnostic.missing_attributes.clone(),
                                },
                            },
                        )
                    }
                }
            }

            fn wit_to_proto_resource(res: &wit_types::ResourceDef) -> proto::Resource {
                proto::Resource {
                    id: wit_to_proto_resource_id(&res.id),
                    attributes: wit_to_proto_value_map(&res.attributes),
                    directives: proto::Directives::default(),
                }
            }

            fn proto_to_wit_resource(res: &proto::Resource) -> wit_types::ResourceDef {
                wit_types::ResourceDef {
                    id: proto_to_wit_resource_id(&res.id),
                    attributes: proto_to_wit_value_map(&res.attributes),
                }
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
            ) -> proto::UpdateRequest {
                let from = wit_to_proto_state(proto_id, &req.current);
                let ops = req
                    .patch
                    .ops
                    .into_iter()
                    .map(|op| proto::PatchOp {
                        kind: wit_to_proto_patch_op_kind(op.kind),
                        key: op.key,
                        value: op.value.as_ref().map(wit_to_proto_value),
                    })
                    .collect();
                proto::UpdateRequest {
                    from,
                    patch: proto::UpdatePatch { ops },
                }
            }

            fn wit_to_proto_create_request(
                req: wit_types::CreateRequest,
            ) -> proto::CreateRequest {
                proto::CreateRequest {
                    resource: wit_to_proto_resource(&req.res),
                }
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
                    serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".to_string())
                }

                fn schemas() -> String {
                    let provider = get_provider().lock().unwrap();
                    let schemas = $crate::CarinaProvider::schemas(&*provider);
                    serde_json::to_string(&schemas).unwrap_or_else(|_| "[]".to_string())
                }

                fn provider_config_attribute_types() -> String {
                    let provider = get_provider().lock().unwrap();
                    let types = $crate::CarinaProvider::provider_config_attribute_types(
                        &*provider,
                    );
                    serde_json::to_string(&types).unwrap_or_else(|_| "{}".to_string())
                }

                fn validate_config(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let map = wit_to_proto_value_map(&attrs);
                    $crate::CarinaProvider::validate_config(&*provider, &map)
                        .map_err(validate_string_to_provider_error)
                }

                fn initialize(
                    attrs: Vec<(String, wit_types::Value)>,
                ) -> Result<(), wit_types::ProviderError> {
                    let mut provider = get_provider().lock().unwrap();
                    let map = wit_to_proto_value_map(&attrs);
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
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn read_data_source(
                    res: wit_types::ResourceDef,
                ) -> Result<wit_types::State, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_res = wit_to_proto_resource(&res);
                    match $crate::CarinaProvider::read_data_source(&*provider, &proto_res) {
                        Ok(state) => Ok(proto_to_wit_state(&state)),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn create(
                    id: wit_types::ResourceId,
                    request: wit_types::CreateRequest,
                ) -> Result<wit_types::CreateOutcome, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_create_request(request);
                    match $crate::CarinaProvider::create(&*provider, &proto_id, proto_request) {
                        Ok(outcome) => Ok(proto_to_wit_create_outcome(&outcome)),
                        Err(e) => Err(proto_to_wit_provider_error(e)),
                    }
                }

                fn update(
                    id: wit_types::ResourceId,
                    identifier: String,
                    request: wit_types::UpdateRequest,
                ) -> Result<wit_types::UpdateOutcome, wit_types::ProviderError> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    let proto_request = wit_to_proto_update_request(request, &proto_id);
                    match $crate::CarinaProvider::update(
                        &*provider,
                        &proto_id,
                        &identifier,
                        proto_request,
                    ) {
                        Ok(outcome) => Ok(proto_to_wit_update_outcome(&outcome)),
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
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&id);
                    $crate::CarinaProvider::required_permissions(
                        &*provider,
                        &proto_id,
                        wit_to_sdk_plan_op(operation),
                    )
                }

                fn satisfier_hint(
                    target_id: wit_types::ResourceId,
                    attr_path: Vec<String>,
                ) -> Vec<wit_types::BindingPattern> {
                    let provider = get_provider().lock().unwrap();
                    let proto_id = wit_to_proto_resource_id(&target_id);
                    $crate::CarinaProvider::satisfier_hint(&*provider, &proto_id, &attr_path)
                        .iter()
                        .map(sdk_to_wit_binding_pattern)
                        .collect()
                }

                fn provider_config_completions() -> String {
                    let provider = get_provider().lock().unwrap();
                    let completions = $crate::CarinaProvider::config_completions(&*provider);
                    serde_json::to_string(&completions).unwrap_or_else(|_| "{}".to_string())
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
                    serde_json::to_string(&aliases).unwrap_or_else(|_| "{}".to_string())
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
                            let parsed_id = helpers::parse_resource_id_string(k);
                            (k.clone(), wit_to_proto_state(&parsed_id, s))
                        })
                        .collect();
                    let result =
                        $crate::CarinaProvider::normalize_state(&*provider, proto_states);
                    result
                        .into_iter()
                        .map(|(k, s)| (k, proto_to_wit_state(&s)))
                        .collect()
                }

                fn hydrate_read_state(
                    states: Vec<(String, wit_types::State)>,
                    saved_attrs: Vec<(String, Vec<(String, wit_types::Value)>)>,
                ) -> Vec<(String, wit_types::State)> {
                    let provider = get_provider().lock().unwrap();
                    let mut proto_states: HashMap<String, proto::State> = states
                        .iter()
                        .map(|(k, s)| {
                            let parsed_id = helpers::parse_resource_id_string(k);
                            (k.clone(), wit_to_proto_state(&parsed_id, s))
                        })
                        .collect();
                    let proto_saved: HashMap<String, HashMap<String, proto::Value>> = saved_attrs
                        .iter()
                        .map(|(k, attrs)| (k.clone(), wit_to_proto_value_map(attrs)))
                        .collect();
                    $crate::CarinaProvider::hydrate_read_state(
                        &*provider,
                        &mut proto_states,
                        &proto_saved,
                    );
                    proto_states
                        .into_iter()
                        .map(|(k, s)| (k, proto_to_wit_state(&s)))
                        .collect()
                }

                fn merge_default_tags(
                    resources: Vec<wit_types::ResourceDef>,
                    default_tags: Vec<(String, wit_types::Value)>,
                ) -> Vec<wit_types::ResourceDef> {
                    let provider = get_provider().lock().unwrap();
                    let mut proto_resources: Vec<_> =
                        resources.iter().map(wit_to_proto_resource).collect();
                    let proto_tags: HashMap<String, proto::Value> = default_tags
                        .iter()
                        .map(|(k, v)| (k.clone(), wit_to_proto_value(v)))
                        .collect();
                    let schemas = $crate::CarinaProvider::schemas(&*provider);
                    $crate::CarinaProvider::merge_default_tags(
                        &*provider,
                        &mut proto_resources,
                        &proto_tags,
                        &schemas,
                    );
                    proto_resources.iter().map(proto_to_wit_resource).collect()
                }
            }

            export!(WasmGuest);
        }
    };
}
