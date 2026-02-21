//! Resource - Representing resources and their state

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Unique identifier for a resource
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    /// Provider name (e.g., "aws", "awscc")
    pub provider: String,
    /// Resource type (e.g., "s3_bucket", "ec2_instance")
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

/// Lifecycle configuration for a resource
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LifecycleConfig {
    /// If true, force-delete the resource (e.g., empty S3 bucket before deletion)
    #[serde(default)]
    pub force_delete: bool,
    // Future: create_before_destroy, prevent_destroy (issue #150)
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
            ResourceId::with_provider("aws", "s3_bucket", "my-bucket"),
            attrs,
        )
        .with_identifier("my-bucket");

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: State = serde_json::from_str(&json).unwrap();
        assert_eq!(state, deserialized);
    }
}
