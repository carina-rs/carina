//! Schema - Define type schemas for resources
//!
//! Providers define schemas for each resource type,
//! enabling type validation at parse time.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::resource::{Resource, Value};
use crate::utils::{extract_enum_value_with_values, validate_enum_namespace};
use crate::value::format_value_with_key;

/// Type alias for resource validator functions
pub type ResourceValidator = fn(&HashMap<String, Value>) -> Result<(), Vec<TypeError>>;
pub type StringEnumParts<'a> = (
    &'a str,
    &'a [String],
    Option<&'a str>,
    Option<fn(&str) -> String>,
);
pub type NamespacedEnumParts<'a> = (&'a str, &'a str, Option<fn(&str) -> String>);

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
    /// Alternative block name for repeated block syntax (e.g., "transition" for "transitions")
    pub block_name: Option<String>,
}

impl StructField {
    pub fn new(name: impl Into<String>, field_type: AttributeType) -> Self {
        Self {
            name: name.into(),
            field_type,
            required: false,
            description: None,
            provider_name: None,
            block_name: None,
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

    pub fn with_block_name(mut self, name: impl Into<String>) -> Self {
        self.block_name = Some(name.into());
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
    /// Floating-point number
    Float,
    /// Boolean
    Bool,
    /// String enum with optional namespace-aware DSL syntax support
    StringEnum {
        name: String,
        values: Vec<String>,
        namespace: Option<String>,
        to_dsl: Option<fn(&str) -> String>,
    },
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
    /// `ordered`: if true, element order matters (sequential comparison);
    /// if false, order is ignored (multiset comparison).
    /// Defaults to true (matching CloudFormation's insertionOrder default).
    List {
        inner: Box<AttributeType>,
        ordered: bool,
    },
    /// Map with typed keys and values.
    /// `key`: type constraint for map keys (e.g., `String` for unconstrained,
    /// `StringEnum` for condition operators).
    /// `value`: type of map values.
    Map {
        key: Box<AttributeType>,
        value: Box<AttributeType>,
    },
    /// Struct (named object with typed fields)
    Struct {
        name: String,
        fields: Vec<StructField>,
    },
    /// Union of multiple types (value is valid if it matches any member)
    Union(Vec<AttributeType>),
}

impl AttributeType {
    /// Create a List type with default ordering (ordered=true, matching CloudFormation default).
    pub fn list(inner: AttributeType) -> Self {
        AttributeType::List {
            inner: Box::new(inner),
            ordered: true,
        }
    }

    /// Create an unordered List type (insertionOrder=false).
    pub fn unordered_list(inner: AttributeType) -> Self {
        AttributeType::List {
            inner: Box::new(inner),
            ordered: false,
        }
    }

    /// Create a Map type with unconstrained string keys.
    pub fn map(value: AttributeType) -> Self {
        Self::map_with_key(AttributeType::String, value)
    }

    /// Create a Map type with a typed key constraint.
    pub fn map_with_key(key: AttributeType, value: AttributeType) -> Self {
        AttributeType::Map {
            key: Box::new(key),
            value: Box::new(value),
        }
    }

    fn resolve_enum_input(
        name: &str,
        namespace: Option<&str>,
        value: &Value,
    ) -> Result<Value, TypeError> {
        if matches!(value, Value::ResourceRef { .. }) {
            return Ok(value.clone());
        }

        match value {
            Value::String(s) if !s.contains('.') => {
                // Bare identifier like "dedicated"
                let expanded = match namespace {
                    Some(ns) => format!("{}.{}.{}", ns, name, s),
                    None => s.clone(),
                };
                Ok(Value::String(expanded))
            }
            Value::String(s) if s.split('.').count() == 2 => {
                // Two-part identifier like "InstanceTenancy.dedicated"
                if let Some((ident, member)) = s.split_once('.') {
                    let expanded = match namespace {
                        Some(ns) if ident == name => format!("{}.{}.{}", ns, ident, member),
                        Some(_) => s.clone(),
                        None => s.clone(),
                    };
                    Ok(Value::String(expanded))
                } else {
                    Ok(value.clone())
                }
            }
            other => Ok(other.clone()),
        }
    }

    pub fn string_enum_parts(&self) -> Option<StringEnumParts<'_>> {
        match self {
            AttributeType::StringEnum {
                name,
                values,
                namespace,
                to_dsl,
            } => Some((name, values, namespace.as_deref(), *to_dsl)),
            _ => None,
        }
    }

    pub fn namespaced_enum_parts(&self) -> Option<NamespacedEnumParts<'_>> {
        match self {
            AttributeType::StringEnum {
                name,
                namespace: Some(namespace),
                to_dsl,
                ..
            }
            | AttributeType::Custom {
                name,
                namespace: Some(namespace),
                to_dsl,
                ..
            } => Some((name, namespace, *to_dsl)),
            _ => None,
        }
    }

    /// Check if a value conforms to this type
    pub fn validate(&self, value: &Value) -> Result<(), TypeError> {
        // FunctionCall and Secret values are resolved at runtime, skip validation
        if matches!(value, Value::FunctionCall { .. } | Value::Secret(_)) {
            return Ok(());
        }

        match (self, value) {
            // ResourceRef and Interpolation values resolve to strings at runtime, so they're valid for String types
            (
                AttributeType::String,
                Value::String(_) | Value::ResourceRef { .. } | Value::Interpolation(_),
            ) => Ok(()),
            (AttributeType::Int, Value::Int(_)) => Ok(()),
            (AttributeType::Float, Value::Float(f)) if f.is_finite() => Ok(()),
            (AttributeType::Float, Value::Float(f)) => Err(TypeError::ValidationFailed {
                message: format!("non-finite float value: {f}"),
            }),
            (AttributeType::Float, Value::Int(_)) => Ok(()), // integers are valid numbers
            (AttributeType::Bool, Value::Bool(_)) => Ok(()),

            (
                AttributeType::StringEnum {
                    name,
                    values,
                    namespace,
                    to_dsl,
                },
                v,
            ) => {
                // Interpolation values resolve to strings at runtime, so accept them
                if matches!(v, Value::Interpolation(_)) {
                    return Ok(());
                }
                let resolved_value = Self::resolve_enum_input(name, namespace.as_deref(), v)?;
                if matches!(resolved_value, Value::ResourceRef { .. }) {
                    return Ok(());
                }
                if let Value::String(s) = &resolved_value {
                    // Check if the raw string directly matches a valid enum value
                    // before namespace validation. This handles values containing
                    // dots (e.g., "ipsec.1") that would be misinterpreted as
                    // namespace separators.
                    let direct_match = values.iter().any(|v| string_enum_value_matches(s, v));
                    let valid: Vec<&str> = values.iter().map(String::as_str).collect();
                    let variant = if direct_match {
                        s.as_str()
                    } else {
                        extract_enum_value_with_values(s, &valid)
                    };

                    // Non-direct matches must have the exact form
                    // `{namespace}.{name}.{variant}`. This rejects malformed
                    // inputs like double-namespaced values while still allowing
                    // enum values that themselves contain dots (e.g., "ipsec.1").
                    if !direct_match && let Some(ns) = namespace.as_deref() {
                        let expected_prefix = format!("{}.{}.", ns, name);
                        let prefix_matches = s.starts_with(&expected_prefix)
                            && &s[expected_prefix.len()..] == variant;
                        if !prefix_matches {
                            // Fall back to strict namespace validation, which
                            // produces a clear error for the common bare form.
                            validate_enum_namespace(s, name, ns).map_err(|message| {
                                TypeError::ValidationFailed {
                                    message: format!("Invalid {} '{}': {}", name, s, message),
                                }
                            })?;
                        }
                    }
                    let matches_canonical =
                        values.iter().any(|v| string_enum_value_matches(variant, v));
                    let matches_alias = to_dsl.is_some_and(|f| {
                        values
                            .iter()
                            .any(|v| string_enum_value_matches(variant, &f(v)))
                    });
                    if matches_canonical || matches_alias {
                        Ok(())
                    } else {
                        let mut expected = values.clone();
                        if let Some(f) = to_dsl {
                            for v in values {
                                let alias = f(v);
                                if alias != *v && !expected.contains(&alias) {
                                    expected.push(alias);
                                }
                            }
                        }
                        Err(TypeError::InvalidEnumVariant {
                            value: s.clone(),
                            expected,
                        })
                    }
                } else {
                    Err(TypeError::TypeMismatch {
                        expected: self.type_name(),
                        got: resolved_value.type_name(),
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
                // ResourceRef and Interpolation values resolve to strings at runtime,
                // so they're valid for Custom types
                if matches!(v, Value::ResourceRef { .. } | Value::Interpolation(_)) {
                    return Ok(());
                }
                let resolved_value = Self::resolve_enum_input(name, namespace.as_deref(), v)?;
                validate(&resolved_value)
                    .map_err(|msg| TypeError::ValidationFailed { message: msg })
            }

            (AttributeType::List { inner, .. }, Value::List(items)) => {
                for (i, item) in items.iter().enumerate() {
                    inner.validate(item).map_err(|e| TypeError::ListItemError {
                        index: i,
                        inner: Box::new(e),
                    })?;
                }
                Ok(())
            }

            (
                AttributeType::Map {
                    key: key_type,
                    value: inner,
                },
                Value::Map(map),
            ) => {
                // Validate keys against key type
                for k in map.keys() {
                    key_type.validate(&Value::String(k.clone())).map_err(|e| {
                        TypeError::MapKeyError {
                            key: k.clone(),
                            inner: Box::new(e),
                        }
                    })?;
                }
                for (k, v) in map {
                    inner.validate(v).map_err(|e| TypeError::MapValueError {
                        key: k.clone(),
                        inner: Box::new(e),
                    })?;
                }
                Ok(())
            }

            // Struct type rejects Value::List (block syntax)
            // Block syntax produces Value::List([Value::Map(...)]), but bare Struct
            // requires map assignment syntax: attr = { ... }
            (AttributeType::Struct { name, .. }, Value::List(_)) => {
                Err(TypeError::BlockSyntaxNotAllowed {
                    attribute: name.clone(),
                })
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

            // Union type: valid if any member accepts the value
            (AttributeType::Union(types), _) => {
                for member in types {
                    if member.validate(value).is_ok() {
                        return Ok(());
                    }
                }
                Err(TypeError::TypeMismatch {
                    expected: self.type_name(),
                    got: value.type_name(),
                })
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
            AttributeType::Float => "Float".to_string(),
            AttributeType::Bool => "Bool".to_string(),
            AttributeType::StringEnum { name, .. } => name.clone(),
            AttributeType::Custom { name, .. } => name.clone(),
            AttributeType::List { inner, .. } => format!("List<{}>", inner.type_name()),
            AttributeType::Map { value: inner, .. } => format!("Map<{}>", inner.type_name()),
            AttributeType::Struct { name, .. } => format!("Struct({})", name),
            AttributeType::Union(types) => {
                let names: Vec<String> = types.iter().map(|t| t.type_name()).collect();
                names.join(" | ")
            }
        }
    }

    /// Check if a type name is accepted by this type.
    /// For Union types, returns true if any member accepts the name.
    /// For other types, returns true if self.type_name() == name.
    pub fn accepts_type_name(&self, name: &str) -> bool {
        match self {
            AttributeType::Union(types) => types.iter().any(|t| t.accepts_type_name(name)),
            _ => self.type_name() == name,
        }
    }
}

impl fmt::Display for AttributeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.type_name())
    }
}

fn string_enum_value_matches(input: &str, expected: &str) -> bool {
    input == expected
        || input.eq_ignore_ascii_case(expected)
        || input.replace('_', "-").eq_ignore_ascii_case(expected)
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

    #[error("Resource validation failed: {message}")]
    ResourceValidationFailed {
        message: String,
        /// Optional attribute name for precise diagnostic positioning.
        attribute: Option<String>,
    },

    #[error("Required attribute '{name}' is missing")]
    MissingRequired { name: String },

    #[error("Unknown attribute '{name}'{}", suggestion.as_ref().map(|s| format!(", did you mean '{}'?", s)).unwrap_or_default())]
    UnknownAttribute {
        name: String,
        suggestion: Option<String>,
    },

    #[error("Unknown field '{field}' in {struct_name}{}", suggestion.as_ref().map(|s| format!(", did you mean '{}'?", s)).unwrap_or_default())]
    UnknownStructField {
        struct_name: String,
        field: String,
        suggestion: Option<String>,
    },

    #[error("List item at index {index}: {inner}")]
    ListItemError { index: usize, inner: Box<TypeError> },

    #[error("Map key '{key}': {inner}")]
    MapKeyError { key: String, inner: Box<TypeError> },

    #[error("Map value for key '{key}': {inner}")]
    MapValueError { key: String, inner: Box<TypeError> },

    #[error("Struct field '{field}': {inner}")]
    StructFieldError {
        field: String,
        inner: Box<TypeError>,
    },

    #[error("'{attribute}' cannot use block syntax; use map assignment: {attribute} = {{ ... }}")]
    BlockSyntaxNotAllowed { attribute: String },
}

impl Value {
    fn type_name(&self) -> String {
        match self {
            Value::String(_) => "String".to_string(),
            Value::Int(_) => "Int".to_string(),
            Value::Float(_) => "Float".to_string(),
            Value::Bool(_) => "Bool".to_string(),
            Value::List(_) => "List".to_string(),
            Value::Map(_) => "Map".to_string(),
            Value::ResourceRef { path } => {
                format!("ResourceRef({})", path.to_dot_string())
            }
            Value::Interpolation(_) => "Interpolation".to_string(),
            Value::FunctionCall { name, .. } => format!("FunctionCall({})", name),
            Value::Secret(_) => "Secret".to_string(),
            Value::Closure { name, .. } => format!("Closure({})", name),
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
            0 => Err(vec![TypeError::ResourceValidationFailed {
                message: format!("Exactly one of [{}] must be specified", fields.join(", ")),
                attribute: None,
            }]),
            1 => Ok(()),
            _ => Err(vec![TypeError::ResourceValidationFailed {
                message: format!(
                    "Only one of [{}] can be specified, but found: {}",
                    fields.join(", "),
                    present_fields.join(", ")
                ),
                attribute: present_fields.first().map(|s| s.to_string()),
            }]),
        }
    }
}

/// Completion value for LSP completions
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// Whether this attribute is read-only (set by the provider, cannot be updated)
    pub read_only: bool,
    /// Override for removability detection.
    /// `None` = auto-detect: removable if `!required && !create_only`.
    /// `Some(false)` = explicitly non-removable (e.g., region inherited from provider).
    /// Only removable attributes trigger removal detection in the differ.
    pub removable: Option<bool>,
    /// Alternative block name for repeated block syntax (e.g., "operating_region" for "operating_regions")
    pub block_name: Option<String>,
    /// Whether this attribute is write-only (not returned by the provider's read API).
    /// Write-only attributes are sent to the provider during create/update but may not
    /// appear in read responses. This is NOT related to sensitive/secret values — it
    /// indicates a CloudFormation `writeOnlyProperties` attribute.
    pub write_only: bool,
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
            read_only: false,
            removable: None,
            block_name: None,
            write_only: false,
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

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub fn write_only(mut self) -> Self {
        self.write_only = true;
        self
    }

    pub fn removable(mut self) -> Self {
        self.removable = Some(true);
        self
    }

    pub fn non_removable(mut self) -> Self {
        self.removable = Some(false);
        self
    }

    /// Whether this attribute can be removed from infrastructure.
    /// Auto-detected: optional (not required), mutable (not create-only), and writable
    /// (not read-only) attributes are removable by default. Can be overridden with
    /// `.removable()` or `.non_removable()`.
    pub fn is_removable(&self) -> bool {
        self.removable
            .unwrap_or(!self.required && !self.create_only && !self.read_only)
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

    pub fn with_block_name(mut self, name: impl Into<String>) -> Self {
        self.block_name = Some(name.into());
        self
    }
}

/// Per-resource operational configuration for provider-specific timeouts and retries.
///
/// Providers can set these on individual resource schemas to override default
/// polling/retry behavior. This avoids hardcoding resource-type string matches
/// in provider implementations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperationConfig {
    /// Polling timeout for delete operations in seconds.
    /// Default: provider-specific (e.g., 600s for CloudControl).
    pub delete_timeout_secs: Option<u64>,
    /// Maximum retry attempts for retryable delete errors.
    /// Default: provider-specific (e.g., 12 for CloudControl).
    pub delete_max_retries: Option<u32>,
    /// Polling timeout for create operations in seconds.
    /// Default: provider-specific (e.g., 600s for CloudControl).
    pub create_timeout_secs: Option<u64>,
    /// Maximum retry attempts for retryable create errors.
    /// Default: provider-specific (e.g., 12 for CloudControl).
    pub create_max_retries: Option<u32>,
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
    /// If true, this resource type is a data source and must be used with `read`
    pub data_source: bool,
    /// The attribute that serves as the unique name for this resource type.
    /// Used for automatic unique name generation during create-before-destroy replacement.
    /// (e.g., "bucket_name" for s3.bucket, "log_group_name" for logs.log_group)
    pub name_attribute: Option<String>,
    /// If true, updates are not supported for this resource type.
    /// The differ will always generate Replace instead of Update.
    /// Used for resource types where the provider API rejects updates
    /// despite the schema indicating update support.
    pub force_replace: bool,
    /// Per-resource operational config (timeouts, retries).
    /// When None, provider defaults are used.
    pub operation_config: Option<OperationConfig>,
}

impl ResourceSchema {
    pub fn new(resource_type: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            attributes: HashMap::new(),
            description: None,
            validator: None,
            data_source: false,
            name_attribute: None,
            force_replace: false,
            operation_config: None,
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

    pub fn as_data_source(mut self) -> Self {
        self.data_source = true;
        self
    }

    pub fn with_name_attribute(mut self, attr: impl Into<String>) -> Self {
        self.name_attribute = Some(attr.into());
        self
    }

    pub fn force_replace(mut self) -> Self {
        self.force_replace = true;
        self
    }

    pub fn with_operation_config(mut self, config: OperationConfig) -> Self {
        self.operation_config = Some(config);
        self
    }

    /// Returns a map of block_name -> canonical attribute name
    /// for all attributes that have a block_name set.
    pub fn block_name_map(&self) -> HashMap<String, String> {
        self.attributes
            .iter()
            .filter_map(|(attr_name, schema)| {
                schema
                    .block_name
                    .as_ref()
                    .map(|bn| (bn.clone(), attr_name.clone()))
            })
            .collect()
    }

    /// Returns the names of read-only attributes (set by the provider after creation)
    pub fn read_only_attributes(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.read_only)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Returns attributes that have default values and are not read-only.
    /// Each entry is (attribute_name, default_value).
    pub fn default_value_attributes(&self) -> Vec<(&str, &Value)> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.default.is_some() && !schema.read_only)
            .map(|(name, schema)| (name.as_str(), schema.default.as_ref().unwrap()))
            .collect()
    }

    /// Returns default-value attributes not specified by the user, sorted by name.
    /// Each entry is (attribute_name, formatted_default_value).
    pub fn compute_default_attrs(&self, user_keys: &HashSet<&str>) -> Vec<(String, String)> {
        let mut default_attrs: Vec<(&str, &Value)> = self
            .default_value_attributes()
            .into_iter()
            .filter(|(a, _)| !user_keys.contains(a))
            .collect();
        default_attrs.sort_by_key(|(a, _)| *a);
        default_attrs
            .into_iter()
            .map(|(name, val)| (name.to_string(), format_value_with_key(val, Some(name))))
            .collect()
    }

    /// Returns read-only attribute names not specified by the user, sorted.
    pub fn compute_read_only_attrs(&self, user_keys: &HashSet<&str>) -> Vec<String> {
        let mut ro_attrs: Vec<&str> = self
            .read_only_attributes()
            .into_iter()
            .filter(|a| !user_keys.contains(a))
            .collect();
        ro_attrs.sort();
        ro_attrs.into_iter().map(|a| a.to_string()).collect()
    }

    /// Returns the names of create-only (immutable) attributes
    pub fn create_only_attributes(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.create_only)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Returns the names of removable attributes.
    /// By default, optional and mutable attributes are removable.
    pub fn removable_attributes(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.is_removable())
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

        // Build block_name -> canonical_name map for alias resolution
        let bn_map = self.block_name_map();

        // Build suggestion candidates (canonical names + block name aliases)
        let mut known: Vec<&str> = self.attributes.keys().map(|s| s.as_str()).collect();
        for bn in bn_map.keys() {
            known.push(bn.as_str());
        }

        // Type check each attribute and reject unknown ones
        for (name, value) in attributes {
            // Skip internal attributes (e.g., _binding)
            if name.starts_with('_') {
                continue;
            }

            // Resolve block_name alias to canonical name
            let canonical = bn_map.get(name).map(|s| s.as_str()).unwrap_or(name);

            if let Some(schema) = self.attributes.get(canonical) {
                if let Err(e) = schema.attr_type.validate(value) {
                    errors.push(e);
                }
            } else {
                let suggestion = suggest_similar_name(name, &known);
                errors.push(TypeError::UnknownAttribute {
                    name: name.clone(),
                    suggestion,
                });
            }
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

/// Collect all attribute_name -> block_name mappings from all schemas.
/// This includes both top-level attributes and nested struct fields.
/// Used by the formatter to convert `= [{...}]` to block syntax.
pub fn collect_all_block_names(
    schemas: &HashMap<String, ResourceSchema>,
) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for schema in schemas.values() {
        for (attr_name, attr_schema) in &schema.attributes {
            if let Some(bn) = &attr_schema.block_name {
                result.insert(attr_name.clone(), bn.clone());
            }
            // Also collect from nested struct fields
            collect_block_names_from_type(&attr_schema.attr_type, &mut result);
        }
    }
    result
}

fn collect_block_names_from_type(attr_type: &AttributeType, result: &mut HashMap<String, String>) {
    match attr_type {
        AttributeType::Struct { fields, .. } => {
            for field in fields {
                if let Some(bn) = &field.block_name {
                    result.insert(field.name.clone(), bn.clone());
                }
                collect_block_names_from_type(&field.field_type, result);
            }
        }
        AttributeType::List { inner, .. } => {
            collect_block_names_from_type(inner, result);
        }
        AttributeType::Map { value: inner, .. } => {
            collect_block_names_from_type(inner, result);
        }
        AttributeType::Union(types) => {
            for t in types {
                collect_block_names_from_type(t, result);
            }
        }
        _ => {}
    }
}

/// Resolve block name aliases in a map using struct field definitions.
///
/// For each key in `map` that matches a `block_name` on a struct field,
/// renames it to the canonical field name. Also recurses into nested
/// struct values to resolve block names at all nesting levels.
fn resolve_block_names_in_map(
    map: &mut HashMap<String, Value>,
    fields: &[StructField],
    resource_id: &str,
    errors: &mut Vec<String>,
) {
    // Build block_name -> canonical field name mapping
    let bn_map: HashMap<String, String> = fields
        .iter()
        .filter_map(|f| f.block_name.as_ref().map(|bn| (bn.clone(), f.name.clone())))
        .collect();

    // Rename block name keys to canonical names, but only when the value
    // is a List (from block syntax). Non-list values (e.g., Value::Map from
    // attribute assignment) target the actual field with that name.
    let renames: Vec<(String, String)> = map
        .keys()
        .filter_map(|key| {
            bn_map.get(key).and_then(|canon| {
                // Only rename if the value is a List (block-originated)
                if matches!(map.get(key), Some(Value::List(_))) {
                    Some((key.clone(), canon.clone()))
                } else {
                    None
                }
            })
        })
        .collect();

    for (block_key, canon_key) in renames {
        // When block_name == canonical name, no rename is needed
        if block_key == canon_key {
            continue;
        }
        if map.contains_key(&canon_key) {
            errors.push(format!(
                "{}: cannot use both '{}' and '{}' (they refer to the same attribute)",
                resource_id, block_key, canon_key
            ));
            continue;
        }
        let value = map.remove(&block_key).unwrap();
        map.insert(canon_key, value);
    }

    // Recurse into nested struct values
    for field in fields {
        let value = match map.get_mut(&field.name) {
            Some(v) => v,
            None => continue,
        };
        match &field.field_type {
            AttributeType::Struct { fields: inner, .. } => {
                if let Value::Map(inner_map) = value {
                    resolve_block_names_in_map(inner_map, inner, resource_id, errors);
                }
            }
            AttributeType::List { inner, .. } => {
                if let AttributeType::Struct { fields: inner, .. } = inner.as_ref()
                    && let Value::List(items) = value
                {
                    for item in items.iter_mut() {
                        if let Value::Map(item_map) = item {
                            resolve_block_names_in_map(item_map, inner, resource_id, errors);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Resolve block name aliases in resources.
///
/// For each resource attribute key that matches a `block_name` in the schema,
/// renames it to the canonical attribute name. Errors if both the block_name
/// (singular) and the canonical attribute name (plural) are present.
///
/// Also recursively resolves block names in nested struct values.
///
/// The `schema_key_fn` closure computes the schema lookup key for a resource.
pub fn resolve_block_names(
    resources: &mut [Resource],
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: impl Fn(&Resource) -> String,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    for resource in resources.iter_mut() {
        let schema_key = schema_key_fn(resource);
        let schema = match schemas.get(&schema_key) {
            Some(s) => s,
            None => continue,
        };

        let bn_map = schema.block_name_map();

        // Collect keys to rename: (block_name_key, canonical_attr_name)
        // Only rename when the value is a List (from block syntax). Non-list values
        // (e.g., Value::Map from attribute assignment) target the actual field with that name.
        let renames: Vec<(String, String)> = resource
            .attributes
            .keys()
            .filter_map(|key| {
                bn_map.get(key).and_then(|canon| {
                    if matches!(resource.get_attr(key), Some(Value::List(_))) {
                        Some((key.clone(), canon.clone()))
                    } else {
                        None
                    }
                })
            })
            .collect();

        for (block_key, canon_key) in renames {
            // When block_name == canonical name, no rename is needed
            if block_key == canon_key {
                continue;
            }
            if resource.attributes.contains_key(&canon_key) {
                all_errors.push(format!(
                    "{}: cannot use both '{}' and '{}' (they refer to the same attribute)",
                    resource.id, block_key, canon_key
                ));
                continue;
            }

            let expr = resource.attributes.remove(&block_key).unwrap();
            resource.attributes.insert(canon_key, expr);
        }

        // Recurse into nested struct values to resolve block names at all levels
        for (attr_name, attr_schema) in &schema.attributes {
            let value = match resource.attributes.get_mut(attr_name) {
                Some(v) => v,
                None => continue,
            };
            match &attr_schema.attr_type {
                AttributeType::Struct { fields, .. } => {
                    if let Value::Map(inner_map) = &mut **value {
                        resolve_block_names_in_map(
                            inner_map,
                            fields,
                            &resource.id.to_string(),
                            &mut all_errors,
                        );
                    }
                }
                AttributeType::List { inner, .. } => {
                    if let AttributeType::Struct { fields, .. } = inner.as_ref()
                        && let Value::List(items) = &mut **value
                    {
                        for item in items.iter_mut() {
                            if let Value::Map(item_map) = item {
                                resolve_block_names_in_map(
                                    item_map,
                                    fields,
                                    &resource.id.to_string(),
                                    &mut all_errors,
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
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

    /// CIDR block type that accepts both IPv4 and IPv6 (e.g., "10.0.0.0/16" or "2001:db8::/32")
    pub fn cidr() -> AttributeType {
        AttributeType::Union(vec![ipv4_cidr(), ipv6_cidr()])
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
pub fn suggest_similar_name(unknown: &str, known: &[&str]) -> Option<String> {
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
    fn attribute_schema_write_only_default_false() {
        let attr = AttributeSchema::new("ipv4_netmask_length", AttributeType::Int);
        assert!(!attr.write_only);
    }

    #[test]
    fn attribute_schema_write_only_builder() {
        let attr = AttributeSchema::new("ipv4_netmask_length", AttributeType::Int).write_only();
        assert!(attr.write_only);
    }

    #[test]
    fn resource_schema_data_source_default_false() {
        let schema = ResourceSchema::new("test.resource");
        assert!(!schema.data_source);
    }

    #[test]
    fn resource_schema_as_data_source_sets_flag() {
        let schema = ResourceSchema::new("test.resource").as_data_source();
        assert!(schema.data_source);
    }

    #[test]
    fn validate_string_type() {
        let t = AttributeType::String;
        assert!(t.validate(&Value::String("hello".to_string())).is_ok());
        assert!(t.validate(&Value::Int(42)).is_err());
    }

    #[test]
    fn validate_string_enum_type() {
        let t = AttributeType::StringEnum {
            name: "AddressFamily".to_string(),
            values: vec!["IPv4".to_string(), "IPv6".to_string()],
            namespace: Some("awscc.ec2.ipam_pool".to_string()),
            to_dsl: None,
        };
        assert!(
            t.validate(&Value::String(
                "awscc.ec2.ipam_pool.AddressFamily.IPv4".to_string()
            ))
            .is_ok()
        );
        assert!(t.validate(&Value::String("IPv6".to_string())).is_ok());
        assert!(t.validate(&Value::String("ipv4".to_string())).is_ok());
        assert!(t.validate(&Value::String("IPv5".to_string())).is_err());
    }

    #[test]
    fn string_enum_type_name_uses_declared_name() {
        let t = AttributeType::StringEnum {
            name: "VersioningStatus".to_string(),
            values: vec!["Enabled".to_string(), "Suspended".to_string()],
            namespace: Some("aws.s3.bucket".to_string()),
            to_dsl: None,
        };
        assert_eq!(t.type_name(), "VersioningStatus");
    }

    #[test]
    fn validate_string_enum_accepts_to_dsl_alias() {
        let t = AttributeType::StringEnum {
            name: "IpProtocol".to_string(),
            values: vec![
                "tcp".to_string(),
                "udp".to_string(),
                "icmp".to_string(),
                "icmpv6".to_string(),
                "-1".to_string(),
            ],
            namespace: Some("awscc.ec2.security_group".to_string()),
            to_dsl: Some(|s: &str| match s {
                "-1" => "all".to_string(),
                _ => s.replace('-', "_"),
            }),
        };
        // Canonical value "-1" should be accepted
        assert!(t.validate(&Value::String("-1".to_string())).is_ok());
        // DSL alias "all" should be accepted
        assert!(
            t.validate(&Value::String(
                "awscc.ec2.security_group.IpProtocol.all".to_string()
            ))
            .is_ok()
        );
        // Other canonical values should still work
        assert!(t.validate(&Value::String("tcp".to_string())).is_ok());
        // Invalid values should still be rejected
        assert!(t.validate(&Value::String("invalid".to_string())).is_err());
    }

    #[test]
    fn validate_string_enum_all_without_to_dsl_requires_explicit_variant() {
        // When StringEnum goes through the protocol layer (external process
        // providers), to_dsl and namespace are lost. Without "all" as a direct
        // variant, it cannot be accepted (issue #1428).
        let without_all = AttributeType::StringEnum {
            name: String::new(),
            values: vec![
                "tcp".to_string(),
                "udp".to_string(),
                "icmp".to_string(),
                "icmpv6".to_string(),
                "-1".to_string(),
            ],
            namespace: None,
            to_dsl: None,
        };
        // Without "all" in values and no to_dsl alias, "all" is rejected
        assert!(
            without_all
                .validate(&Value::String("all".to_string()))
                .is_err()
        );

        // With "all" added to values, it is accepted even without to_dsl
        let with_all = AttributeType::StringEnum {
            name: String::new(),
            values: vec![
                "tcp".to_string(),
                "udp".to_string(),
                "icmp".to_string(),
                "icmpv6".to_string(),
                "-1".to_string(),
                "all".to_string(),
            ],
            namespace: None,
            to_dsl: None,
        };
        assert!(with_all.validate(&Value::String("all".to_string())).is_ok());
    }

    #[test]
    fn validate_string_enum_accepts_values_with_dots() {
        // Values like "ipsec.1" contain dots that should not be treated as
        // namespace separators (issue #611)
        let t = AttributeType::StringEnum {
            name: "Type".to_string(),
            values: vec!["ipsec.1".to_string()],
            namespace: Some("awscc.ec2.vpn_gateway".to_string()),
            to_dsl: None,
        };
        // Quoted string with dot should match directly
        assert!(t.validate(&Value::String("ipsec.1".to_string())).is_ok());
        // Fully qualified form should also be accepted
        assert!(
            t.validate(&Value::String(
                "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
            ))
            .is_ok()
        );
        // Invalid value should still be rejected
        assert!(t.validate(&Value::String("ipsec.2".to_string())).is_err());
    }

    #[test]
    fn validate_string_enum_rejects_double_namespace() {
        let t = AttributeType::StringEnum {
            name: "InstanceTenancy".to_string(),
            values: vec![
                "default".to_string(),
                "dedicated".to_string(),
                "host".to_string(),
            ],
            namespace: Some("awscc.ec2.vpc".to_string()),
            to_dsl: None,
        };
        // Double-namespace must be rejected
        assert!(
            t.validate(&Value::String(
                "awscc.ec2.vpc.InstanceTenancy.awscc.ec2.vpc.InstanceTenancy.default".to_string()
            ))
            .is_err()
        );
    }

    #[test]
    fn validate_float_type() {
        let t = AttributeType::Float;
        assert!(t.validate(&Value::Float(2.5)).is_ok());
        assert!(t.validate(&Value::Float(-0.5)).is_ok());
        assert!(t.validate(&Value::Int(42)).is_ok()); // integers are valid numbers
        assert!(t.validate(&Value::String("3.14".to_string())).is_err());
        assert!(t.validate(&Value::Bool(true)).is_err());
    }

    #[test]
    fn validate_float_rejects_non_finite() {
        let t = AttributeType::Float;
        assert!(t.validate(&Value::Float(f64::NAN)).is_err());
        assert!(t.validate(&Value::Float(f64::INFINITY)).is_err());
        assert!(t.validate(&Value::Float(f64::NEG_INFINITY)).is_err());
    }

    #[test]
    fn validate_int_rejects_float() {
        let t = AttributeType::Int;
        assert!(t.validate(&Value::Int(42)).is_ok());
        assert!(t.validate(&Value::Float(2.5)).is_err()); // strict integer typing
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
        let t = types::ipv4_cidr();

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
        let list_type = AttributeType::list(struct_type);

        let mut item = HashMap::new();
        item.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
        let list = Value::List(vec![Value::Map(item)]);
        assert!(list_type.validate(&list).is_ok());

        // Invalid item in list
        let bad_list = Value::List(vec![Value::Map(HashMap::new())]);
        assert!(list_type.validate(&bad_list).is_err());
    }

    #[test]
    fn struct_rejects_block_syntax_single_element() {
        // Block syntax produces Value::List([Value::Map(...)]) which should be rejected
        // for bare Struct attributes
        let struct_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields: vec![StructField::new("status", AttributeType::String).required()],
        };

        let mut map = HashMap::new();
        map.insert("status".to_string(), Value::String("Enabled".to_string()));
        let single_list = Value::List(vec![Value::Map(map)]);
        let result = struct_type.validate(&single_list);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            TypeError::BlockSyntaxNotAllowed { attribute } => {
                assert_eq!(attribute, "VersioningConfiguration");
            }
            other => panic!("Expected BlockSyntaxNotAllowed, got: {:?}", other),
        }
        assert!(
            err.to_string()
                .contains("cannot use block syntax; use map assignment")
        );
    }

    #[test]
    fn struct_rejects_block_syntax_multiple_elements() {
        // Multiple blocks for a bare Struct attribute should also be rejected
        let struct_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields: vec![StructField::new("status", AttributeType::String).required()],
        };

        let mut map1 = HashMap::new();
        map1.insert("status".to_string(), Value::String("Enabled".to_string()));
        let mut map2 = HashMap::new();
        map2.insert("status".to_string(), Value::String("Suspended".to_string()));
        let multi_list = Value::List(vec![Value::Map(map1), Value::Map(map2)]);
        let result = struct_type.validate(&multi_list);
        assert!(result.is_err());
        match result.unwrap_err() {
            TypeError::BlockSyntaxNotAllowed { attribute } => {
                assert_eq!(attribute, "VersioningConfiguration");
            }
            other => panic!("Expected BlockSyntaxNotAllowed, got: {:?}", other),
        }
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
    fn validate_cidr_accepts_both_ipv4_and_ipv6() {
        let t = types::cidr();

        // Valid IPv4 CIDRs
        assert!(
            t.validate(&Value::String("10.0.0.0/16".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("0.0.0.0/0".to_string())).is_ok());

        // Valid IPv6 CIDRs
        assert!(
            t.validate(&Value::String("2001:db8::/32".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("::/0".to_string())).is_ok());

        // Invalid
        assert!(
            t.validate(&Value::String("not-a-cidr".to_string()))
                .is_err()
        );
        assert!(t.validate(&Value::String("10.0.0.0".to_string())).is_err()); // no prefix
        assert!(t.validate(&Value::Int(42)).is_err());
    }

    #[test]
    fn custom_type_accepts_resource_ref() {
        // ResourceRef values resolve to strings at runtime, so Custom types should accept them
        let ipv4 = types::ipv4_cidr();
        assert!(
            ipv4.validate(&Value::resource_ref(
                "vpc".to_string(),
                "cidr_block".to_string(),
                vec![]
            ))
            .is_ok()
        );

        let ipv6 = types::ipv6_cidr();
        assert!(
            ipv6.validate(&Value::resource_ref(
                "subnet".to_string(),
                "ipv6_cidr".to_string(),
                vec![]
            ))
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

    #[test]
    fn validate_union_type() {
        // Create two Custom types that validate different prefixes
        let type_a = AttributeType::Custom {
            name: "TypeA".to_string(),
            base: Box::new(AttributeType::String),
            validate: |value| {
                if let Value::String(s) = value {
                    if s.starts_with("a-") {
                        Ok(())
                    } else {
                        Err(format!("Expected 'a-' prefix, got '{}'", s))
                    }
                } else {
                    Err("Expected string".to_string())
                }
            },
            namespace: None,
            to_dsl: None,
        };
        let type_b = AttributeType::Custom {
            name: "TypeB".to_string(),
            base: Box::new(AttributeType::String),
            validate: |value| {
                if let Value::String(s) = value {
                    if s.starts_with("b-") {
                        Ok(())
                    } else {
                        Err(format!("Expected 'b-' prefix, got '{}'", s))
                    }
                } else {
                    Err("Expected string".to_string())
                }
            },
            namespace: None,
            to_dsl: None,
        };

        let union_type = AttributeType::Union(vec![type_a, type_b]);

        // Valid: matches first member
        assert!(
            union_type
                .validate(&Value::String("a-12345678".to_string()))
                .is_ok()
        );
        // Valid: matches second member
        assert!(
            union_type
                .validate(&Value::String("b-12345678".to_string()))
                .is_ok()
        );
        // Invalid: matches neither
        assert!(
            union_type
                .validate(&Value::String("c-12345678".to_string()))
                .is_err()
        );
        // Valid: ResourceRef is accepted by Custom members
        assert!(
            union_type
                .validate(&Value::resource_ref(
                    "gw".to_string(),
                    "id".to_string(),
                    vec![]
                ))
                .is_ok()
        );
    }

    #[test]
    fn union_type_name() {
        let type_a = AttributeType::Custom {
            name: "TypeA".to_string(),
            base: Box::new(AttributeType::String),
            validate: |_| Ok(()),
            namespace: None,
            to_dsl: None,
        };
        let type_b = AttributeType::Custom {
            name: "TypeB".to_string(),
            base: Box::new(AttributeType::String),
            validate: |_| Ok(()),
            namespace: None,
            to_dsl: None,
        };

        let union_type = AttributeType::Union(vec![type_a, type_b]);
        assert_eq!(union_type.type_name(), "TypeA | TypeB");
    }

    #[test]
    fn union_accepts_type_name() {
        let type_a = AttributeType::Custom {
            name: "TypeA".to_string(),
            base: Box::new(AttributeType::String),
            validate: |_| Ok(()),
            namespace: None,
            to_dsl: None,
        };
        let type_b = AttributeType::Custom {
            name: "TypeB".to_string(),
            base: Box::new(AttributeType::String),
            validate: |_| Ok(()),
            namespace: None,
            to_dsl: None,
        };

        let union_type = AttributeType::Union(vec![type_a, type_b]);
        assert!(union_type.accepts_type_name("TypeA"));
        assert!(union_type.accepts_type_name("TypeB"));
        assert!(!union_type.accepts_type_name("TypeC"));

        // Non-union types
        let simple = AttributeType::String;
        assert!(simple.accepts_type_name("String"));
        assert!(!simple.accepts_type_name("Int"));
    }

    #[test]
    fn with_block_name_builder() {
        let attr = AttributeSchema::new("operating_regions", AttributeType::String)
            .with_block_name("operating_region");
        assert_eq!(attr.block_name.as_deref(), Some("operating_region"));
    }

    #[test]
    fn block_name_default_is_none() {
        let attr = AttributeSchema::new("name", AttributeType::String);
        assert!(attr.block_name.is_none());
    }

    #[test]
    fn block_name_map_returns_mapping() {
        let schema = ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::new("operating_regions", AttributeType::String)
                    .with_block_name("operating_region"),
            )
            .attribute(AttributeSchema::new("name", AttributeType::String));

        let map = schema.block_name_map();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("operating_region").unwrap(), "operating_regions");
    }

    #[test]
    fn block_name_map_empty_when_no_block_names() {
        let schema = ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::new("name", AttributeType::String));

        let map = schema.block_name_map();
        assert!(map.is_empty());
    }

    #[test]
    fn resolve_block_names_renames_key() {
        let mut resources = vec![{
            let mut r = Resource::new("ec2.ipam", "my-ipam");
            // Block syntax produces Value::List
            r.set_attr(
                "operating_region".to_string(),
                Value::List(vec![Value::Map({
                    let mut m = HashMap::new();
                    m.insert(
                        "region_name".to_string(),
                        Value::String("us-east-1".to_string()),
                    );
                    m
                })]),
            );
            r
        }];

        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.ipam".to_string(),
            ResourceSchema::new("ec2.ipam").attribute(
                AttributeSchema::new("operating_regions", AttributeType::String)
                    .with_block_name("operating_region"),
            ),
        );

        resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

        assert!(resources[0].attributes.contains_key("operating_regions"));
        assert!(!resources[0].attributes.contains_key("operating_region"));
    }

    #[test]
    fn resolve_block_names_noop_when_no_match() {
        let mut resources = vec![{
            let mut r = Resource::new("ec2.ipam", "my-ipam");
            r.set_attr("name".to_string(), Value::String("test".to_string()));
            r
        }];

        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.ipam".to_string(),
            ResourceSchema::new("ec2.ipam")
                .attribute(AttributeSchema::new("name", AttributeType::String)),
        );

        resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

        assert!(resources[0].attributes.contains_key("name"));
    }

    #[test]
    fn resolve_block_names_errors_on_mixed_syntax() {
        let mut resources = vec![{
            let mut r = Resource::new("ec2.ipam", "my-ipam");
            // Block syntax produces Value::List
            r.set_attr(
                "operating_region".to_string(),
                Value::List(vec![Value::Map({
                    let mut m = HashMap::new();
                    m.insert(
                        "region_name".to_string(),
                        Value::String("us-east-1".to_string()),
                    );
                    m
                })]),
            );
            // User also explicitly set the canonical name
            r.set_attr(
                "operating_regions".to_string(),
                Value::List(vec![Value::Map({
                    let mut m = HashMap::new();
                    m.insert(
                        "region_name".to_string(),
                        Value::String("us-west-2".to_string()),
                    );
                    m
                })]),
            );
            r
        }];

        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.ipam".to_string(),
            ResourceSchema::new("ec2.ipam").attribute(
                AttributeSchema::new("operating_regions", AttributeType::String)
                    .with_block_name("operating_region"),
            ),
        );

        let result = resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("operating_region"));
        assert!(err.contains("operating_regions"));
    }

    #[test]
    fn resolve_block_names_skips_unknown_schema() {
        let mut resources = vec![{
            let mut r = Resource::new("unknown.type", "test");
            r.set_attr(
                "operating_region".to_string(),
                Value::String("us-east-1".to_string()),
            );
            r
        }];

        let schemas = HashMap::new();

        // Should not error for unknown resource types
        resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

        // Key should remain unchanged
        assert!(resources[0].attributes.contains_key("operating_region"));
    }

    #[test]
    fn struct_field_with_block_name() {
        let field = StructField::new(
            "transitions",
            AttributeType::list(AttributeType::Struct {
                name: "Transition".to_string(),
                fields: vec![],
            }),
        )
        .with_block_name("transition");
        assert_eq!(field.block_name.as_deref(), Some("transition"));
    }

    #[test]
    fn resolve_block_names_nested_struct() {
        // Simulate: lifecycle_configuration = { transition { ... } }
        // where "transition" is the block name for "transitions" field
        let mut inner_map = HashMap::new();
        inner_map.insert(
            "transition".to_string(),
            Value::List(vec![Value::Map({
                let mut m = HashMap::new();
                m.insert(
                    "storage_class".to_string(),
                    Value::String("GLACIER".to_string()),
                );
                m
            })]),
        );

        let mut resources = vec![{
            let mut r = Resource::new("s3.bucket", "my-bucket");
            r.set_attr("lifecycle_configuration".to_string(), Value::Map(inner_map));
            r
        }];

        let mut schemas = HashMap::new();
        schemas.insert(
            "s3.bucket".to_string(),
            ResourceSchema::new("s3.bucket").attribute(AttributeSchema::new(
                "lifecycle_configuration",
                AttributeType::Struct {
                    name: "LifecycleConfiguration".to_string(),
                    fields: vec![
                        StructField::new(
                            "transitions",
                            AttributeType::list(AttributeType::Struct {
                                name: "Transition".to_string(),
                                fields: vec![],
                            }),
                        )
                        .with_block_name("transition"),
                    ],
                },
            )),
        );

        resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

        // The nested "transition" key should be renamed to "transitions"
        let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
            Some(Value::Map(m)) => m,
            _ => panic!("expected Map"),
        };
        assert!(
            lifecycle.contains_key("transitions"),
            "expected 'transitions' key after resolve"
        );
        assert!(
            !lifecycle.contains_key("transition"),
            "expected 'transition' key to be removed"
        );
    }

    #[test]
    fn resolve_block_names_singular_field_not_renamed_when_assigned() {
        // When a struct has both `transition` (Struct) and `transitions` (List(Struct))
        // with block_name("transition") on the List field, an attribute assignment
        // `transition = { ... }` (Value::Map) should NOT be renamed to `transitions`.
        // Only block syntax `transition { ... }` (Value::List) should be renamed.
        let mut inner_map = HashMap::new();
        // This is an attribute assignment: transition = { storage_class = "GLACIER" }
        // Parser produces Value::Map for attribute assignments
        inner_map.insert(
            "transition".to_string(),
            Value::Map({
                let mut m = HashMap::new();
                m.insert(
                    "storage_class".to_string(),
                    Value::String("GLACIER".to_string()),
                );
                m
            }),
        );

        let mut resources = vec![{
            let mut r = Resource::new("s3.bucket", "my-bucket");
            r.set_attr("lifecycle_configuration".to_string(), Value::Map(inner_map));
            r
        }];

        let mut schemas = HashMap::new();
        schemas.insert(
            "s3.bucket".to_string(),
            ResourceSchema::new("s3.bucket").attribute(AttributeSchema::new(
                "lifecycle_configuration",
                AttributeType::Struct {
                    name: "LifecycleConfiguration".to_string(),
                    fields: vec![
                        StructField::new(
                            "transition",
                            AttributeType::Struct {
                                name: "Transition".to_string(),
                                fields: vec![],
                            },
                        ),
                        StructField::new(
                            "transitions",
                            AttributeType::list(AttributeType::Struct {
                                name: "Transition".to_string(),
                                fields: vec![],
                            }),
                        )
                        .with_block_name("transition"),
                    ],
                },
            )),
        );

        resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

        let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
            Some(Value::Map(m)) => m,
            _ => panic!("expected Map"),
        };
        // The Value::Map should remain as "transition" (not renamed)
        assert!(
            lifecycle.contains_key("transition"),
            "expected 'transition' key to remain (attribute assignment)"
        );
        assert!(
            !lifecycle.contains_key("transitions"),
            "expected 'transitions' key NOT to be created from attribute assignment"
        );
    }

    #[test]
    fn resolve_block_names_block_syntax_renamed_when_singular_field_exists() {
        // Block syntax `transition { ... }` should still be renamed to `transitions`
        // even when a singular `transition` field exists in the schema.
        let mut inner_map = HashMap::new();
        // Block syntax produces Value::List
        inner_map.insert(
            "transition".to_string(),
            Value::List(vec![Value::Map({
                let mut m = HashMap::new();
                m.insert(
                    "storage_class".to_string(),
                    Value::String("GLACIER".to_string()),
                );
                m
            })]),
        );

        let mut resources = vec![{
            let mut r = Resource::new("s3.bucket", "my-bucket");
            r.set_attr("lifecycle_configuration".to_string(), Value::Map(inner_map));
            r
        }];

        let mut schemas = HashMap::new();
        schemas.insert(
            "s3.bucket".to_string(),
            ResourceSchema::new("s3.bucket").attribute(AttributeSchema::new(
                "lifecycle_configuration",
                AttributeType::Struct {
                    name: "LifecycleConfiguration".to_string(),
                    fields: vec![
                        StructField::new(
                            "transition",
                            AttributeType::Struct {
                                name: "Transition".to_string(),
                                fields: vec![],
                            },
                        ),
                        StructField::new(
                            "transitions",
                            AttributeType::list(AttributeType::Struct {
                                name: "Transition".to_string(),
                                fields: vec![],
                            }),
                        )
                        .with_block_name("transition"),
                    ],
                },
            )),
        );

        resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

        let lifecycle = match resources[0].get_attr("lifecycle_configuration") {
            Some(Value::Map(m)) => m,
            _ => panic!("expected Map"),
        };
        // Block syntax (Value::List) should be renamed to "transitions"
        assert!(
            lifecycle.contains_key("transitions"),
            "expected 'transitions' key after resolve (block syntax)"
        );
        assert!(
            !lifecycle.contains_key("transition"),
            "expected 'transition' key to be removed (block syntax renamed)"
        );
    }

    #[test]
    fn resolve_block_names_same_block_and_canonical_name() {
        // When block_name == canonical attribute name, block syntax should work
        // without triggering a false "cannot use both" error.
        // This regression was introduced in PR #913 and fixed in PR #917.
        let mut resources = vec![{
            let mut r = Resource::new("ec2.security_group", "my-sg");
            // Block syntax produces Value::List
            r.set_attr(
                "ingress".to_string(),
                Value::List(vec![Value::Map({
                    let mut m = HashMap::new();
                    m.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
                    m
                })]),
            );
            r
        }];

        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.security_group".to_string(),
            ResourceSchema::new("ec2.security_group").attribute(
                AttributeSchema::new(
                    "ingress",
                    AttributeType::list(AttributeType::Struct {
                        name: "Ingress".to_string(),
                        fields: vec![StructField::new("ip_protocol", AttributeType::String)],
                    }),
                )
                .with_block_name("ingress"),
            ),
        );

        // Should succeed without errors (block_name == canonical name, no rename needed)
        resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

        // Key should remain as "ingress"
        assert!(resources[0].attributes.contains_key("ingress"));
        // Value should be unchanged
        match resources[0].get_attr("ingress") {
            Some(Value::List(items)) => assert_eq!(items.len(), 1),
            other => panic!("expected List, got {:?}", other),
        }
    }

    #[test]
    fn resolve_block_names_same_block_and_canonical_name_multiple_items() {
        // When block_name == canonical name and the user provides multiple block
        // items (Value::List with multiple entries), no conflict should occur.
        // The key already exists (it IS the canonical key), so the `continue`
        // path handles it. This test verifies all items are preserved.
        let mut resources = vec![{
            let mut r = Resource::new("ec2.security_group", "my-sg");
            r.set_attr(
                "ingress".to_string(),
                Value::List(vec![
                    Value::Map({
                        let mut m = HashMap::new();
                        m.insert("ip_protocol".to_string(), Value::String("tcp".to_string()));
                        m
                    }),
                    Value::Map({
                        let mut m = HashMap::new();
                        m.insert("ip_protocol".to_string(), Value::String("udp".to_string()));
                        m
                    }),
                ]),
            );
            r
        }];

        let mut schemas = HashMap::new();
        schemas.insert(
            "ec2.security_group".to_string(),
            ResourceSchema::new("ec2.security_group").attribute(
                AttributeSchema::new(
                    "ingress",
                    AttributeType::list(AttributeType::Struct {
                        name: "Ingress".to_string(),
                        fields: vec![StructField::new("ip_protocol", AttributeType::String)],
                    }),
                )
                .with_block_name("ingress"),
            ),
        );

        // Should succeed; block_name == canonical name means no conflict possible
        resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

        assert!(resources[0].attributes.contains_key("ingress"));
        match resources[0].get_attr("ingress") {
            Some(Value::List(items)) => assert_eq!(items.len(), 2),
            other => panic!("expected List with 2 items, got {:?}", other),
        }
    }

    #[test]
    fn resolve_block_names_nested_same_block_and_canonical_name() {
        // Nested struct field where block_name == canonical field name.
        // Should resolve without errors.
        let mut inner_map = HashMap::new();
        inner_map.insert(
            "tag".to_string(),
            Value::List(vec![Value::Map({
                let mut m = HashMap::new();
                m.insert("key".to_string(), Value::String("Name".to_string()));
                m.insert("value".to_string(), Value::String("test".to_string()));
                m
            })]),
        );

        let mut resources = vec![{
            let mut r = Resource::new("test.resource", "my-resource");
            r.set_attr("config".to_string(), Value::Map(inner_map));
            r
        }];

        let mut schemas = HashMap::new();
        schemas.insert(
            "test.resource".to_string(),
            ResourceSchema::new("test.resource").attribute(AttributeSchema::new(
                "config",
                AttributeType::Struct {
                    name: "Config".to_string(),
                    fields: vec![
                        StructField::new(
                            "tag",
                            AttributeType::list(AttributeType::Struct {
                                name: "Tag".to_string(),
                                fields: vec![
                                    StructField::new("key", AttributeType::String),
                                    StructField::new("value", AttributeType::String),
                                ],
                            }),
                        )
                        .with_block_name("tag"),
                    ],
                },
            )),
        );

        // Should succeed without errors
        resolve_block_names(&mut resources, &schemas, |r| r.id.resource_type.clone()).unwrap();

        let config = match resources[0].get_attr("config") {
            Some(Value::Map(m)) => m,
            _ => panic!("expected Map"),
        };
        // Key should remain as "tag" (no rename needed since block_name == canonical)
        assert!(
            config.contains_key("tag"),
            "expected 'tag' key to remain (block_name == canonical name)"
        );
        match config.get("tag") {
            Some(Value::List(items)) => assert_eq!(items.len(), 1),
            other => panic!("expected List, got {:?}", other),
        }
    }

    #[test]
    fn test_operation_config_default() {
        let config = OperationConfig::default();
        assert_eq!(config.delete_timeout_secs, None);
        assert_eq!(config.delete_max_retries, None);
        assert_eq!(config.create_timeout_secs, None);
        assert_eq!(config.create_max_retries, None);
    }

    #[test]
    fn test_resource_schema_with_operation_config() {
        let schema =
            ResourceSchema::new("ec2.transit_gateway").with_operation_config(OperationConfig {
                delete_timeout_secs: Some(1800),
                delete_max_retries: Some(24),
                ..Default::default()
            });
        let config = schema.operation_config.unwrap();
        assert_eq!(config.delete_timeout_secs, Some(1800));
        assert_eq!(config.delete_max_retries, Some(24));
        assert_eq!(config.create_timeout_secs, None);
    }

    #[test]
    fn test_resource_schema_without_operation_config() {
        let schema = ResourceSchema::new("ec2.vpc");
        assert!(schema.operation_config.is_none());
    }

    #[test]
    fn validate_rejects_unknown_attribute() {
        let schema = ResourceSchema::new("s3.bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String));

        let mut attrs = HashMap::new();
        attrs.insert(
            "bucket_name".to_string(),
            Value::String("my-bucket".to_string()),
        );
        attrs.insert("tags".to_string(), Value::Map(HashMap::new()));

        let result = schema.validate(&attrs);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(matches!(&errors[0], TypeError::UnknownAttribute { name, .. } if name == "tags"));
    }

    #[test]
    fn validate_allows_known_attributes_only() {
        let schema = ResourceSchema::new("s3.bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String))
            .attribute(AttributeSchema::new(
                "tags",
                AttributeType::map(AttributeType::String),
            ));

        let mut attrs = HashMap::new();
        attrs.insert(
            "bucket_name".to_string(),
            Value::String("my-bucket".to_string()),
        );
        attrs.insert("tags".to_string(), Value::Map(HashMap::new()));

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn validate_unknown_attribute_with_suggestion() {
        let schema = ResourceSchema::new("s3.bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String));

        let mut attrs = HashMap::new();
        attrs.insert(
            "bukcet_name".to_string(),
            Value::String("my-bucket".to_string()),
        );

        let result = schema.validate(&attrs);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        match &errors[0] {
            TypeError::UnknownAttribute { name, suggestion } => {
                assert_eq!(name, "bukcet_name");
                assert_eq!(suggestion.as_deref(), Some("bucket_name"));
            }
            other => panic!("Expected UnknownAttribute, got: {:?}", other),
        }
    }

    #[test]
    fn validate_accepts_block_name_alias() {
        let schema = ResourceSchema::new("ec2.security_group").attribute(
            AttributeSchema::new(
                "ingress_rules",
                AttributeType::List {
                    inner: Box::new(AttributeType::String),
                    ordered: false,
                },
            )
            .with_block_name("ingress_rule"),
        );

        let mut attrs = HashMap::new();
        attrs.insert(
            "ingress_rule".to_string(),
            Value::List(vec![Value::String("rule1".to_string())]),
        );

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn validate_skips_internal_attributes() {
        let schema = ResourceSchema::new("s3.bucket")
            .attribute(AttributeSchema::new("bucket_name", AttributeType::String));

        let mut attrs = HashMap::new();
        attrs.insert(
            "bucket_name".to_string(),
            Value::String("my-bucket".to_string()),
        );
        attrs.insert("_binding".to_string(), Value::String("b".to_string()));

        assert!(schema.validate(&attrs).is_ok());
    }
}
