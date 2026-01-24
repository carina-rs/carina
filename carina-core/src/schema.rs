//! Schema - Define type schemas for resources
//!
//! Providers define schemas for each resource type,
//! enabling type validation at parse time.

use std::collections::HashMap;

use crate::resource::Value;

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
    },
    /// List
    List(Box<AttributeType>),
    /// Map
    Map(Box<AttributeType>),
}

impl AttributeType {
    /// Check if a value conforms to this type
    pub fn validate(&self, value: &Value) -> Result<(), TypeError> {
        match (self, value) {
            (AttributeType::String, Value::String(_)) => Ok(()),
            (AttributeType::Int, Value::Int(_)) => Ok(()),
            (AttributeType::Bool, Value::Bool(_)) => Ok(()),

            (AttributeType::Enum(variants), Value::String(s)) => {
                // Extract variant from "Type.variant" format
                let variant = s.split('.').last().unwrap_or(s);
                if variants.iter().any(|v| v == variant || s == v) {
                    Ok(())
                } else {
                    Err(TypeError::InvalidEnumVariant {
                        value: s.clone(),
                        expected: variants.clone(),
                    })
                }
            }

            (AttributeType::Custom { validate, .. }, v) => {
                validate(v).map_err(|msg| TypeError::ValidationFailed { message: msg })
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
        }
    }
}

/// Type error
#[derive(Debug, Clone, thiserror::Error)]
pub enum TypeError {
    #[error("Type mismatch: expected {expected}, got {got}")]
    TypeMismatch { expected: String, got: String },

    #[error("Invalid enum variant '{value}', expected one of: {}", expected.join(", "))]
    InvalidEnumVariant { value: String, expected: Vec<String> },

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
}

impl Value {
    fn type_name(&self) -> String {
        match self {
            Value::String(_) => "String".to_string(),
            Value::Int(_) => "Int".to_string(),
            Value::Bool(_) => "Bool".to_string(),
            Value::List(_) => "List".to_string(),
            Value::Map(_) => "Map".to_string(),
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
}

impl AttributeSchema {
    pub fn new(name: impl Into<String>, attr_type: AttributeType) -> Self {
        Self {
            name: name.into(),
            attr_type,
            required: false,
            default: None,
            description: None,
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
            if let Some(schema) = self.attributes.get(name) {
                if let Err(e) = schema.attr_type.validate(value) {
                    errors.push(e);
                }
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

    /// AWS region enum type
    pub fn aws_region() -> AttributeType {
        AttributeType::Enum(vec![
            "ap_northeast_1".to_string(),
            "ap_northeast_2".to_string(),
            "ap_northeast_3".to_string(),
            "ap_southeast_1".to_string(),
            "ap_southeast_2".to_string(),
            "ap_south_1".to_string(),
            "us_east_1".to_string(),
            "us_east_2".to_string(),
            "us_west_1".to_string(),
            "us_west_2".to_string(),
            "eu_west_1".to_string(),
            "eu_west_2".to_string(),
            "eu_central_1".to_string(),
        ])
    }

    /// S3 ACL enum type
    pub fn s3_acl() -> AttributeType {
        AttributeType::Enum(vec![
            "private".to_string(),
            "public_read".to_string(),
            "public_read_write".to_string(),
            "authenticated_read".to_string(),
        ])
    }

    /// S3 bucket name type (with validation)
    pub fn s3_bucket_name() -> AttributeType {
        AttributeType::Custom {
            name: "BucketName".to_string(),
            base: Box::new(AttributeType::String),
            validate: |value| {
                if let Value::String(s) = value {
                    if s.len() < 3 {
                        return Err("Bucket name must be at least 3 characters".to_string());
                    }
                    if s.len() > 63 {
                        return Err("Bucket name must be at most 63 characters".to_string());
                    }
                    if !s.chars().next().unwrap_or('_').is_ascii_lowercase()
                        && !s.chars().next().unwrap_or('_').is_ascii_digit()
                    {
                        return Err(
                            "Bucket name must start with a lowercase letter or number".to_string()
                        );
                    }
                    Ok(())
                } else {
                    Err("Expected string".to_string())
                }
            },
        }
    }

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
        }
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
    fn validate_aws_region() {
        let t = types::aws_region();
        assert!(t
            .validate(&Value::String("Region.ap_northeast_1".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("Region.invalid".to_string()))
            .is_err());
    }

    #[test]
    fn validate_bucket_name() {
        let t = types::s3_bucket_name();
        assert!(t.validate(&Value::String("my-bucket".to_string())).is_ok());
        assert!(t.validate(&Value::String("ab".to_string())).is_err()); // too short
    }

    #[test]
    fn validate_resource_schema() {
        let schema = ResourceSchema::new("bucket")
            .attribute(AttributeSchema::new("name", types::s3_bucket_name()).required())
            .attribute(AttributeSchema::new("region", types::aws_region()))
            .attribute(AttributeSchema::new("versioning", AttributeType::Bool));

        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-bucket".to_string()));
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );
        attrs.insert("versioning".to_string(), Value::Bool(true));

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
}
