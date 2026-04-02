//! Conversions between carina-core types and Wasmtime-generated WIT types.

use std::collections::HashMap;

use carina_core::resource::{
    Expr, LifecycleConfig as CoreLifecycle, Resource as CoreResource, ResourceId as CoreResourceId,
    State as CoreState, Value as CoreValue,
};
use carina_core::schema::{
    AttributeSchema as CoreAttributeSchema, AttributeType as CoreAttributeType,
    ResourceSchema as CoreResourceSchema, StructField as CoreStructField,
};

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
    }
}

pub fn wit_to_core_state(state: &wit::State, id: &CoreResourceId) -> CoreState {
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

// -- LifecycleConfig --

pub fn core_to_wit_lifecycle(_lifecycle: &CoreLifecycle) -> wit::LifecycleConfig {
    // WIT LifecycleConfig only has prevent_destroy; core LifecycleConfig has
    // force_delete and create_before_destroy. No direct mapping exists.
    wit::LifecycleConfig {
        prevent_destroy: false,
    }
}

// -- ProviderError --

pub fn wit_to_core_provider_error(e: &wit::ProviderError) -> carina_core::provider::ProviderError {
    carina_core::provider::ProviderError {
        message: e.message.clone(),
        resource_id: e.resource_id.as_ref().map(wit_to_core_resource_id),
        cause: None,
        is_timeout: e.is_timeout,
    }
}

// -- AttributeType --

/// JSON-serializable representation of an attribute type for WIT encoding.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
enum AttrTypeJson {
    #[serde(rename = "string")]
    String,
    #[serde(rename = "int")]
    Int,
    #[serde(rename = "float")]
    Float,
    #[serde(rename = "bool")]
    Bool,
    #[serde(rename = "string-enum")]
    StringEnum { values: Vec<String> },
    #[serde(rename = "list")]
    List {
        inner: Box<AttrTypeJson>,
        ordered: bool,
    },
    #[serde(rename = "map")]
    Map { inner: Box<AttrTypeJson> },
    #[serde(rename = "struct")]
    Struct {
        name: String,
        fields: Vec<StructFieldJson>,
    },
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StructFieldJson {
    name: String,
    field_type: AttrTypeJson,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_name: Option<String>,
}

fn core_attr_type_to_json(t: &CoreAttributeType) -> AttrTypeJson {
    match t {
        CoreAttributeType::String => AttrTypeJson::String,
        CoreAttributeType::Int => AttrTypeJson::Int,
        CoreAttributeType::Float => AttrTypeJson::Float,
        CoreAttributeType::Bool => AttrTypeJson::Bool,
        CoreAttributeType::StringEnum { values, .. } => AttrTypeJson::StringEnum {
            values: values.clone(),
        },
        CoreAttributeType::List { inner, ordered } => AttrTypeJson::List {
            inner: Box::new(core_attr_type_to_json(inner)),
            ordered: *ordered,
        },
        CoreAttributeType::Map(inner) => AttrTypeJson::Map {
            inner: Box::new(core_attr_type_to_json(inner)),
        },
        CoreAttributeType::Struct { name, fields } => AttrTypeJson::Struct {
            name: name.clone(),
            fields: fields.iter().map(core_struct_field_to_json).collect(),
        },
        CoreAttributeType::Custom { base, .. } => core_attr_type_to_json(base),
        CoreAttributeType::Union(_) => AttrTypeJson::String, // degrade to String
    }
}

fn json_to_core_attr_type(t: &AttrTypeJson) -> CoreAttributeType {
    match t {
        AttrTypeJson::String => CoreAttributeType::String,
        AttrTypeJson::Int => CoreAttributeType::Int,
        AttrTypeJson::Float => CoreAttributeType::Float,
        AttrTypeJson::Bool => CoreAttributeType::Bool,
        AttrTypeJson::StringEnum { values } => CoreAttributeType::StringEnum {
            name: String::new(),
            values: values.clone(),
            namespace: None,
            to_dsl: None,
        },
        AttrTypeJson::List { inner, ordered } => CoreAttributeType::List {
            inner: Box::new(json_to_core_attr_type(inner)),
            ordered: *ordered,
        },
        AttrTypeJson::Map { inner } => {
            CoreAttributeType::Map(Box::new(json_to_core_attr_type(inner)))
        }
        AttrTypeJson::Struct { name, fields } => CoreAttributeType::Struct {
            name: name.clone(),
            fields: fields.iter().map(json_to_core_struct_field).collect(),
        },
    }
}

fn core_struct_field_to_json(f: &CoreStructField) -> StructFieldJson {
    StructFieldJson {
        name: f.name.clone(),
        field_type: core_attr_type_to_json(&f.field_type),
        required: f.required,
        description: f.description.clone(),
        provider_name: f.provider_name.clone(),
        block_name: f.block_name.clone(),
    }
}

fn json_to_core_struct_field(f: &StructFieldJson) -> CoreStructField {
    CoreStructField {
        name: f.name.clone(),
        field_type: json_to_core_attr_type(&f.field_type),
        required: f.required,
        description: f.description.clone(),
        provider_name: f.provider_name.clone(),
        block_name: f.block_name.clone(),
    }
}

pub fn core_to_wit_attribute_type(t: &CoreAttributeType) -> wit::AttributeType {
    match t {
        CoreAttributeType::String => wit::AttributeType::StringType,
        CoreAttributeType::Int => wit::AttributeType::IntType,
        CoreAttributeType::Float => wit::AttributeType::FloatType,
        CoreAttributeType::Bool => wit::AttributeType::BoolType,
        CoreAttributeType::StringEnum { values, .. } => {
            wit::AttributeType::StringEnum(values.clone())
        }
        CoreAttributeType::List { inner, ordered } => {
            let json = AttrTypeJson::List {
                inner: Box::new(core_attr_type_to_json(inner)),
                ordered: *ordered,
            };
            wit::AttributeType::ListType(serde_json::to_string(&json).unwrap())
        }
        CoreAttributeType::Map(inner) => {
            let json = AttrTypeJson::Map {
                inner: Box::new(core_attr_type_to_json(inner)),
            };
            wit::AttributeType::MapType(serde_json::to_string(&json).unwrap())
        }
        CoreAttributeType::Struct { name, fields } => {
            let fields_json: Vec<StructFieldJson> =
                fields.iter().map(core_struct_field_to_json).collect();
            wit::AttributeType::StructType(wit::StructDef {
                name: name.clone(),
                fields: serde_json::to_string(&fields_json).unwrap(),
            })
        }
        CoreAttributeType::Custom { base, .. } => core_to_wit_attribute_type(base),
        CoreAttributeType::Union(_) => wit::AttributeType::StringType,
    }
}

pub fn wit_to_core_attribute_type(t: &wit::AttributeType) -> CoreAttributeType {
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
            if let Ok(attr_json) = serde_json::from_str::<AttrTypeJson>(json) {
                json_to_core_attr_type(&attr_json)
            } else {
                // Fallback: treat the JSON string as the inner type descriptor
                CoreAttributeType::list(CoreAttributeType::String)
            }
        }
        wit::AttributeType::MapType(json) => {
            if let Ok(attr_json) = serde_json::from_str::<AttrTypeJson>(json) {
                json_to_core_attr_type(&attr_json)
            } else {
                CoreAttributeType::Map(Box::new(CoreAttributeType::String))
            }
        }
        wit::AttributeType::StructType(struct_def) => {
            let fields: Vec<StructFieldJson> =
                serde_json::from_str(&struct_def.fields).unwrap_or_default();
            CoreAttributeType::Struct {
                name: struct_def.name.clone(),
                fields: fields.iter().map(json_to_core_struct_field).collect(),
            }
        }
    }
}

