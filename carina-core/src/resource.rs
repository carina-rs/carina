//! Resource - Representing resources and their state

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Unique identifier for a resource
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    /// Provider name (e.g., "aws", "awscc")
    pub provider: String,
    /// Resource type (e.g., "s3.bucket", "ec2.instance")
    pub resource_type: String,
    /// Resource name (identifier specified in DSL)
    pub name: String,
}

impl ResourceId {
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            provider: String::new(),
            resource_type: resource_type.into(),
            name: name.into(),
        }
    }

    pub fn with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
            name: name.into(),
        }
    }

    /// Returns the display type including provider prefix if available
    pub fn display_type(&self) -> String {
        if self.provider.is_empty() {
            self.resource_type.clone()
        } else {
            format!("{}.{}", self.provider, self.resource_type)
        }
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.provider.is_empty() {
            write!(f, "{}.{}", self.resource_type, self.name)
        } else {
            write!(f, "{}.{}.{}", self.provider, self.resource_type, self.name)
        }
    }
}

/// Attribute value of a resource
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<Value>),
    Map(HashMap<String, Value>),
    /// Reference to another resource's attribute
    ResourceRef {
        /// Binding name of the referenced resource (e.g., "vpc", "web_sg")
        binding_name: String,
        /// Attribute name being referenced (e.g., "id", "name")
        attribute_name: String,
    },
    /// Unresolved identifier that will be resolved during schema validation
    /// This allows shorthand enum values like `dedicated` to be resolved to
    /// `aws.vpc.InstanceTenancy.dedicated` based on schema context.
    /// The tuple contains (identifier, optional_member) for forms like:
    /// - `dedicated` -> ("dedicated", None)
    /// - `InstanceTenancy.dedicated` -> ("InstanceTenancy", Some("dedicated"))
    UnresolvedIdent(String, Option<String>),
}

impl Value {
    /// Semantic equality: for Lists, compares as multisets (order-insensitive);
    /// for Maps, compares values recursively with semantic equality;
    /// for all other variants, falls back to PartialEq.
    pub fn semantically_equal(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::List(a), Value::List(b)) => lists_equal(a, b),
            (Value::Map(a), Value::Map(b)) => maps_semantically_equal(a, b),
            _ => self == other,
        }
    }
}

/// Compare two maps using semantic equality for their values.
/// This ensures nested lists within maps are compared order-insensitively.
fn maps_semantically_equal(a: &HashMap<String, Value>, b: &HashMap<String, Value>) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .all(|(k, v)| b.get(k).map(|bv| v.semantically_equal(bv)).unwrap_or(false))
}

/// Multiset (bag) comparison for two lists of Values.
/// Returns true if both lists contain the same elements with the same multiplicities,
/// regardless of order. Uses O(n²) matching to handle non-hashable Values.
fn lists_equal(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut matched = vec![false; b.len()];
    for item_a in a {
        let mut found = false;
        for (j, item_b) in b.iter().enumerate() {
            if !matched[j] && item_a.semantically_equal(item_b) {
                matched[j] = true;
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// Lifecycle configuration for a resource
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LifecycleConfig {
    /// If true, force-delete the resource (e.g., empty S3 bucket before deletion)
    #[serde(default)]
    pub force_delete: bool,
    /// If true, create the new resource before destroying the old one during replacement
    #[serde(default)]
    pub create_before_destroy: bool,
}

/// Desired state declared in DSL
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    pub attributes: HashMap<String, Value>,
    /// If true, this is a data source (read-only) that won't be modified
    pub read_only: bool,
    /// Lifecycle meta-argument configuration
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    /// Attribute prefixes: maps attribute name -> prefix string
    /// e.g., {"bucket_name": "my-app-"} from `bucket_name_prefix = "my-app-"`
    #[serde(default)]
    pub prefixes: HashMap<String, String>,
}

impl Resource {
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: ResourceId::new(resource_type, name),
            attributes: HashMap::new(),
            read_only: false,
            lifecycle: LifecycleConfig::default(),
            prefixes: HashMap::new(),
        }
    }

    pub fn with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            id: ResourceId::with_provider(provider, resource_type, name),
            attributes: HashMap::new(),
            read_only: false,
            lifecycle: LifecycleConfig::default(),
            prefixes: HashMap::new(),
        }
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Returns true if this resource is a data source (read-only)
    pub fn is_data_source(&self) -> bool {
        self.read_only
    }
}

/// Current state fetched from actual infrastructure
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub id: ResourceId,
    /// AWS internal identifier (e.g., vpc-xxx, subnet-xxx)
    pub identifier: Option<String>,
    pub attributes: HashMap<String, Value>,
    /// Whether this state exists
    pub exists: bool,
}

impl State {
    pub fn not_found(id: ResourceId) -> Self {
        Self {
            id,
            identifier: None,
            attributes: HashMap::new(),
            exists: false,
        }
    }

    pub fn existing(id: ResourceId, attributes: HashMap<String, Value>) -> Self {
        Self {
            id,
            identifier: None,
            attributes,
            exists: true,
        }
    }

