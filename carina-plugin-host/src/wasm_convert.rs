//! Conversions between carina-core types and Wasmtime-generated WIT types.

use std::collections::HashMap;

use carina_core::resource::{
    Expr, LifecycleConfig, Resource as CoreResource, ResourceId as CoreResourceId,
    State as CoreState, Value as CoreValue,
};
use carina_core::schema::{
    AttributeSchema as CoreAttributeSchema, AttributeType as CoreAttributeType,
    ResourceSchema as CoreResourceSchema, StructField as CoreStructField,
};

use carina_provider_protocol::types as proto;

use crate::wasm_bindings::carina::provider::types as wit;

// -- Value --

/// Convert a core Value to a WIT Value.
///
/// List and Map values are serialized to JSON strings because WIT does not
/// support recursive types.
pub fn core_to_wit_value(v: &CoreValue) -> wit::Value {
    match v {
        CoreValue::String(s) => wit::Value::StrVal(s.clone()),
        CoreValue::Int(i) => wit::Value::IntVal(*i),
        CoreValue::Float(f) => wit::Value::FloatVal(*f),
        CoreValue::Bool(b) => wit::Value::BoolVal(*b),
        CoreValue::List(items) => {
            let json_items: Vec<serde_json::Value> = items.iter().map(core_value_to_json).collect();
            wit::Value::ListVal(serde_json::to_string(&json_items).unwrap())
        }
        CoreValue::Map(map) => {
            let json_map: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), core_value_to_json(v)))
                .collect();
            wit::Value::MapVal(serde_json::to_string(&json_map).unwrap())
        }
        // ResourceRef, Interpolation, FunctionCall, Closure, Secret
        // should be resolved before reaching the provider.
        _ => wit::Value::StrVal(format!("{v:?}")),
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
    }
}

/// Helper: convert a core Value to a serde_json::Value for JSON encoding.
fn core_value_to_json(v: &CoreValue) -> serde_json::Value {
    match v {
        CoreValue::String(s) => serde_json::Value::String(s.clone()),
        CoreValue::Int(i) => serde_json::Value::Number((*i).into()),
        CoreValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        CoreValue::Bool(b) => serde_json::Value::Bool(*b),
        CoreValue::List(items) => {
            serde_json::Value::Array(items.iter().map(core_value_to_json).collect())
        }
        CoreValue::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), core_value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        _ => serde_json::Value::String(format!("{v:?}")),
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

pub fn core_to_wit_value_map(map: &HashMap<String, CoreValue>) -> Vec<(String, wit::Value)> {
    map.iter()
        .map(|(k, v)| (k.clone(), core_to_wit_value(v)))
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
        name: id.name.clone(),
    }
}

pub fn wit_to_core_resource_id(id: &wit::ResourceId) -> CoreResourceId {
    CoreResourceId::with_provider(&id.provider, &id.resource_type, &id.name)
}

// -- State --

pub fn core_to_wit_state(state: &CoreState) -> wit::State {
    wit::State {
        identifier: state.identifier.clone(),
        attributes: core_to_wit_value_map(&state.attributes),
        exists: state.exists,
    }
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

pub fn core_to_wit_resource(resource: &CoreResource) -> wit::ResourceDef {
    wit::ResourceDef {
        id: core_to_wit_resource_id(&resource.id),
        attributes: core_to_wit_value_map(&resource.resolved_attributes()),
    }
}

pub fn wit_to_core_resource(resource: &wit::ResourceDef) -> CoreResource {
    let id = wit_to_core_resource_id(&resource.id);
    let mut core_resource = CoreResource::with_provider(&id.provider, &id.resource_type, &id.name);
    core_resource.attributes = resource
        .attributes
        .iter()
        .map(|(k, v)| (k.clone(), Expr(wit_to_core_value(v))))
        .collect();
    core_resource
}

// -- WIT type conversions for provider-specific types --

/// Convert a core LifecycleConfig to a WIT LifecycleConfig.
pub fn core_to_wit_lifecycle(lifecycle: &LifecycleConfig) -> wit::LifecycleConfig {
    wit::LifecycleConfig {
        prevent_destroy: lifecycle.prevent_destroy,
    }
}

/// Convert a WIT ProviderError to a core ProviderError.
pub fn wit_to_core_provider_error(
    err: &wit::ProviderError,
) -> carina_core::provider::ProviderError {
    carina_core::provider::ProviderError {
        message: err.message.clone(),
        resource_id: err
            .resource_id
            .as_ref()
            .map(|pid| CoreResourceId::with_provider(&pid.provider, &pid.resource_type, &pid.name)),
        cause: None,
        is_timeout: err.is_timeout,
    }
}

/// Convert a WIT ProviderInfo to a (name, display_name) tuple.
pub fn wit_to_provider_info(info: &wit::ProviderInfo) -> (String, String) {
    (info.name.clone(), info.display_name.clone())
}

/// Convert a Vec of WIT ResourceSchemas to a Vec of core ResourceSchemas.
pub fn wit_to_core_schemas(schemas: &[wit::ResourceSchema]) -> Vec<CoreResourceSchema> {
    schemas.iter().map(wit_schema_to_core).collect()
}

// -- WIT schema to core schema conversion --

fn wit_schema_to_core(s: &wit::ResourceSchema) -> CoreResourceSchema {
    CoreResourceSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .iter()
            .map(|a| (a.name.clone(), wit_attr_schema_to_core(a)))
            .collect(),
        description: s.description.clone(),
        validator: None,
        data_source: s.data_source,
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
    }
}