// -- Schema --

fn core_to_wit_attribute_schema(a: &CoreAttributeSchema) -> wit::AttributeSchema {
    wit::AttributeSchema {
        name: a.name.clone(),
        attr_type: core_to_wit_attribute_type(&a.attr_type),
        required: a.required,
        description: a.description.clone(),
        create_only: a.create_only,
        read_only: a.read_only,
        write_only: a.write_only,
    }
}

fn wit_to_core_attribute_schema(a: &wit::AttributeSchema) -> CoreAttributeSchema {
    CoreAttributeSchema {
        name: a.name.clone(),
        attr_type: wit_to_core_attribute_type(&a.attr_type),
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

pub fn core_to_wit_schema(s: &CoreResourceSchema) -> wit::ResourceSchema {
    wit::ResourceSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .values()
            .map(core_to_wit_attribute_schema)
            .collect(),
        description: s.description.clone(),
        data_source: s.data_source,
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
    }
}

pub fn wit_to_core_schema(s: &wit::ResourceSchema) -> CoreResourceSchema {
    CoreResourceSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .iter()
            .map(|a| (a.name.clone(), wit_to_core_attribute_schema(a)))
            .collect(),
        description: s.description.clone(),
        validator: None,
        data_source: s.data_source,
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
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

    // -- Schema with basic string attribute --

    #[test]
    fn test_schema_basic_string_roundtrip() {
        let core_schema = CoreResourceSchema {
            resource_type: "ec2.vpc".into(),
            attributes: HashMap::from([(
                "cidr_block".into(),
                CoreAttributeSchema {
                    name: "cidr_block".into(),
                    attr_type: CoreAttributeType::String,
                    required: true,
                    default: None,
                    description: Some("CIDR block".into()),
                    completions: None,
                    provider_name: None,
                    create_only: true,
                    read_only: false,
                    removable: None,
                    block_name: None,
                    write_only: false,
                },
            )]),
            description: Some("VPC".into()),
            validator: None,
            data_source: false,
            name_attribute: None,
            force_replace: false,
        };

        let wit = core_to_wit_schema(&core_schema);
        assert_eq!(wit.resource_type, "ec2.vpc");
        assert_eq!(wit.attributes.len(), 1);

        let back = wit_to_core_schema(&wit);
        assert_eq!(back.resource_type, "ec2.vpc");
        let attr = &back.attributes["cidr_block"];
        assert_eq!(attr.name, "cidr_block");
        assert!(attr.required);
        assert!(attr.create_only);
        assert_eq!(attr.description, Some("CIDR block".into()));
    }

    // -- Schema with enum attribute --

    #[test]
    fn test_schema_enum_roundtrip() {
        let core_type = CoreAttributeType::StringEnum {
            name: "VersioningStatus".into(),
            values: vec!["Enabled".into(), "Suspended".into()],
            namespace: Some("aws.s3".into()),
            to_dsl: None,
        };

        let wit = core_to_wit_attribute_type(&core_type);
        if let wit::AttributeType::StringEnum(values) = &wit {
            assert_eq!(values, &["Enabled", "Suspended"]);
        } else {
            panic!("Expected StringEnum");
        }

        let back = wit_to_core_attribute_type(&wit);
        if let CoreAttributeType::StringEnum { values, .. } = &back {
            assert_eq!(values, &["Enabled", "Suspended"]);
        } else {
            panic!("Expected StringEnum");
        }
    }

    // -- Schema with list attribute type --

    #[test]
    fn test_schema_list_type_roundtrip() {
        let core_type = CoreAttributeType::List {
            inner: Box::new(CoreAttributeType::String),
            ordered: true,
        };

        let wit = core_to_wit_attribute_type(&core_type);
        assert!(matches!(wit, wit::AttributeType::ListType(_)));

        let back = wit_to_core_attribute_type(&wit);
        if let CoreAttributeType::List { inner, ordered } = &back {
            assert!(matches!(inner.as_ref(), CoreAttributeType::String));
            assert!(*ordered);
        } else {
            panic!("Expected List type");
        }
    }

    #[test]
    fn test_schema_unordered_list_roundtrip() {
        let core_type = CoreAttributeType::List {
            inner: Box::new(CoreAttributeType::Int),
            ordered: false,
        };

        let wit = core_to_wit_attribute_type(&core_type);
        let back = wit_to_core_attribute_type(&wit);
        if let CoreAttributeType::List { ordered, .. } = &back {
            assert!(!ordered, "ordered=false must survive roundtrip");
        } else {
            panic!("Expected List type");
        }
    }

    // -- Schema with struct attribute type --

    #[test]
    fn test_schema_struct_type_roundtrip() {
        let core_type = CoreAttributeType::Struct {
            name: "Tag".into(),
            fields: vec![
                CoreStructField {
                    name: "key".into(),
                    field_type: CoreAttributeType::String,
                    required: true,
                    description: Some("Tag key".into()),
                    provider_name: Some("Key".into()),
                    block_name: None,
                },
                CoreStructField {
                    name: "value".into(),
                    field_type: CoreAttributeType::String,
                    required: false,
                    description: None,
                    provider_name: Some("Value".into()),
                    block_name: None,
                },
            ],
        };

        let wit = core_to_wit_attribute_type(&core_type);
        if let wit::AttributeType::StructType(struct_def) = &wit {
            assert_eq!(struct_def.name, "Tag");
        } else {
            panic!("Expected StructType");
        }

        let back = wit_to_core_attribute_type(&wit);
        if let CoreAttributeType::Struct { name, fields } = &back {
            assert_eq!(name, "Tag");
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "key");
            assert!(fields[0].required);
            assert_eq!(fields[0].description, Some("Tag key".into()));
            assert_eq!(fields[0].provider_name, Some("Key".into()));
            assert_eq!(fields[1].name, "value");
            assert!(!fields[1].required);
        } else {
            panic!("Expected Struct type");
        }
    }

    // -- ProviderError conversion --

    #[test]
    fn test_provider_error_conversion() {
        let wit_err = wit::ProviderError {
            message: "something failed".into(),
            resource_id: Some(wit::ResourceId {
                provider: "aws".into(),
                resource_type: "s3.bucket".into(),
                name: "test".into(),
            }),
            is_timeout: true,
        };

        let core_err = wit_to_core_provider_error(&wit_err);
        assert_eq!(core_err.message, "something failed");
        assert!(core_err.is_timeout);
        assert_eq!(core_err.resource_id.as_ref().unwrap().provider, "aws");
    }

    // -- Custom type degrades to base --

    #[test]
    fn test_custom_type_degrades_to_base() {
        let core_type = CoreAttributeType::Custom {
            name: "Region".into(),
            base: Box::new(CoreAttributeType::String),
            validate: |_| Ok(()),
            namespace: None,
            to_dsl: None,
        };

        let wit = core_to_wit_attribute_type(&core_type);
        assert!(matches!(wit, wit::AttributeType::StringType));
    }

    // -- Union degrades to String --

    #[test]
    fn test_union_degrades_to_string() {
        let core_type =
            CoreAttributeType::Union(vec![CoreAttributeType::String, CoreAttributeType::Int]);

        let wit = core_to_wit_attribute_type(&core_type);
        assert!(matches!(wit, wit::AttributeType::StringType));
    }

    // -- Map type roundtrip --

    #[test]
    fn test_schema_map_type_roundtrip() {
        let core_type = CoreAttributeType::Map(Box::new(CoreAttributeType::String));

        let wit = core_to_wit_attribute_type(&core_type);
        assert!(matches!(wit, wit::AttributeType::MapType(_)));

        let back = wit_to_core_attribute_type(&wit);
        if let CoreAttributeType::Map(inner) = &back {
            assert!(matches!(inner.as_ref(), CoreAttributeType::String));
        } else {
            panic!("Expected Map type");
        }
    }
}