    pub fn with_identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_serde_round_trip() {
        let values = vec![
            Value::String("hello".to_string()),
            Value::Int(42),
            Value::Float(2.5),
            Value::Float(-0.5),
            Value::Bool(true),
            Value::List(vec![Value::String("a".to_string()), Value::Int(1)]),
            Value::Map(HashMap::from([
                ("key".to_string(), Value::String("val".to_string())),
                ("num".to_string(), Value::Int(10)),
            ])),
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "id".to_string(),
            },
            Value::ResourceRef {
                binding_name: "web_sg".to_string(),
                attribute_name: "id".to_string(),
            },
            Value::ResourceRef {
                binding_name: "bucket".to_string(),
                attribute_name: "arn".to_string(),
            },
            Value::UnresolvedIdent("dedicated".to_string(), None),
            Value::UnresolvedIdent("InstanceTenancy".to_string(), Some("dedicated".to_string())),
        ];

        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            let deserialized: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value, deserialized, "Round-trip failed for {:?}", value);
        }
    }

    #[test]
    fn resource_id_serde_round_trip() {
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "main-vpc");
        let json = serde_json::to_string(&id).unwrap();
        let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deserialized);
    }

    #[test]
    fn state_serde_round_trip() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-bucket".to_string()));
        attrs.insert("versioning".to_string(), Value::Bool(true));

        let state = State::existing(
            ResourceId::with_provider("aws", "s3.bucket", "my-bucket"),
            attrs,
        )
        .with_identifier("my-bucket");

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: State = serde_json::from_str(&json).unwrap();
        assert_eq!(state, deserialized);
    }

    #[test]
    fn lifecycle_config_serde_with_create_before_destroy() {
        let config = LifecycleConfig {
            force_delete: false,
            create_before_destroy: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: LifecycleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
        assert!(deserialized.create_before_destroy);
    }

    #[test]
    fn lifecycle_config_backward_compatible_deserialize() {
        // Old JSON without create_before_destroy field should deserialize with default (false)
        let json = r#"{"force_delete":true}"#;
        let config: LifecycleConfig = serde_json::from_str(json).unwrap();
        assert!(config.force_delete);
        assert!(!config.create_before_destroy);
    }

    #[test]
    fn semantically_equal_lists_same_order() {
        let a = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_different_order() {
        let a = Value::List(vec![Value::Int(3), Value::Int(1), Value::Int(2)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_different_content() {
        let a = Value::List(vec![Value::Int(1), Value::Int(2)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(3)]);
        assert!(!a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_different_lengths() {
        let a = Value::List(vec![Value::Int(1), Value::Int(2)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert!(!a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_with_duplicates() {
        let a = Value::List(vec![Value::Int(1), Value::Int(1), Value::Int(2)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(1)]);
        assert!(a.semantically_equal(&b));

        // Different multiplicities should not be equal
        let c = Value::List(vec![Value::Int(1), Value::Int(1), Value::Int(2)]);
        let d = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(2)]);
        assert!(!c.semantically_equal(&d));
    }

    #[test]
    fn semantically_equal_empty_lists() {
        let a = Value::List(vec![]);
        let b = Value::List(vec![]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_of_maps_different_order() {
        let mut map1 = HashMap::new();
        map1.insert("port".to_string(), Value::Int(80));
        map1.insert("protocol".to_string(), Value::String("tcp".to_string()));

        let mut map2 = HashMap::new();
        map2.insert("port".to_string(), Value::Int(443));
        map2.insert("protocol".to_string(), Value::String("tcp".to_string()));

        let a = Value::List(vec![Value::Map(map1.clone()), Value::Map(map2.clone())]);
        let b = Value::List(vec![Value::Map(map2), Value::Map(map1)]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_of_strings() {
        let a = Value::List(vec![
            Value::String("b".to_string()),
            Value::String("a".to_string()),
        ]);
        let b = Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_non_list_values() {
        // Non-list values should use regular equality
        assert!(Value::Int(42).semantically_equal(&Value::Int(42)));
        assert!(!Value::Int(42).semantically_equal(&Value::Int(43)));
        assert!(
            Value::String("hello".to_string())
                .semantically_equal(&Value::String("hello".to_string()))
        );
        assert!(Value::Bool(true).semantically_equal(&Value::Bool(true)));
    }

    #[test]
    fn semantically_equal_nested_lists() {
        // Lists inside maps are compared order-insensitively via recursive semantically_equal
        let mut map1 = HashMap::new();
        map1.insert(
            "ports".to_string(),
            Value::List(vec![Value::Int(80), Value::Int(443)]),
        );

        let mut map2 = HashMap::new();
        map2.insert(
            "ports".to_string(),
            Value::List(vec![Value::Int(443), Value::Int(80)]),
        );

        let a = Value::Map(map1);
        let b = Value::Map(map2);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_maps_different_keys() {
        let mut map1 = HashMap::new();
        map1.insert("a".to_string(), Value::Int(1));

        let mut map2 = HashMap::new();
        map2.insert("b".to_string(), Value::Int(1));

        assert!(!Value::Map(map1).semantically_equal(&Value::Map(map2)));
    }

    #[test]
    fn semantically_equal_maps_different_sizes() {
        let mut map1 = HashMap::new();
        map1.insert("a".to_string(), Value::Int(1));

        let mut map2 = HashMap::new();
        map2.insert("a".to_string(), Value::Int(1));
        map2.insert("b".to_string(), Value::Int(2));

        assert!(!Value::Map(map1).semantically_equal(&Value::Map(map2)));
    }
}
