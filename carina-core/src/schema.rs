//! Schema - Define type schemas for resources
//!
//! Providers define schemas for each resource type,
//! enabling type validation at parse time.

use std::collections::HashMap;
use std::fmt;

use crate::resource::Value;

/// A field within a Struct type
#[derive(Debug, Clone)]
pub struct StructField {
    /// Field name (snake_case, e.g., "ip_protocol")
    pub name: String,
    /// Field type
    pub field_type: AttributeType,
    /// Whether this field is required
    pub required: bool,
    /// Description of this field
    pub description: Option<String>,
    /// Provider-side property name (e.g., "IpProtocol")
    pub provider_name: Option<String>,
}

impl StructField {
    pub fn new(name: impl Into<String>, field_type: AttributeType) -> Self {
        Self {
            name: name.into(),
            field_type,
            required: false,
            description: None,
            provider_name: None,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = Some(name.into());
        self
    }
}

/// Attribute type
#[derive(Debug, Clone)]
pub enum AttributeType {
    /// String
    String,
    /// Integer
    Int,
    /// Boolean
    Bool,
    /// Enum (list of allowed values)
    Enum(Vec<String>),
    /// Custom type (with validation function)
    Custom {
        name: String,
        base: Box<AttributeType>,
        validate: fn(&Value) -> Result<(), String>,
        /// Namespace for resolving shorthand enum values (e.g., "aws.vpc")
        /// When set, allows `dedicated` to be resolved to `aws.vpc.InstanceTenancy.dedicated`
        namespace: Option<String>,
    },
    /// List
    List(Box<AttributeType>),
    /// Map
    Map(Box<AttributeType>),
    /// Struct (named object with typed fields)
    Struct {
        name: String,
        fields: Vec<StructField>,
    },
}

impl AttributeType {
    /// Check if a value conforms to this type
    pub fn validate(&self, value: &Value) -> Result<(), TypeError> {
        match (self, value) {
            // ResourceRef values resolve to strings at runtime, so they're valid for String types
            (AttributeType::String, Value::String(_) | Value::ResourceRef(_, _)) => Ok(()),
            (AttributeType::Int, Value::Int(_)) => Ok(()),
            (AttributeType::Bool, Value::Bool(_)) => Ok(()),

            (AttributeType::Enum(variants), Value::String(s)) => {
                // Extract variant from "Type.variant" format
                let variant = s.split('.').next_back().unwrap_or(s);
                if variants.iter().any(|v| v == variant || s == v) {
                    Ok(())
                } else {
                    Err(TypeError::InvalidEnumVariant {
                        value: s.clone(),
                        expected: variants.clone(),
                    })
                }
            }

            (
                AttributeType::Custom {
                    validate,
                    name,
                    namespace,
                    ..
                },
                v,
            ) => {
                // Handle UnresolvedIdent by expanding to full namespace format
                let resolved_value = match v {
                    Value::UnresolvedIdent(ident, member) => {
                        let expanded = match (namespace, member) {
                            // TypeName.value -> namespace.TypeName.value
                            (Some(ns), Some(m)) if ident == name => {
                                format!("{}.{}.{}", ns, ident, m)
                            }
                            // SomeOther.value with namespace -> namespace.TypeName.SomeOther.value
                            // This is an error case, but let validation handle it
                            (Some(_ns), Some(m)) => {
                                format!("{}.{}", ident, m)
                            }
                            // value -> namespace.TypeName.value
                            (Some(ns), None) => {
                                format!("{}.{}.{}", ns, name, ident)
                            }
                            // No namespace, keep as-is for validation
                            (None, Some(m)) => format!("{}.{}", ident, m),
                            (None, None) => ident.clone(),
                        };
                        Value::String(expanded)
                    }
                    _ => v.clone(),
                };
                validate(&resolved_value)
                    .map_err(|msg| TypeError::ValidationFailed { message: msg })
            }

            (AttributeType::List(inner), Value::List(items)) => {
                for (i, item) in items.iter().enumerate() {
                    inner.validate(item).map_err(|e| TypeError::ListItemError {
                        index: i,
                        inner: Box::new(e),
                    })?;
                }
                Ok(())
            }

            (AttributeType::Map(inner), Value::Map(map)) => {
                for (k, v) in map {
                    inner.validate(v).map_err(|e| TypeError::MapValueError {
                        key: k.clone(),
                        inner: Box::new(e),
                    })?;
                }
                Ok(())
            }

            (AttributeType::Struct { fields, .. }, Value::Map(map)) => {
                // Check required fields
                for field in fields {
                    if field.required && !map.contains_key(&field.name) {
                        return Err(TypeError::StructFieldError {
                            field: field.name.clone(),
                            inner: Box::new(TypeError::MissingRequired {
                                name: field.name.clone(),
                            }),
                        });
                    }
                }
                // Type-check each field value
                let field_map: std::collections::HashMap<&str, &StructField> =
                    fields.iter().map(|f| (f.name.as_str(), f)).collect();
                for (k, v) in map {
                    if let Some(field) = field_map.get(k.as_str()) {
                        field
                            .field_type
                            .validate(v)
                            .map_err(|e| TypeError::StructFieldError {
                                field: k.clone(),
                                inner: Box::new(e),
                            })?;
                    }
                    // Unknown fields are allowed (for flexibility)
                }
                Ok(())
            }

            _ => Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name(),
            }),
        }
    }

    fn type_name(&self) -> String {
        match self {
            AttributeType::String => "String".to_string(),
            AttributeType::Int => "Int".to_string(),
            AttributeType::Bool => "Bool".to_string(),
            AttributeType::Enum(variants) => format!("Enum({})", variants.join(" | ")),
            AttributeType::Custom { name, .. } => name.clone(),
            AttributeType::List(inner) => format!("List<{}>", inner.type_name()),
            AttributeType::Map(inner) => format!("Map<{}>", inner.type_name()),
            AttributeType::Struct { name, .. } => format!("Struct({})", name),
        }
    }
}

