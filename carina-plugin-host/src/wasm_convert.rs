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
                CoreResourceId::with_provider(&pid.provider, &pid.resource_type, &pid.name)
            }),
            cause: None,
            is_timeout: proto_err.is_timeout,
        }
    } else {
        carina_core::provider::ProviderError {
            message: json.to_string(),
            resource_id: None,
            cause: None,
            is_timeout: false,
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
        validator: None,
        data_source: s.data_source,
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
    }
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
        let json = r#"{"message":"something failed","resource_id":{"provider":"aws","resource_type":"s3.bucket","name":"test"},"is_timeout":true}"#;
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
            "resource_type": "ec2.security_group",
            "description": "EC2 Security Group",
            "data_source": false,
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
        assert_eq!(schema.resource_type, "ec2.security_group");
        assert_eq!(schema.description.as_deref(), Some("EC2 Security Group"));
        assert!(!schema.data_source);
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

        let wit = core_to_wit_value(&policies);
        let back = wit_to_core_value(&wit);
        assert_eq!(policies, back);
    }
}