fn wit_attr_schema_to_core(a: &wit::AttributeSchema) -> CoreAttributeSchema {
    CoreAttributeSchema {
        name: a.name.clone(),
        attr_type: wit_attr_type_to_core(&a.attr_type),
        required: a.required,
        default: None,
        description: a.description.clone(),
        completions: None,
        provider_name: None,
        create_only: a.create_only,
        read_only: a.read_only,
        removable: None,
        block_name: None,
        write_only: a.write_only,
    }
}

fn wit_attr_type_to_core(t: &wit::AttributeType) -> CoreAttributeType {
    match t {
        wit::AttributeType::StringType => CoreAttributeType::String,
        wit::AttributeType::IntType => CoreAttributeType::Int,
        wit::AttributeType::FloatType => CoreAttributeType::Float,
        wit::AttributeType::BoolType => CoreAttributeType::Bool,
        wit::AttributeType::StringEnum(values) => CoreAttributeType::StringEnum {
            name: String::new(),
            values: values.clone(),
            namespace: None,
            to_dsl: None,
        },
        wit::AttributeType::ListType(json) => {
            // inner type is JSON-encoded as a proto::AttributeType
            if let Ok(inner) = serde_json::from_str::<proto::AttributeType>(json) {
                CoreAttributeType::List {
                    inner: Box::new(proto_attr_type_to_core(&inner)),
                    ordered: true, // default
                }
            } else {
                CoreAttributeType::List {
                    inner: Box::new(CoreAttributeType::String),
                    ordered: true,
                }
            }
        }
        wit::AttributeType::MapType(json) => {
            if let Ok(inner) = serde_json::from_str::<proto::AttributeType>(json) {
                CoreAttributeType::Map(Box::new(proto_attr_type_to_core(&inner)))
            } else {
                CoreAttributeType::Map(Box::new(CoreAttributeType::String))
            }
        }
        wit::AttributeType::StructType(def) => {
            let fields: Vec<proto::StructField> =
                serde_json::from_str(&def.fields).unwrap_or_default();
            CoreAttributeType::Struct {
                name: def.name.clone(),
                fields: fields.iter().map(proto_struct_field_to_core).collect(),
            }
        }
        wit::AttributeType::UnionType(json) => {
            if let Ok(members) = serde_json::from_str::<Vec<proto::AttributeType>>(json) {
                CoreAttributeType::Union(members.iter().map(proto_attr_type_to_core).collect())
            } else {
                CoreAttributeType::Union(vec![])
            }
        }
    }
}

