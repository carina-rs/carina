//! Serializable protocol types for host-guest communication.
//!
//! These mirror carina-core types but are JSON-serializable across the process boundary.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Mirrors `carina_core::resource::ResourceId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    pub provider: String,
    pub resource_type: String,
    pub name: String,
}

/// Mirrors `carina_core::resource::Value`.
///
/// Only includes variants that can cross the process boundary.
/// `ResourceRef`, `Interpolation`, `FunctionCall`, `Closure` are resolved
/// before reaching the provider, so they are excluded.
///
/// Custom Serialize/Deserialize to produce untagged JSON (strings as `"hello"`,
/// ints as `42`, etc.) without hitting serde's recursive monomorphization limit.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Map(HashMap<String, Value>),
}

impl Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Int(i) => serializer.serialize_i64(*i),
            Value::Float(f) => serializer.serialize_f64(*f),
            Value::String(s) => serializer.serialize_str(s),
            Value::List(l) => {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(l.len()))?;
                for v in l {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Value::Map(m) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(m.len()))?;
                for (k, v) in m {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let json = serde_json::Value::deserialize(deserializer)?;
        Ok(Value::from_json(json))
    }
}

impl Value {
    fn from_json(json: serde_json::Value) -> Self {
        match json {
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else {
                    Value::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Value::String(s),
            serde_json::Value::Array(a) => {
                Value::List(a.into_iter().map(Value::from_json).collect())
            }
            serde_json::Value::Object(m) => Value::Map(
                m.into_iter()
                    .map(|(k, v)| (k, Value::from_json(v)))
                    .collect(),
            ),
            serde_json::Value::Null => Value::String(String::new()),
        }
    }
}

/// Mirrors `carina_core::resource::State`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub id: ResourceId,
    pub identifier: Option<String>,
    pub attributes: HashMap<String, Value>,
    pub exists: bool,
}

/// Mirrors `carina_core::resource::LifecycleConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LifecycleConfig {
    pub force_delete: bool,
    pub create_before_destroy: bool,
}

/// Simplified resource for the process boundary.
/// Attributes are pre-resolved `Value`s, not `Expr`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    pub attributes: HashMap<String, Value>,
    pub lifecycle: LifecycleConfig,
}

/// Provider metadata returned by `provider_info`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub display_name: String,
}

/// Provider error returned from operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<ResourceId>,
    #[serde(default)]
    pub is_timeout: bool,
}

/// Schema types for resource validation and completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSchema {
    pub resource_type: String,
    pub attributes: HashMap<String, AttributeSchema>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub data_source: bool,
    #[serde(default)]
    pub name_attribute: Option<String>,
    #[serde(default)]
    pub force_replace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributeSchema {
    pub name: String,
    pub attr_type: AttributeType,
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub create_only: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub write_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AttributeType {
    String,
    Int,
    Float,
    Bool,
    #[serde(rename = "string_enum")]
    StringEnum {
        values: Vec<String>,
    },
    #[serde(rename = "list")]
    List {
        inner: Box<AttributeType>,
    },
    #[serde(rename = "map")]
    Map {
        inner: Box<AttributeType>,
    },
    #[serde(rename = "struct")]
    Struct {
        name: String,
        fields: Vec<StructField>,
    },
    #[serde(rename = "union")]
    Union {
        members: Vec<AttributeType>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructField {
    pub name: String,
    pub field_type: AttributeType,
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_roundtrip() {
        let values = vec![
            Value::String("hello".into()),
            Value::Int(42),
            Value::Float(2.71),
            Value::Bool(true),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::Map(HashMap::from([("key".into(), Value::String("val".into()))])),
        ];

        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            let back: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value, back);
        }
    }

    #[test]
    fn test_state_roundtrip() {
        let state = State {
            id: ResourceId {
                provider: "mock".into(),
                resource_type: "test.resource".into(),
                name: "my-resource".into(),
            },
            identifier: Some("mock-id".into()),
            attributes: HashMap::from([("name".into(), Value::String("test".into()))]),
            exists: true,
        };

        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(state.id, back.id);
        assert_eq!(state.identifier, back.identifier);
        assert_eq!(state.exists, back.exists);
    }

    #[test]
    fn test_attribute_type_roundtrip() {
        let attr = AttributeType::Struct {
            name: "Config".into(),
            fields: vec![StructField {
                name: "enabled".into(),
                field_type: AttributeType::Bool,
                required: true,
                description: None,
                block_name: None,
            }],
        };

        let json = serde_json::to_string(&attr).unwrap();
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }

    #[test]
    fn test_union_type_roundtrip() {
        let attr = AttributeType::Union {
            members: vec![
                AttributeType::Struct {
                    name: "IamPolicyPrincipal".into(),
                    fields: vec![StructField {
                        name: "service".into(),
                        field_type: AttributeType::String,
                        required: false,
                        description: None,
                        block_name: None,
                    }],
                },
                AttributeType::String,
            ],
        };

        let json = serde_json::to_string(&attr).unwrap();
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }
}