impl fmt::Display for AttributeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.type_name())
    }
}

/// Type error
#[derive(Debug, Clone, thiserror::Error)]
pub enum TypeError {
    #[error("Type mismatch: expected {expected}, got {got}")]
    TypeMismatch { expected: String, got: String },

    #[error("Invalid enum variant '{value}', expected one of: {}", expected.join(", "))]
    InvalidEnumVariant {
        value: String,
        expected: Vec<String>,
    },

    #[error("Validation failed: {message}")]
    ValidationFailed { message: String },

    #[error("Required attribute '{name}' is missing")]
    MissingRequired { name: String },

    #[error("Unknown attribute '{name}'")]
    UnknownAttribute { name: String },

    #[error("List item at index {index}: {inner}")]
    ListItemError { index: usize, inner: Box<TypeError> },

    #[error("Map value for key '{key}': {inner}")]
    MapValueError { key: String, inner: Box<TypeError> },

    #[error("Struct field '{field}': {inner}")]
    StructFieldError {
        field: String,
        inner: Box<TypeError>,
    },
}

impl Value {
    fn type_name(&self) -> String {
        match self {
            Value::String(_) => "String".to_string(),
            Value::Int(_) => "Int".to_string(),
            Value::Bool(_) => "Bool".to_string(),
            Value::List(_) => "List".to_string(),
            Value::Map(_) => "Map".to_string(),
            Value::ResourceRef(binding, attr) => format!("ResourceRef({}.{})", binding, attr),
            Value::TypedResourceRef {
                binding_name,
                attribute_name,
                ..
            } => format!("TypedResourceRef({}.{})", binding_name, attribute_name),
            Value::UnresolvedIdent(name, member) => match member {
                Some(m) => format!("UnresolvedIdent({}.{})", name, m),
                None => format!("UnresolvedIdent({})", name),
            },
        }
    }
}

/// Completion value for LSP completions
#[derive(Debug, Clone)]
pub struct CompletionValue {
    /// The value to insert (e.g., "aws.vpc.InstanceTenancy.default")
    pub value: String,
    /// Description shown in completion popup
    pub description: String,
}

impl CompletionValue {
    pub fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: description.into(),
        }
    }
}