fn proto_attr_type_to_core(t: &proto::AttributeType) -> CoreAttributeType {
    match t {
        proto::AttributeType::String => CoreAttributeType::String,
        proto::AttributeType::Int => CoreAttributeType::Int,
        proto::AttributeType::Float => CoreAttributeType::Float,
        proto::AttributeType::Bool => CoreAttributeType::Bool,
        proto::AttributeType::StringEnum { values } => CoreAttributeType::StringEnum {
            name: String::new(),
            values: values.clone(),
            namespace: None,
            to_dsl: None,
        },
        proto::AttributeType::List { inner, ordered } => CoreAttributeType::List {
            inner: Box::new(proto_attr_type_to_core(inner)),
            ordered: *ordered,
        },
        proto::AttributeType::Map { inner } => {
            CoreAttributeType::Map(Box::new(proto_attr_type_to_core(inner)))
        }
        proto::AttributeType::Struct { name, fields } => CoreAttributeType::Struct {
            name: name.clone(),
            fields: fields.iter().map(proto_struct_field_to_core).collect(),
        },
        proto::AttributeType::Union { members } => {
            CoreAttributeType::Union(members.iter().map(proto_attr_type_to_core).collect())
        }
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

    // -- Value roundtrips --

    #[test]
    fn test_scalar_bool_roundtrip() {
        let core = CoreValue::Bool(true);
        let wit = core_to_wit_value(&core);
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_int_roundtrip() {
        let core = CoreValue::Int(42);
        let wit = core_to_wit_value(&core);
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_float_roundtrip() {
        let core = CoreValue::Float(2.78);
        let wit = core_to_wit_value(&core);
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    #[test]
    fn test_scalar_string_roundtrip() {
        let core = CoreValue::String("hello".into());
        let wit = core_to_wit_value(&core);
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
        let wit = core_to_wit_value(&core);
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
        let wit = core_to_wit_value(&core);
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
        let wit = core_to_wit_value(&core);
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
        let wit = core_to_wit_value(&core);
        let back = wit_to_core_value(&wit);
        assert_eq!(core, back);
    }

    // -- ResourceId roundtrip --

    #[test]
    fn test_resource_id_roundtrip() {
        let core = CoreResourceId::with_provider("aws", "s3.bucket", "my-bucket");
        let wit = core_to_wit_resource_id(&core);
        assert_eq!(wit.provider, "aws");
        assert_eq!(wit.resource_type, "s3.bucket");
        assert_eq!(wit.name, "my-bucket");
        let back = wit_to_core_resource_id(&wit);
        assert_eq!(core, back);
    }

    // -- State roundtrip --

    #[test]
    fn test_state_roundtrip() {
        let id = CoreResourceId::with_provider("aws", "s3.bucket", "my-bucket");
        let mut attrs = HashMap::new();
        attrs.insert("name".into(), CoreValue::String("my-bucket".into()));
        attrs.insert("region".into(), CoreValue::String("us-east-1".into()));
        let core = CoreState::existing(id.clone(), attrs);

        let wit = core_to_wit_state(&core);
        let back = wit_to_core_state(&wit, &id);

        assert_eq!(back.id, core.id);
        assert_eq!(back.attributes, core.attributes);
        assert!(back.exists);
    }

    #[test]
    fn test_state_with_identifier_roundtrip() {
        let id = CoreResourceId::with_provider("aws", "ec2.vpc", "main");
        let attrs = HashMap::from([("cidr".into(), CoreValue::String("10.0.0.0/16".into()))]);
        let core = CoreState::existing(id.clone(), attrs).with_identifier("vpc-12345");

        let wit = core_to_wit_state(&core);
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

        let wit = core_to_wit_value_map(&map);
        let back = wit_to_core_value_map(&wit);
        assert_eq!(map, back);
    }

    // -- Resource roundtrip --

    #[test]
    fn test_resource_roundtrip() {
        let mut resource = CoreResource::with_provider("aws", "s3.bucket", "my-bucket");
        resource.attributes = HashMap::from([
            ("name".into(), Expr(CoreValue::String("my-bucket".into()))),
            ("region".into(), Expr(CoreValue::String("us-east-1".into()))),
        ]);

        let wit = core_to_wit_resource(&resource);
        assert_eq!(wit.id.provider, "aws");
        assert_eq!(wit.id.resource_type, "s3.bucket");
        assert_eq!(wit.id.name, "my-bucket");

        let back = wit_to_core_resource(&wit);
        assert_eq!(back.id, resource.id);
        // Compare resolved attributes
        assert_eq!(back.resolved_attributes(), resource.resolved_attributes());
    }

    // -- WIT type conversion tests --

    #[test]
    fn test_lifecycle_to_wit() {
        let lifecycle = LifecycleConfig {
            force_delete: true,
            create_before_destroy: false,
            prevent_destroy: true,
        };
        let wit_lc = core_to_wit_lifecycle(&lifecycle);
        assert!(wit_lc.prevent_destroy);
    }

    #[test]
    fn test_wit_provider_error_to_core() {
        let wit_err = wit::ProviderError {
            message: "something failed".to_string(),
            resource_id: Some(wit::ResourceId {
                provider: "aws".to_string(),
                resource_type: "s3.bucket".to_string(),
                name: "test".to_string(),
            }),
            is_timeout: true,
        };
        let err = wit_to_core_provider_error(&wit_err);
        assert_eq!(err.message, "something failed");
        assert!(err.is_timeout);
        assert_eq!(err.resource_id.as_ref().unwrap().provider, "aws");
    }

    #[test]
    fn test_wit_provider_info_to_tuple() {
        let info = wit::ProviderInfo {
            name: "aws".to_string(),
            display_name: "AWS Provider".to_string(),
        };
        let (name, display) = wit_to_provider_info(&info);
        assert_eq!(name, "aws");
        assert_eq!(display, "AWS Provider");
    }

    #[test]
    fn test_wit_schemas_to_core() {
        let schemas = vec![wit::ResourceSchema {
            resource_type: "s3.bucket".to_string(),
            attributes: vec![
                wit::AttributeSchema {
                    name: "name".to_string(),
                    attr_type: wit::AttributeType::StringType,
                    required: true,
                    description: Some("Bucket name".to_string()),
                    create_only: false,
                    read_only: false,
                    write_only: false,
                },
                wit::AttributeSchema {
                    name: "versioning".to_string(),
                    attr_type: wit::AttributeType::BoolType,
                    required: false,
                    description: None,
                    create_only: false,
                    read_only: false,
                    write_only: false,
                },
            ],
            description: Some("S3 Bucket".to_string()),
            data_source: false,
            name_attribute: Some("name".to_string()),
            force_replace: false,
        }];
        let core = wit_to_core_schemas(&schemas);
        assert_eq!(core.len(), 1);
        assert_eq!(core[0].resource_type, "s3.bucket");
        assert_eq!(core[0].attributes.len(), 2);
        let name_attr = core[0].attributes.get("name").expect("name attr");
        assert!(matches!(name_attr.attr_type, CoreAttributeType::String));
        assert!(name_attr.required);
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

        let wit = core_to_wit_value(&policies);
        let back = wit_to_core_value(&wit);
        assert_eq!(policies, back);
    }
}
