//! Schema - Define type schemas for resources
//!
//! Providers define schemas for each resource type,
//! enabling type validation at parse time.

use std::collections::HashMap;
use std::fmt;

use crate::resource::Value;

/// Type alias for resource validator functions
pub type ResourceValidator = fn(&HashMap<String, Value>) -> Result<(), Vec<TypeError>>;

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
        /// Optional callback to normalize AWS values to DSL format.
        /// For example, availability_zone uses `|s| s.replace('-', "_")` to convert
        /// "ap-northeast-1a" to "ap_northeast_1a" for DSL identifier form.
        to_dsl: Option<fn(&str) -> String>,
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
                // ResourceRef values resolve to strings at runtime, so they're valid for Custom types
                if matches!(v, Value::ResourceRef(_, _) | Value::TypedResourceRef { .. }) {
                    return Ok(());
                }
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

            (AttributeType::Struct { name, fields }, Value::Map(map)) => {
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
                let field_names: Vec<&str> = field_map.keys().copied().collect();
                for (k, v) in map {
                    if let Some(field) = field_map.get(k.as_str()) {
                        field
                            .field_type
                            .validate(v)
                            .map_err(|e| TypeError::StructFieldError {
                                field: k.clone(),
                                inner: Box::new(e),
                            })?;
                    } else {
                        let suggestion = suggest_similar_name(k, &field_names);
                        return Err(TypeError::UnknownStructField {
                            struct_name: name.clone(),
                            field: k.clone(),
                            suggestion,
                        });
                    }
                }
                Ok(())
            }

            _ => Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name(),
            }),
        }
    }

    pub fn type_name(&self) -> String {
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

    #[error("Unknown field '{field}' in {struct_name}{}", suggestion.as_ref().map(|s| format!(", did you mean '{}'?", s)).unwrap_or_default())]
    UnknownStructField {
        struct_name: String,
        field: String,
        suggestion: Option<String>,
    },

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

/// Common validation patterns for resource schemas
pub mod validators {
    use super::*;

    /// Helper function to validate that exactly one of the specified fields is present.
    /// Returns `Ok(())` if exactly one field is present, `Err` otherwise.
    ///
    /// Use this in custom validator functions for mutually exclusive required fields.
    ///
    /// # Example
    /// ```
    /// use std::collections::HashMap;
    /// use carina_core::resource::Value;
    /// use carina_core::schema::{validators, TypeError};
    ///
    /// fn my_validator(attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
    ///     validators::validate_exclusive_required(attributes, &["option_a", "option_b"])
    /// }
    /// ```
    pub fn validate_exclusive_required(
        attributes: &HashMap<String, Value>,
        fields: &[&str],
    ) -> Result<(), Vec<TypeError>> {
        let present_fields: Vec<&str> = fields
            .iter()
            .filter(|&&name| attributes.contains_key(name))
            .copied()
            .collect();

        match present_fields.len() {
            0 => Err(vec![TypeError::ValidationFailed {
                message: format!("Exactly one of [{}] must be specified", fields.join(", ")),
            }]),
            1 => Ok(()),
            _ => Err(vec![TypeError::ValidationFailed {
                message: format!(
                    "Only one of [{}] can be specified, but found: {}",
                    fields.join(", "),
                    present_fields.join(", ")
                ),
            }]),
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
    /// Whether this attribute is create-only (immutable after creation)
    pub create_only: bool,
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
            create_only: false,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn create_only(mut self) -> Self {
        self.create_only = true;
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
    /// Optional validator function for cross-attribute validation
    /// (e.g., mutually exclusive required fields)
    pub validator: Option<ResourceValidator>,
}

impl ResourceSchema {
    pub fn new(resource_type: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            attributes: HashMap::new(),
            description: None,
            validator: None,
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

    pub fn with_validator(mut self, validator: ResourceValidator) -> Self {
        self.validator = Some(validator);
        self
    }

    /// Returns the names of create-only (immutable) attributes
    pub fn create_only_attributes(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.create_only)
            .map(|(name, _)| name.as_str())
            .collect()
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

        // Run custom validator if present
        if let Some(validator) = self.validator
            && let Err(mut validation_errors) = validator(attributes)
        {
            errors.append(&mut validation_errors);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Provider-agnostic types only. AWS-specific types (arn, aws_resource_id,
/// availability_zone, etc.) belong in provider crates.
/// See carina-provider-awscc/src/schemas/generated/mod.rs for AWS types.
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
            to_dsl: None,
        }
    }

    /// IPv4 CIDR block type (e.g., "10.0.0.0/16")
    pub fn ipv4_cidr() -> AttributeType {
        AttributeType::Custom {
            name: "Ipv4Cidr".to_string(),
            base: Box::new(AttributeType::String),
            validate: |value| {
                if let Value::String(s) = value {
                    validate_ipv4_cidr(s)
                } else {
                    Err("Expected string".to_string())
                }
            },
            namespace: None,
            to_dsl: None,
        }
    }

    /// CIDR block type — alias for `ipv4_cidr()` for backward compatibility
    pub fn cidr() -> AttributeType {
        AttributeType::Custom {
            name: "Cidr".to_string(),
            base: Box::new(AttributeType::String),
            validate: |value| {
                if let Value::String(s) = value {
                    validate_ipv4_cidr(s)
                } else {
                    Err("Expected string".to_string())
                }
            },
            namespace: None,
            to_dsl: None,
        }
    }

    /// IPv4 address type (e.g., "10.0.1.5", "192.168.0.1")
    pub fn ipv4_address() -> AttributeType {
        AttributeType::Custom {
            name: "Ipv4Address".to_string(),
            base: Box::new(AttributeType::String),
            validate: |value| {
                if let Value::String(s) = value {
                    validate_ipv4_address(s)
                } else {
                    Err("Expected string".to_string())
                }
            },
            namespace: None,
            to_dsl: None,
        }
    }

    /// IPv6 address type (e.g., "2001:db8::1", "::1")
    pub fn ipv6_address() -> AttributeType {
        AttributeType::Custom {
            name: "Ipv6Address".to_string(),
            base: Box::new(AttributeType::String),
            validate: |value| {
                if let Value::String(s) = value {
                    validate_ipv6_address(s)
                } else {
                    Err("Expected string".to_string())
                }
            },
            namespace: None,
            to_dsl: None,
        }
    }

    /// IPv6 CIDR block type (e.g., "2001:db8::/32", "::/0")
    pub fn ipv6_cidr() -> AttributeType {
        AttributeType::Custom {
            name: "Ipv6Cidr".to_string(),
            base: Box::new(AttributeType::String),
            validate: |value| {
                if let Value::String(s) = value {
                    validate_ipv6_cidr(s)
                } else {
                    Err("Expected string".to_string())
                }
            },
            namespace: None,
            to_dsl: None,
        }
    }
}

/// Validate an IPv4 address (e.g., "10.0.1.5", "192.168.0.1")
pub fn validate_ipv4_address(ip: &str) -> Result<(), String> {
    let octets: Vec<&str> = ip.split('.').collect();
    if octets.len() != 4 {
        return Err(format!("Invalid IPv4 address '{}': expected 4 octets", ip));
    }

    for octet in &octets {
        match octet.parse::<u8>() {
            Ok(_) => {}
            Err(_) => {
                return Err(format!(
                    "Invalid octet '{}' in IPv4 address: must be 0-255",
                    octet
                ));
            }
        }
    }

    Ok(())
}

/// Validate IPv4 CIDR block format (e.g., "10.0.0.0/16")
pub fn validate_ipv4_cidr(cidr: &str) -> Result<(), String> {
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
    validate_ipv4_address(ip)?;

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

/// Backward-compatible alias for `validate_ipv4_cidr`
pub fn validate_cidr(cidr: &str) -> Result<(), String> {
    validate_ipv4_cidr(cidr)
}

/// Validate IPv6 CIDR block format (e.g., "2001:db8::/32", "::/0")
pub fn validate_ipv6_cidr(cidr: &str) -> Result<(), String> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid IPv6 CIDR format '{}': expected address/prefix",
            cidr
        ));
    }

    let addr = parts[0];
    let prefix = parts[1];

    // Validate IPv6 address
    validate_ipv6_address(addr)?;

    // Validate prefix length (0-128)
    match prefix.parse::<u8>() {
        Ok(p) if p <= 128 => Ok(()),
        Ok(p) => Err(format!("Invalid IPv6 prefix length '{}': must be 0-128", p)),
        Err(_) => Err(format!(
            "Invalid IPv6 prefix length '{}': must be a number",
            prefix
        )),
    }
}

/// Validate an IPv6 address (supports `::` shorthand)
pub fn validate_ipv6_address(addr: &str) -> Result<(), String> {
    if addr.is_empty() {
        return Err("Empty IPv6 address".to_string());
    }

    // Handle :: shorthand
    if addr.contains("::") {
        let halves: Vec<&str> = addr.splitn(2, "::").collect();
        if halves.len() != 2 {
            return Err(format!("Invalid IPv6 address '{}': malformed '::'", addr));
        }

        // Check for multiple ::
        if halves[1].contains("::") {
            return Err(format!(
                "Invalid IPv6 address '{}': only one '::' allowed",
                addr
            ));
        }

        let left_groups: Vec<&str> = if halves[0].is_empty() {
            vec![]
        } else {
            halves[0].split(':').collect()
        };
        let right_groups: Vec<&str> = if halves[1].is_empty() {
            vec![]
        } else {
            halves[1].split(':').collect()
        };

        let total = left_groups.len() + right_groups.len();
        if total > 7 {
            return Err(format!(
                "Invalid IPv6 address '{}': too many groups with '::'",
                addr
            ));
        }

        for group in left_groups.iter().chain(right_groups.iter()) {
            validate_ipv6_group(group, addr)?;
        }
    } else {
        let groups: Vec<&str> = addr.split(':').collect();
        if groups.len() != 8 {
            return Err(format!(
                "Invalid IPv6 address '{}': expected 8 groups, got {}",
                addr,
                groups.len()
            ));
        }
        for group in &groups {
            validate_ipv6_group(group, addr)?;
        }
    }

    Ok(())
}

/// Compute Levenshtein edit distance between two strings
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_len = a.len();
    let b_len = b.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0; b_len + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}

/// Suggest the most similar field name, if one is close enough
fn suggest_similar_name(unknown: &str, known: &[&str]) -> Option<String> {
    let max_distance = match unknown.len() {
        0..=2 => 1,
        3..=5 => 2,
        _ => 3,
    };

    known
        .iter()
        .map(|name| (*name, levenshtein_distance(unknown, name)))
        .filter(|(_, dist)| *dist <= max_distance)
        .min_by_key(|(_, dist)| *dist)
        .map(|(name, _)| name.to_string())
}

/// Validate a single IPv6 group (1-4 hex digits)
fn validate_ipv6_group(group: &str, addr: &str) -> Result<(), String> {
    if group.is_empty() || group.len() > 4 {
        return Err(format!(
            "Invalid IPv6 group '{}' in address '{}': must be 1-4 hex digits",
            group, addr
        ));
    }
    if !group.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid IPv6 group '{}' in address '{}': must be hex digits",
            group, addr
        ));
    }
    Ok(())
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
    fn struct_rejects_unknown_field() {
        let t = AttributeType::Struct {
            name: "Ingress".to_string(),
            fields: vec![
                StructField::new("ip_protocol", AttributeType::String).required(),
                StructField::new("from_port", AttributeType::Int),
                StructField::new("to_port", AttributeType::Int),
                StructField::new("cidr_ip", AttributeType::String),
            ],
        };

        // Unknown field should be rejected
        let mut map = HashMap::new();
        map.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        map.insert(
            "unknown_field".to_string(),
            Value::String("value".to_string()),
        );
        let result = t.validate(&Value::Map(map));
        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            TypeError::UnknownStructField {
                struct_name,
                field,
                suggestion,
            } => {
                assert_eq!(struct_name, "Ingress");
                assert_eq!(field, "unknown_field");
                assert!(suggestion.is_none());
            }
            other => panic!("Expected UnknownStructField, got: {:?}", other),
        }
    }

    #[test]
    fn struct_suggests_similar_field() {
        let t = AttributeType::Struct {
            name: "Ingress".to_string(),
            fields: vec![
                StructField::new("ip_protocol", AttributeType::String),
                StructField::new("from_port", AttributeType::Int),
                StructField::new("to_port", AttributeType::Int),
                StructField::new("cidr_ip", AttributeType::String),
            ],
        };

        // Typo: "ip_protcol" -> should suggest "ip_protocol"
        let mut map = HashMap::new();
        map.insert("ip_protcol".to_string(), Value::String("tcp".to_string()));
        let result = t.validate(&Value::Map(map));
        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            TypeError::UnknownStructField {
                struct_name,
                field,
                suggestion,
            } => {
                assert_eq!(struct_name, "Ingress");
                assert_eq!(field, "ip_protcol");
                assert_eq!(suggestion.as_deref(), Some("ip_protocol"));
            }
            other => panic!("Expected UnknownStructField, got: {:?}", other),
        }

        // Typo: "cidr_iip" -> should suggest "cidr_ip"
        let mut map2 = HashMap::new();
        map2.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        map2.insert(
            "cidr_iip".to_string(),
            Value::String("10.0.0.0/8".to_string()),
        );
        let result2 = t.validate(&Value::Map(map2));
        assert!(result2.is_err());
        let err2 = result2.unwrap_err();
        match &err2 {
            TypeError::UnknownStructField {
                suggestion, field, ..
            } => {
                assert_eq!(field, "cidr_iip");
                assert_eq!(suggestion.as_deref(), Some("cidr_ip"));
            }
            other => panic!("Expected UnknownStructField, got: {:?}", other),
        }
    }

    #[test]
    fn struct_error_message_format() {
        let t = AttributeType::Struct {
            name: "SecurityGroupIngress".to_string(),
            fields: vec![
                StructField::new("vpc_id", AttributeType::String),
                StructField::new("cidr_ip", AttributeType::String),
            ],
        };

        // With suggestion
        let mut map = HashMap::new();
        map.insert("vpc_idd".to_string(), Value::String("vpc-123".to_string()));
        let err = t.validate(&Value::Map(map)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unknown field 'vpc_idd' in SecurityGroupIngress, did you mean 'vpc_id'?"
        );

        // Without suggestion (completely different name)
        let mut map2 = HashMap::new();
        map2.insert(
            "completely_different".to_string(),
            Value::String("x".to_string()),
        );
        let err2 = t.validate(&Value::Map(map2)).unwrap_err();
        assert_eq!(
            err2.to_string(),
            "Unknown field 'completely_different' in SecurityGroupIngress"
        );
    }

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("abc", ""), 3);
        assert_eq!(levenshtein_distance("", "abc"), 3);
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("vpc_id", "vpc_idd"), 1);
        assert_eq!(levenshtein_distance("ip_protocol", "ip_protcol"), 1);
    }

    #[test]
    fn test_suggest_similar_name() {
        let fields = vec!["ip_protocol", "from_port", "to_port", "cidr_ip"];

        // Close match
        assert_eq!(
            suggest_similar_name("ip_protcol", &fields),
            Some("ip_protocol".to_string())
        );
        assert_eq!(
            suggest_similar_name("cidr_iip", &fields),
            Some("cidr_ip".to_string())
        );
        assert_eq!(
            suggest_similar_name("from_prot", &fields),
            Some("from_port".to_string())
        );

        // No match (too far)
        assert_eq!(suggest_similar_name("completely_unrelated", &fields), None);
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

    #[test]
    fn validate_ipv4_cidr_type() {
        let t = types::ipv4_cidr();

        // Valid IPv4 CIDRs
        assert!(
            t.validate(&Value::String("10.0.0.0/16".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("0.0.0.0/0".to_string())).is_ok());
        assert!(
            t.validate(&Value::String("255.255.255.255/32".to_string()))
                .is_ok()
        );

        // Invalid IPv4 CIDRs
        assert!(
            t.validate(&Value::String("10.0.0.0/33".to_string()))
                .is_err()
        );
        assert!(t.validate(&Value::String("10.0.0.0".to_string())).is_err());
        assert!(t.validate(&Value::Int(42)).is_err());
    }

    #[test]
    fn validate_ipv6_cidr_type() {
        let t = types::ipv6_cidr();

        // Valid IPv6 CIDRs
        assert!(t.validate(&Value::String("::/0".to_string())).is_ok());
        assert!(
            t.validate(&Value::String("2001:db8::/32".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("fe80::/10".to_string())).is_ok());
        assert!(t.validate(&Value::String("::1/128".to_string())).is_ok());
        assert!(
            t.validate(&Value::String(
                "2001:0db8:85a3:0000:0000:8a2e:0370:7334/64".to_string()
            ))
            .is_ok()
        );
        assert!(t.validate(&Value::String("ff00::/8".to_string())).is_ok());

        // Invalid IPv6 CIDRs
        assert!(
            t.validate(&Value::String("2001:db8::/129".to_string()))
                .is_err()
        ); // prefix > 128
        assert!(
            t.validate(&Value::String("2001:db8::".to_string()))
                .is_err()
        ); // missing prefix
        assert!(
            t.validate(&Value::String("2001:gggg::/32".to_string()))
                .is_err()
        ); // invalid hex
        assert!(
            t.validate(&Value::String("2001:db8::1::2/64".to_string()))
                .is_err()
        ); // double ::
        assert!(
            t.validate(&Value::String("10.0.0.0/16".to_string()))
                .is_err()
        ); // IPv4, not IPv6
        assert!(t.validate(&Value::Int(42)).is_err()); // wrong type
    }

    #[test]
    fn validate_ipv6_cidr_function_directly() {
        // Valid
        assert!(validate_ipv6_cidr("::/0").is_ok());
        assert!(validate_ipv6_cidr("2001:db8::/32").is_ok());
        assert!(validate_ipv6_cidr("fe80::/10").is_ok());
        assert!(validate_ipv6_cidr("::1/128").is_ok());
        assert!(validate_ipv6_cidr("2001:0db8:85a3:0000:0000:8a2e:0370:7334/64").is_ok());

        // Invalid
        assert!(validate_ipv6_cidr("2001:db8::/129").is_err());
        assert!(validate_ipv6_cidr("not-a-cidr").is_err());
        assert!(validate_ipv6_cidr("2001:db8::").is_err());
        assert!(validate_ipv6_cidr("/64").is_err());
    }

    #[test]
    fn custom_type_accepts_resource_ref() {
        // ResourceRef values resolve to strings at runtime, so Custom types should accept them
        let ipv4 = types::ipv4_cidr();
        assert!(
            ipv4.validate(&Value::ResourceRef(
                "vpc".to_string(),
                "cidr_block".to_string()
            ))
            .is_ok()
        );

        let ipv6 = types::ipv6_cidr();
        assert!(
            ipv6.validate(&Value::ResourceRef(
                "subnet".to_string(),
                "ipv6_cidr".to_string()
            ))
            .is_ok()
        );

        // TypedResourceRef should also be accepted
        assert!(
            ipv4.validate(&Value::TypedResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "cidr_block".to_string(),
                resource_type: None,
            })
            .is_ok()
        );
    }

    #[test]
    fn validate_ipv4_address_type() {
        let t = types::ipv4_address();

        // Valid IPv4 addresses
        assert!(t.validate(&Value::String("10.0.1.5".to_string())).is_ok());
        assert!(
            t.validate(&Value::String("192.168.0.1".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("0.0.0.0".to_string())).is_ok());
        assert!(
            t.validate(&Value::String("255.255.255.255".to_string()))
                .is_ok()
        );

        // Invalid IPv4 addresses
        assert!(
            t.validate(&Value::String("10.0.0.0/16".to_string()))
                .is_err()
        ); // CIDR, not address
        assert!(t.validate(&Value::String("256.0.0.1".to_string())).is_err()); // octet > 255
        assert!(t.validate(&Value::String("10.0.1".to_string())).is_err()); // only 3 octets
        assert!(t.validate(&Value::String("not-an-ip".to_string())).is_err());
        assert!(t.validate(&Value::Int(42)).is_err()); // wrong type
    }

    #[test]
    fn validate_ipv6_address_type() {
        let t = types::ipv6_address();

        // Valid IPv6 addresses
        assert!(t.validate(&Value::String("::1".to_string())).is_ok());
        assert!(
            t.validate(&Value::String("2001:db8::1".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("fe80::1".to_string())).is_ok());
        assert!(
            t.validate(&Value::String(
                "2001:0db8:85a3:0000:0000:8a2e:0370:7334".to_string()
            ))
            .is_ok()
        );

        // Invalid IPv6 addresses
        assert!(
            t.validate(&Value::String("2001:db8::/32".to_string()))
                .is_err()
        ); // CIDR, not address
        assert!(t.validate(&Value::String("not-an-ip".to_string())).is_err());
        assert!(t.validate(&Value::String("".to_string())).is_err());
        assert!(t.validate(&Value::Int(42)).is_err()); // wrong type
    }

    #[test]
    fn types_module_has_no_aws_specific_types() {
        // Verify that AWS-specific types are not defined in carina-core.
        // These belong in provider crates (e.g., carina-provider-awscc).
        let source = include_str!("schema.rs");
        let aws_keywords = [
            "fn arn()",
            "fn aws_resource_id()",
            "fn availability_zone()",
            "validate_arn",
            "validate_aws_resource_id",
            "validate_availability_zone",
        ];
        for keyword in &aws_keywords {
            // Exclude this test function itself from the check
            let occurrences: Vec<_> = source.match_indices(keyword).collect();
            // Each keyword appears once in the aws_keywords array literal above
            // If it appears more than once, it means it's also defined elsewhere
            assert!(
                occurrences.len() <= 1,
                "Found AWS-specific type '{}' in carina-core/src/schema.rs. \
                 AWS-specific types belong in provider crates.",
                keyword
            );
        }
    }

    #[test]
    fn resource_validator_called() {
        fn my_validator(attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
            if attributes.contains_key("forbidden") {
                Err(vec![TypeError::ValidationFailed {
                    message: "forbidden attribute not allowed".to_string(),
                }])
            } else {
                Ok(())
            }
        }

        let schema = ResourceSchema::new("test")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("forbidden", AttributeType::String))
            .with_validator(my_validator);

        // Valid: no forbidden attribute
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("test".to_string()));
        assert!(schema.validate(&attrs).is_ok());

        // Invalid: forbidden attribute present
        let mut bad_attrs = HashMap::new();
        bad_attrs.insert("forbidden".to_string(), Value::String("bad".to_string()));
        let result = schema.validate(&bad_attrs);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().len(), 1);
    }

    #[test]
    fn validate_exclusive_required_helper() {
        use validators::validate_exclusive_required;

        // Valid: exactly one field present
        let mut attrs = HashMap::new();
        attrs.insert("option_a".to_string(), Value::String("value".to_string()));
        assert!(validate_exclusive_required(&attrs, &["option_a", "option_b"]).is_ok());

        let mut attrs2 = HashMap::new();
        attrs2.insert("option_b".to_string(), Value::String("value".to_string()));
        assert!(validate_exclusive_required(&attrs2, &["option_a", "option_b"]).is_ok());

        // Invalid: neither field present
        let empty = HashMap::new();
        let result = validate_exclusive_required(&empty, &["option_a", "option_b"]);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("Exactly one of [option_a, option_b] must be specified")
        );

        // Invalid: both fields present
        let mut both = HashMap::new();
        both.insert("option_a".to_string(), Value::String("a".to_string()));
        both.insert("option_b".to_string(), Value::String("b".to_string()));
        let result = validate_exclusive_required(&both, &["option_a", "option_b"]);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("Only one of [option_a, option_b] can be specified")
        );
        assert!(errors[0].to_string().contains("option_a, option_b"));
    }

    #[test]
    fn exclusive_required_with_resource_schema() {
        fn subnet_validator(attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
            validators::validate_exclusive_required(
                attributes,
                &["cidr_block", "ipv4_ipam_pool_id"],
            )
        }

        let schema = ResourceSchema::new("subnet")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
            .attribute(AttributeSchema::new(
                "ipv4_ipam_pool_id",
                AttributeType::String,
            ))
            .attribute(AttributeSchema::new("vpc_id", AttributeType::String).required())
            .with_validator(subnet_validator);

        // Valid: has cidr_block only
        let mut attrs1 = HashMap::new();
        attrs1.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        attrs1.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/24".to_string()),
        );
        assert!(schema.validate(&attrs1).is_ok());

        // Valid: has ipv4_ipam_pool_id only
        let mut attrs2 = HashMap::new();
        attrs2.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        attrs2.insert(
            "ipv4_ipam_pool_id".to_string(),
            Value::String("ipam-pool-123".to_string()),
        );
        assert!(schema.validate(&attrs2).is_ok());

        // Invalid: has neither
        let mut attrs3 = HashMap::new();
        attrs3.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        let result = schema.validate(&attrs3);
        assert!(result.is_err());

        // Invalid: has both
        let mut attrs4 = HashMap::new();
        attrs4.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        attrs4.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/24".to_string()),
        );
        attrs4.insert(
            "ipv4_ipam_pool_id".to_string(),
            Value::String("ipam-pool-123".to_string()),
        );
        let result = schema.validate(&attrs4);
        assert!(result.is_err());
    }
}