/// Attribute schema
#[derive(Debug, Clone)]
pub struct AttributeSchema {
    pub name: String,
    pub attr_type: AttributeType,
    pub required: bool,
    pub default: Option<Value>,
    pub description: Option<String>,
    /// Completion values for this attribute (used by LSP)
    pub completions: Option<Vec<CompletionValue>>,
    /// Provider-side property name (e.g., "VpcId" for AWS Cloud Control)
    pub provider_name: Option<String>,
}

impl AttributeSchema {
    pub fn new(name: impl Into<String>, attr_type: AttributeType) -> Self {
        Self {
            name: name.into(),
            attr_type,
            required: false,
            default: None,
            description: None,
            completions: None,
            provider_name: None,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn with_default(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_completions(mut self, completions: Vec<CompletionValue>) -> Self {
        self.completions = Some(completions);
        self
    }

    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = Some(name.into());
        self
    }
}

/// Resource schema
#[derive(Debug, Clone)]
pub struct ResourceSchema {
    pub resource_type: String,
    pub attributes: HashMap<String, AttributeSchema>,
    pub description: Option<String>,
}

impl ResourceSchema {
    pub fn new(resource_type: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            attributes: HashMap::new(),
            description: None,
        }
    }

    pub fn attribute(mut self, schema: AttributeSchema) -> Self {
        self.attributes.insert(schema.name.clone(), schema);
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Validate resource attributes
    pub fn validate(&self, attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
        let mut errors = Vec::new();

        // Check required attributes
        for (name, schema) in &self.attributes {
            if schema.required && !attributes.contains_key(name) && schema.default.is_none() {
                errors.push(TypeError::MissingRequired { name: name.clone() });
            }
        }

        // Type check each attribute
        for (name, value) in attributes {
            if let Some(schema) = self.attributes.get(name)
                && let Err(e) = schema.attr_type.validate(value)
            {
                errors.push(e);
            }
            // Unknown attributes are allowed (for flexibility)
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Helper functions for common types
pub mod types {
    use super::*;

    /// Positive integer type
    pub fn positive_int() -> AttributeType {
        AttributeType::Custom {
            name: "PositiveInt".to_string(),
            base: Box::new(AttributeType::Int),
            validate: |value| {
                if let Value::Int(n) = value {
                    if *n > 0 {
                        Ok(())
                    } else {
                        Err("Value must be positive".to_string())
                    }
                } else {
                    Err("Expected integer".to_string())
                }
            },
            namespace: None,
        }
    }

    /// CIDR block type (e.g., "10.0.0.0/16")
    pub fn cidr() -> AttributeType {
        AttributeType::Custom {
            name: "Cidr".to_string(),
            base: Box::new(AttributeType::String),
            validate: |value| {
                if let Value::String(s) = value {
                    validate_cidr(s)
                } else {
                    Err("Expected string".to_string())
                }
            },
            namespace: None,
        }
    }
}

/// Validate CIDR block format (e.g., "10.0.0.0/16")
pub fn validate_cidr(cidr: &str) -> Result<(), String> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid CIDR format '{}': expected IP/prefix",
            cidr
        ));
    }

    let ip = parts[0];
    let prefix = parts[1];

    // Validate IP address
    let octets: Vec<&str> = ip.split('.').collect();
    if octets.len() != 4 {
        return Err(format!("Invalid IP address '{}': expected 4 octets", ip));
    }

    for octet in &octets {
        match octet.parse::<u8>() {
            Ok(_) => {}
            Err(_) => {
                return Err(format!(
                    "Invalid octet '{}' in IP address: must be 0-255",
                    octet
                ));
            }
        }
    }

    // Validate prefix length
    match prefix.parse::<u8>() {
        Ok(p) if p <= 32 => Ok(()),
        Ok(p) => Err(format!("Invalid prefix length '{}': must be 0-32", p)),
        Err(_) => Err(format!(
            "Invalid prefix length '{}': must be a number",
            prefix
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_string_type() {
        let t = AttributeType::String;
        assert!(t.validate(&Value::String("hello".to_string())).is_ok());
        assert!(t.validate(&Value::Int(42)).is_err());
    }

    #[test]
    fn validate_enum_type() {
        let t = AttributeType::Enum(vec!["a".to_string(), "b".to_string()]);
        assert!(t.validate(&Value::String("a".to_string())).is_ok());
        assert!(t.validate(&Value::String("Type.a".to_string())).is_ok());
        assert!(t.validate(&Value::String("c".to_string())).is_err());
    }

    #[test]
    fn validate_positive_int() {
        let t = types::positive_int();
        assert!(t.validate(&Value::Int(1)).is_ok());
        assert!(t.validate(&Value::Int(100)).is_ok());
        assert!(t.validate(&Value::Int(0)).is_err());
        assert!(t.validate(&Value::Int(-1)).is_err());
    }

    #[test]
    fn validate_resource_schema() {
        let schema = ResourceSchema::new("resource")
            .attribute(AttributeSchema::new("name", AttributeType::String).required())
            .attribute(AttributeSchema::new("count", types::positive_int()))
            .attribute(AttributeSchema::new("enabled", AttributeType::Bool));

        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-resource".to_string()));
        attrs.insert("count".to_string(), Value::Int(5));
        attrs.insert("enabled".to_string(), Value::Bool(true));

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn missing_required_attribute() {
        let schema = ResourceSchema::new("bucket")
            .attribute(AttributeSchema::new("name", AttributeType::String).required());

        let attrs = HashMap::new();
        let result = schema.validate(&attrs);
        assert!(result.is_err());
    }

    #[test]
    fn validate_cidr_type() {
        let t = types::cidr();

        // Valid CIDRs
        assert!(
            t.validate(&Value::String("10.0.0.0/16".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("192.168.1.0/24".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("0.0.0.0/0".to_string())).is_ok());
        assert!(
            t.validate(&Value::String("255.255.255.255/32".to_string()))
                .is_ok()
        );

        // Invalid CIDRs
        assert!(t.validate(&Value::String("10.0.0.0".to_string())).is_err()); // no prefix
        assert!(
            t.validate(&Value::String("10.0.0.0/33".to_string()))
                .is_err()
        ); // prefix too large
        assert!(
            t.validate(&Value::String("10.0.0.256/16".to_string()))
                .is_err()
        ); // octet > 255
        assert!(t.validate(&Value::String("10.0.0/16".to_string())).is_err()); // only 3 octets
        assert!(t.validate(&Value::String("invalid".to_string())).is_err()); // not a CIDR
        assert!(t.validate(&Value::Int(42)).is_err()); // wrong type
    }

    #[test]
    fn validate_struct_type() {
        let t = AttributeType::Struct {
            name: "Ingress".to_string(),
            fields: vec![
                StructField::new("ip_protocol", AttributeType::String).required(),
                StructField::new("from_port", AttributeType::Int),
                StructField::new("to_port", AttributeType::Int),
            ],
        };

        // Valid: all required fields present
        let mut map = HashMap::new();
        map.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        map.insert("from_port".to_string(), Value::Int(80));
        assert!(t.validate(&Value::Map(map)).is_ok());

        // Invalid: missing required field
        let empty_map = HashMap::new();
        assert!(t.validate(&Value::Map(empty_map)).is_err());

        // Invalid: wrong type for field
        let mut bad_map = HashMap::new();
        bad_map.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        bad_map.insert(
            "from_port".to_string(),
            Value::String("not_a_number".to_string()),
        );
        assert!(t.validate(&Value::Map(bad_map)).is_err());

        // Invalid: not a Map
        assert!(
            t.validate(&Value::String("not a struct".to_string()))
                .is_err()
        );
    }

    #[test]
    fn validate_list_of_struct() {
        let struct_type = AttributeType::Struct {
            name: "Ingress".to_string(),
            fields: vec![StructField::new("ip_protocol", AttributeType::String).required()],
        };
        let list_type = AttributeType::List(Box::new(struct_type));

        let mut item = HashMap::new();
        item.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        let list = Value::List(vec![Value::Map(item)]);
        assert!(list_type.validate(&list).is_ok());

        // Invalid item in list
        let bad_list = Value::List(vec![Value::Map(HashMap::new())]);
        assert!(list_type.validate(&bad_list).is_err());
    }
}
