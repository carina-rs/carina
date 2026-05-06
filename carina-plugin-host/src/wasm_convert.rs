//! Conversions between carina-core types and Wasmtime-generated WIT types.

use std::collections::HashMap;

use carina_core::resource::{
    LifecycleConfig, Resource as CoreResource, ResourceId as CoreResourceId, State as CoreState,
    Value as CoreValue,
};
use carina_core::schema::{
    AttributeSchema as CoreAttributeSchema, AttributeType as CoreAttributeType,
    ResourceSchema as CoreResourceSchema, StructField as CoreStructField, noop_validator,
};
use carina_core::value::{SerializationContext, SerializationError};

use carina_provider_protocol::types as proto;

use crate::wasm_bindings::carina::provider::types as wit;

// -- Value --

/// Convert a core Value to a WIT Value.
///
/// List and Map values are serialized to JSON strings because WIT does
/// not support recursive types.
///
/// Returns `Err(SerializationError)` for `Value::Unknown`,
/// `Value::ResourceRef`, `Value::Interpolation`, and
/// `Value::FunctionCall`. The plan-time pipeline strips every attribute
/// that recursively contains one of these via
/// `PlanPreprocessor::prepare`'s strip-and-restore pass, so reaching
/// any of these arms is a producer-side bug — the diagnostic identifies
/// which variant leaked. See RFC #2371 stage 2 (#2378) for `Unknown`
/// and #2387 for `ResourceRef` / `Interpolation` / `FunctionCall`.
///
/// `Value::Secret(inner)` is encoded as the `secret-val` WIT variant
/// (#2390): the inner is JSON-encoded the same way `list-val` / `map-val`
/// encode their contents, and a typed signal crosses the boundary so the
/// WASM provider can distinguish a secret from a plain string. The
/// previous `format!("{v:?}")` debug-format fallback would send
/// `"Secret(String(\"…\"))"` literally to the provider — a contract leak
/// equivalent to the pre-#2387 `ResourceRef` debug-string.
pub fn core_to_wit_value(v: &CoreValue) -> Result<wit::Value, SerializationError> {
    match v {
        CoreValue::String(s) => Ok(wit::Value::StrVal(s.clone())),
        CoreValue::Int(i) => Ok(wit::Value::IntVal(*i)),
        CoreValue::Float(f) => Ok(wit::Value::FloatVal(*f)),
        CoreValue::Bool(b) => Ok(wit::Value::BoolVal(*b)),
        CoreValue::List(items) => {
            let json_items: Result<Vec<serde_json::Value>, _> =
                items.iter().map(core_value_to_json).collect();
            let json_str = serde_json::to_string(&json_items?)
                .expect("serde_json::Value -> String is infallible");
            Ok(wit::Value::ListVal(json_str))
        }
        CoreValue::StringList(items) => {
            // Cross the WASM boundary as a plain JSON array of strings —
            // the provider sees the same shape as `Value::List([String])`,
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
        CoreValue::Map(map) => {
            let json_map: Result<serde_json::Map<String, serde_json::Value>, _> = map
                .iter()
                .map(|(k, v)| core_value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect();
            let json_str = serde_json::to_string(&json_map?)
                .expect("serde_json::Value -> String is infallible");
            Ok(wit::Value::MapVal(json_str))
        }
        CoreValue::Unknown(reason) => Err(SerializationError::UnknownNotAllowed {
            reason: reason.clone(),
            context: SerializationContext::WasmBoundary,
        }),
        CoreValue::ResourceRef { path } => Err(SerializationError::UnresolvedResourceRef {
            path: path.to_dot_string(),
            context: SerializationContext::WasmBoundary,
        }),
        CoreValue::Interpolation(_) => Err(SerializationError::UnresolvedInterpolation {
            context: SerializationContext::WasmBoundary,
        }),
        CoreValue::FunctionCall { name, .. } => Err(SerializationError::UnresolvedFunctionCall {
            name: name.clone(),
            context: SerializationContext::WasmBoundary,
        }),
        CoreValue::Secret(inner) => {
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
        wit::Value::StrVal(s) => CoreValue::String(s.clone()),
        wit::Value::IntVal(i) => CoreValue::Int(*i),
        wit::Value::FloatVal(f) => CoreValue::Float(*f),
        wit::Value::BoolVal(b) => CoreValue::Bool(*b),
        wit::Value::ListVal(json) => {
            let items: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
            CoreValue::List(items.iter().map(json_to_core_value).collect())
        }
        wit::Value::MapVal(json) => {
            let map: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(json).unwrap_or_default();
            CoreValue::Map(
                map.iter()
                    .map(|(k, v)| (k.clone(), json_to_core_value(v)))
                    .collect(),
            )
        }
        wit::Value::SecretVal(json) => {
            // Decode the JSON-encoded inner value the same way `ListVal` /
            // `MapVal` decode theirs, then re-wrap in `Value::Secret` so the
            // host's secret-tracking machinery (state hashing, plan
            // redaction) keeps working. A malformed encoding falls back to
            // an empty string secret rather than panicking — the WASM
            // provider produced this, so we treat it as untrusted input.
            let inner_json: serde_json::Value =
                serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
            let inner = json_to_core_value(&inner_json);
            CoreValue::Secret(Box::new(inner))
        }
    }
}

/// Helper: convert a core Value to a serde_json::Value for JSON
/// encoding inside the WIT-string fallback for List/Map. Returns
/// `Err` for the same set of variants as `core_to_wit_value` — see
/// that function's doc for the rationale and the strip-and-restore
/// pass that keeps these arms unreachable in legitimate flows.
fn core_value_to_json(v: &CoreValue) -> Result<serde_json::Value, SerializationError> {
    match v {
        CoreValue::String(s) => Ok(serde_json::Value::String(s.clone())),
        CoreValue::Int(i) => Ok(serde_json::Value::Number((*i).into())),
        CoreValue::Float(f) => Ok(serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)),
        CoreValue::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        CoreValue::List(items) => {
            let arr: Result<Vec<_>, _> = items.iter().map(core_value_to_json).collect();
            Ok(serde_json::Value::Array(arr?))
        }
        CoreValue::StringList(items) => Ok(serde_json::Value::Array(
            items
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        )),
        CoreValue::Map(map) => {
            let obj: Result<serde_json::Map<String, serde_json::Value>, _> = map
                .iter()
                .map(|(k, v)| core_value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect();
            Ok(serde_json::Value::Object(obj?))
        }
        CoreValue::Unknown(reason) => Err(SerializationError::UnknownNotAllowed {
            reason: reason.clone(),
            context: SerializationContext::WasmBoundary,
        }),
        CoreValue::ResourceRef { path } => Err(SerializationError::UnresolvedResourceRef {
            path: path.to_dot_string(),
            context: SerializationContext::WasmBoundary,
        }),
        CoreValue::Interpolation(_) => Err(SerializationError::UnresolvedInterpolation {
            context: SerializationContext::WasmBoundary,
        }),
        CoreValue::FunctionCall { name, .. } => Err(SerializationError::UnresolvedFunctionCall {
            name: name.clone(),
            context: SerializationContext::WasmBoundary,
        }),
        // Within this helper a `Secret` would only appear if the caller
        // wrapped one inside a `List` / `Map` payload going to a WIT
        // `list-val` / `map-val`. Render it as the JSON-encoded inner so
        // the byte stream the provider receives is identical to what
        // `core_to_wit_value`'s `secret-val` arm produces. Provider plugins
        // that decode `list-val` / `map-val` see the inner JSON shape;
        // `secret-val` is the channel used to mark the *attribute* itself
        // as a secret.
        CoreValue::Secret(inner) => core_value_to_json(inner),
    }
}

/// Helper: convert a serde_json::Value to a core Value.
fn json_to_core_value(v: &serde_json::Value) -> CoreValue {
    match v {
        serde_json::Value::String(s) => CoreValue::String(s.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CoreValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                CoreValue::Float(f)
            } else {
                CoreValue::String(n.to_string())
            }
        }
        serde_json::Value::Bool(b) => CoreValue::Bool(*b),
        serde_json::Value::Array(items) => {
            CoreValue::List(items.iter().map(json_to_core_value).collect())
        }
        serde_json::Value::Object(map) => CoreValue::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_core_value(v)))
                .collect(),
        ),
        serde_json::Value::Null => CoreValue::String(String::new()),
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
    CoreResourceId::with_provider(&id.provider, &id.resource_type, &id.name)
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

pub fn wit_to_core_resource(resource: &wit::ResourceDef) -> CoreResource {
    let id = wit_to_core_resource_id(&resource.id);
    let mut core_resource =
        CoreResource::with_provider(&id.provider, &id.resource_type, id.name_str());
    core_resource.attributes = resource
        .attributes
        .iter()
        .map(|(k, v)| (k.clone(), wit_to_core_value(v)))
        .collect();
    core_resource
}

// -- JSON passthrough functions for provider-specific types --

/// Serialize LifecycleConfig to JSON string for the WIT boundary.
pub fn lifecycle_to_json(lifecycle: &LifecycleConfig) -> String {
    serde_json::to_string(lifecycle).unwrap_or_else(|_| "{}".to_string())
}

/// Deserialize a JSON error string to a core ProviderError.
pub fn json_to_provider_error(json: &str) -> carina_core::provider::ProviderError {
    if let Ok(proto_err) = serde_json::from_str::<proto::ProviderError>(json) {
        carina_core::provider::ProviderError {
            message: proto_err.message,
            resource_id: proto_err.resource_id.map(|pid| {
                Box::new(CoreResourceId::with_provider(
                    &pid.provider,
                    &pid.resource_type,
                    &pid.name,
                ))
            }),
            cause: None,
            is_timeout: proto_err.is_timeout,
            provider_name: None,
        }
    } else {
        carina_core::provider::ProviderError {
            message: json.to_string(),
            resource_id: None,
            cause: None,
            is_timeout: false,
            provider_name: None,
        }
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

/// Deserialize JSON to a Vec of core ResourceSchemas.
pub fn json_to_schemas(json: &str) -> Vec<CoreResourceSchema> {
    let proto_schemas: Vec<proto::ResourceSchema> = serde_json::from_str(json).unwrap_or_default();
    proto_schemas.iter().map(proto_schema_to_core).collect()
}

/// Deserialize a JSON-encoded `HashMap<String, AttributeType>` from a WASM
/// guest and convert it to core `AttributeType` values.
pub fn json_to_attribute_types(json: &str) -> HashMap<String, CoreAttributeType> {
    let proto_types: HashMap<String, proto::AttributeType> =
        serde_json::from_str(json).unwrap_or_default();
    proto_types
        .into_iter()
        .map(|(k, v)| (k, proto_attr_type_to_core(&v)))
        .collect()
}

// -- Protocol schema to core schema conversion --

fn proto_schema_to_core(s: &proto::ResourceSchema) -> CoreResourceSchema {
    CoreResourceSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .iter()
            .map(|(name, a)| (name.clone(), proto_attr_schema_to_core(a)))
            .collect(),
        description: s.description.clone(),
        validator: build_validator_from_types(&s.validators),
        kind: match s.kind {
            proto::SchemaKind::Managed => carina_core::schema::SchemaKind::Managed,
            proto::SchemaKind::DataSource => carina_core::schema::SchemaKind::DataSource,
        },
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
        operation_config: s.operation_config.as_ref().map(|c| {
            carina_core::schema::OperationConfig {
                delete_timeout_secs: c.delete_timeout_secs,
                delete_max_retries: c.delete_max_retries,
                create_timeout_secs: c.create_timeout_secs,
                create_max_retries: c.create_max_retries,
            }
        }),
        exclusive_required: s.exclusive_required.clone(),
    }
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
    if let Some(CoreValue::Map(map)) = attrs.get("tags") {
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

fn proto_attr_schema_to_core(a: &proto::AttributeSchema) -> CoreAttributeSchema {
    CoreAttributeSchema {
        name: a.name.clone(),
        attr_type: proto_attr_type_to_core(&a.attr_type),
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
    }
}

fn proto_attr_type_to_core(t: &proto::AttributeType) -> CoreAttributeType {
    match t {
        proto::AttributeType::String => CoreAttributeType::String,
        proto::AttributeType::Int => CoreAttributeType::Int,
        proto::AttributeType::Float => CoreAttributeType::Float,
        proto::AttributeType::Bool => CoreAttributeType::Bool,
        proto::AttributeType::StringEnum {
            values,
            name,
            namespace,
        } => CoreAttributeType::StringEnum {
            name: name.clone(),
            values: values.clone(),
            namespace: namespace.clone(),
            to_dsl: None,
        },
        proto::AttributeType::List { inner, ordered } => CoreAttributeType::List {
            inner: Box::new(proto_attr_type_to_core(inner)),
            ordered: *ordered,
        },
        proto::AttributeType::Map { inner, key } => CoreAttributeType::map_with_key(
            proto_attr_type_to_core(key),
            proto_attr_type_to_core(inner),
        ),
        proto::AttributeType::Struct { name, fields } => CoreAttributeType::Struct {
            name: name.clone(),
            fields: fields.iter().map(proto_struct_field_to_core).collect(),
        },
        proto::AttributeType::Union { members } => {
            CoreAttributeType::Union(members.iter().map(proto_attr_type_to_core).collect())
        }
        proto::AttributeType::Custom {
            name,
            base,
            namespace,
        } => CoreAttributeType::Custom {
            semantic_name: if name.is_empty() {
                None
            } else {
                Some(name.clone())
            },
            base: Box::new(proto_attr_type_to_core(base)),
            pattern: None,
            length: None,
            validate: noop_validator(), // Validation is handled via ProviderContext.validators
            namespace: namespace.clone(),
            to_dsl: None,
        },
    }
}

fn proto_struct_field_to_core(f: &proto::StructField) -> CoreStructField {
    CoreStructField {
        name: f.name.clone(),
        field_type: proto_attr_type_to_core(&f.field_type),
        required: f.required,
        description: f.description.clone(),
        provider_name: f.provider_name.clone(),
        block_name: f.block_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC #2371 stage 4 contract pin: `Value::Unknown` reaching either
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
        let v = CoreValue::Unknown(UnknownReason::UpstreamRef { path: path.clone() });
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
        let v = CoreValue::Unknown(UnknownReason::UpstreamRef { path: path.clone() });
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
        let core = CoreValue::Bool(true);
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_int_roundtrip() {
        let core = CoreValue::Int(42);
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_float_roundtrip() {
        let core = CoreValue::Float(2.78);
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_string_roundtrip() {
        let core = CoreValue::String("hello".into());
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_list_roundtrip() {
        let core = CoreValue::List(vec![
            CoreValue::String("a".into()),
            CoreValue::Int(1),
            CoreValue::Bool(false),
        ]);
        let wit = core_to_wit_value(&core).unwrap();
        assert!(matches!(wit, wit::Value::ListVal(_)));
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_map_roundtrip() {
        let core = CoreValue::Map(
            vec![
                ("key1".to_string(), CoreValue::String("val1".into())),
                ("key2".to_string(), CoreValue::Int(99)),
            ]
            .into_iter()
            .collect(),
        );
        let wit = core_to_wit_value(&core).unwrap();
        assert!(matches!(wit, wit::Value::MapVal(_)));
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_nested_list_of_maps_roundtrip() {
        let inner_map = CoreValue::Map(
            vec![
                ("name".to_string(), CoreValue::String("test".into())),
                ("count".to_string(), CoreValue::Int(5)),
            ]
            .into_iter()
            .collect(),
        );
        let core = CoreValue::List(vec![inner_map.clone(), inner_map]);
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_nested_map_of_lists_roundtrip() {
        let core = CoreValue::Map(
            vec![
                (
                    "tags".to_string(),
                    CoreValue::List(vec![
                        CoreValue::String("a".into()),
                        CoreValue::String("b".into()),
                    ]),
                ),
                (
                    "counts".to_string(),
                    CoreValue::List(vec![CoreValue::Int(1), CoreValue::Int(2)]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let wit = core_to_wit_value(&core).unwrap();
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    /// #2387: `Value::ResourceRef` reaching `core_to_wit_value` is a
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
        let v = CoreValue::ResourceRef { path: path.clone() };
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
        let v = CoreValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(CoreValue::ResourceRef {
                path: AccessPath::new("vpc", "id"),
            }),
        ]);
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
        let v = CoreValue::FunctionCall {
            name: "join".into(),
            args: vec![
                CoreValue::String("-".into()),
                CoreValue::ResourceRef {
                    path: AccessPath::new("vpc", "id"),
                },
            ],
        };
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
        let v = CoreValue::ResourceRef { path: path.clone() };
        let err = core_value_to_json(&v).unwrap_err();
        assert!(matches!(
            err,
            SerializationError::UnresolvedResourceRef {
                context: SerializationContext::WasmBoundary,
                ..
            }
        ));
    }

    /// #2390: `Value::Secret` crosses the boundary as the `secret-val`
    /// WIT variant carrying the JSON-encoded inner — never as a
    /// `format!("{v:?}")` debug string.
    #[test]
    fn secret_string_emits_secret_val_variant() {
        let v = CoreValue::Secret(Box::new(CoreValue::String("password".into())));
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
        let v = CoreValue::Secret(Box::new(CoreValue::Int(42)));
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
        let original = CoreValue::Secret(Box::new(CoreValue::String("hunter2".into())));
        let wit_v = core_to_wit_value(&original).unwrap();
        let back = wit_to_core_value(&wit_v);
        match back {
            CoreValue::Secret(inner) => match *inner {
                CoreValue::String(s) => assert_eq!(s, "hunter2"),
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
        let v = CoreValue::List(vec![CoreValue::Secret(Box::new(CoreValue::String(
            "p".into(),
        )))]);
        let wit_v = core_to_wit_value(&v).unwrap();
        match wit_v {
            wit::Value::ListVal(json) => assert_eq!(json, "[\"p\"]"),
            other => panic!("expected ListVal, got: {other:?}"),
        }
    }

    // -- ResourceId roundtrip --

    #[test]
    fn test_resource_id_roundtrip() {
        let core = CoreResourceId::with_provider("aws", "s3.Bucket", "my-bucket");
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
        let id = CoreResourceId::with_provider("aws", "s3.Bucket", "my-bucket");
        let mut attrs = HashMap::new();
        attrs.insert("name".into(), CoreValue::String("my-bucket".into()));
        attrs.insert("region".into(), CoreValue::String("us-east-1".into()));
        let core = CoreState::existing(id.clone(), attrs);

        let wit = core_to_wit_state(&core).unwrap();
        let back = wit_to_core_state(&wit, &id);

        assert_eq!(back.id, core.id);
        assert_eq!(back.attributes, core.attributes);
        assert!(back.exists);
    }

    #[test]
    fn test_state_with_identifier_roundtrip() {
        let id = CoreResourceId::with_provider("aws", "ec2.Vpc", "main");
        let attrs = HashMap::from([("cidr".into(), CoreValue::String("10.0.0.0/16".into()))]);
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
            ("a".into(), CoreValue::String("hello".into())),
            ("b".into(), CoreValue::Int(42)),
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
        let mut resource = CoreResource::with_provider("aws", "s3.Bucket", "my-bucket");
        resource.attributes = indexmap::IndexMap::from([
            ("name".into(), CoreValue::String("my-bucket".into())),
            ("region".into(), CoreValue::String("us-east-1".into())),
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
    fn test_lifecycle_to_json() {
        let lifecycle = LifecycleConfig {
            force_delete: true,
            create_before_destroy: false,
            prevent_destroy: false,
        };
        let json = lifecycle_to_json(&lifecycle);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["force_delete"], true);
        assert_eq!(parsed["create_before_destroy"], false);
        assert_eq!(parsed["prevent_destroy"], false);
    }

    #[test]
    fn test_json_to_provider_error_valid() {
        let json = r#"{"message":"something failed","resource_id":{"provider":"aws","resource_type":"s3.Bucket","name":"test"},"is_timeout":true}"#;
        let err = json_to_provider_error(json);
        assert_eq!(err.message, "something failed");
        assert!(err.is_timeout);
        assert_eq!(err.resource_id.as_ref().unwrap().provider, "aws");
    }

    #[test]
    fn test_json_to_provider_error_plain_string() {
        let err = json_to_provider_error("some error message");
        assert_eq!(err.message, "some error message");
        assert!(!err.is_timeout);
        assert!(err.resource_id.is_none());
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
    fn test_json_to_schemas_empty() {
        let schemas = json_to_schemas("[]");
        assert!(schemas.is_empty());
    }

    #[test]
    fn test_json_to_schemas_with_complex_attributes() {
        let json = r#"[
          {
            "resource_type": "ec2.SecurityGroup",
            "description": "EC2 Security Group",
            "name_attribute": "group_name",
            "force_replace": true,
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

        let schemas = json_to_schemas(json);
        assert_eq!(schemas.len(), 1);

        let schema = &schemas[0];
        assert_eq!(schema.resource_type, "ec2.SecurityGroup");
        assert_eq!(schema.description.as_deref(), Some("EC2 Security Group"));
        assert!(!schema.is_data_source());
        assert_eq!(schema.name_attribute.as_deref(), Some("group_name"));
        assert!(schema.force_replace);

        // Basic attribute types
        let desc_attr = schema
            .attributes
            .get("description")
            .expect("description attribute");
        assert_eq!(desc_attr.name, "description");
        assert!(matches!(desc_attr.attr_type, CoreAttributeType::String));
        assert!(desc_attr.required);

        let enabled_attr = schema.attributes.get("enabled").expect("enabled attribute");
        assert!(matches!(enabled_attr.attr_type, CoreAttributeType::Bool));

        let priority_attr = schema
            .attributes
            .get("priority")
            .expect("priority attribute");
        assert!(matches!(priority_attr.attr_type, CoreAttributeType::Int));

        // Ingress attribute: list with ordered=false, provider_name, block_name, removable
        let ingress_attr = schema.attributes.get("ingress").expect("ingress attribute");
        assert_eq!(ingress_attr.provider_name.as_deref(), Some("IpPermissions"));
        assert_eq!(ingress_attr.block_name.as_deref(), Some("ingress_block"));
        assert_eq!(ingress_attr.removable, Some(false));

        // List with ordered: false
        match &ingress_attr.attr_type {
            CoreAttributeType::List { inner, ordered } => {
                assert!(!ordered, "list should be unordered");

                // Union inside list
                match inner.as_ref() {
                    CoreAttributeType::Union(members) => {
                        assert_eq!(members.len(), 2);

                        // First member: struct with block_name and provider_name on fields
                        match &members[0] {
                            CoreAttributeType::Struct { name, fields } => {
                                assert_eq!(name, "IngressRule");
                                assert_eq!(fields.len(), 2);

                                let from_port = &fields[0];
                                assert_eq!(from_port.name, "from_port");
                                assert!(matches!(from_port.field_type, CoreAttributeType::Int));
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
                                assert!(matches!(protocol.field_type, CoreAttributeType::String));
                                assert!(protocol.block_name.is_none());
                                assert!(protocol.provider_name.is_none());
                            }
                            other => panic!("expected Struct, got {:?}", other),
                        }

                        // Second member: String
                        assert!(matches!(members[1], CoreAttributeType::String));
                    }
                    other => panic!("expected Union inside list, got {:?}", other),
                }
            }
            other => panic!("expected List, got {:?}", other),
        }
    }

    #[test]
    fn test_deeply_nested_list_map_roundtrip() {
        let policy_document = CoreValue::Map(
            vec![
                (
                    "version".to_string(),
                    CoreValue::String("2012-10-17".into()),
                ),
                (
                    "statement".to_string(),
                    CoreValue::List(vec![CoreValue::Map(
                        vec![
                            ("effect".to_string(), CoreValue::String("Allow".into())),
                            (
                                "action".to_string(),
                                CoreValue::String("logs:CreateLogGroup".into()),
                            ),
                            ("resource".to_string(), CoreValue::String("*".into())),
                        ]
                        .into_iter()
                        .collect(),
                    )]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let policies = CoreValue::List(vec![CoreValue::Map(
            vec![
                (
                    "policy_name".to_string(),
                    CoreValue::String("test-policy".into()),
                ),
                ("policy_document".to_string(), policy_document),
            ]
            .into_iter()
            .collect(),
        )]);

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
            force_replace: false,
            operation_config: None,
            validators: vec![proto::ValidatorType::TagsKeyValueCheck],
            exclusive_required: vec![],
        };
        let core_schema = proto_schema_to_core(&proto_schema);
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
            force_replace: false,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![],
        };
        let core_schema = proto_schema_to_core(&proto_schema);
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
            force_replace: false,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![vec![
                "cidr_block".to_string(),
                "ipv4_ipam_pool_id".to_string(),
            ]],
        };
        let core_schema = proto_schema_to_core(&proto_schema);
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
            force_replace: false,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![vec!["a".to_string(), "b".to_string()]],
        };
        let json = serde_json::to_string(&vec![proto_schema]).unwrap();
        let schemas = json_to_schemas(&json);
        assert_eq!(schemas.len(), 1);
        assert_eq!(
            schemas[0].exclusive_required,
            vec![vec!["a".to_string(), "b".to_string()]]
        );
    }

    #[test]
    fn test_tags_validator_detects_key_value_pattern() {
        let proto_schema = proto::ResourceSchema {
            resource_type: "awscc.s3.Bucket".to_string(),
            attributes: HashMap::new(),
            description: None,
            kind: proto::SchemaKind::Managed,
            name_attribute: None,
            force_replace: false,
            operation_config: None,
            validators: vec![proto::ValidatorType::TagsKeyValueCheck],
            exclusive_required: vec![],
        };
        let core_schema = proto_schema_to_core(&proto_schema);
        let validator = core_schema.validator.unwrap();

        // key/value pattern should fail
        let mut attrs = HashMap::new();
        attrs.insert(
            "tags".to_string(),
            CoreValue::Map(
                [
                    ("key".to_string(), CoreValue::String("Project".into())),
                    ("value".to_string(), CoreValue::String("carina".into())),
                ]
                .into_iter()
                .collect(),
            ),
        );
        assert!(validator(&attrs).is_err());

        // normal tags should pass
        let mut attrs = HashMap::new();
        attrs.insert(
            "tags".to_string(),
            CoreValue::Map(
                [("Project".to_string(), CoreValue::String("carina".into()))]
                    .into_iter()
                    .collect(),
            ),
        );
        assert!(validator(&attrs).is_ok());
    }
}
