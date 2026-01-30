//! Resource - Representing resources and their state

use std::collections::HashMap;

use crate::parser::ResourceTypePath;

/// Unique identifier for a resource
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceId {
    /// Resource type (e.g., "s3_bucket", "ec2_instance")
    pub resource_type: String,
    /// Resource name (identifier specified in DSL)
    pub name: String,
}

impl ResourceId {
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            name: name.into(),
        }
    }
}

/// Attribute value of a resource
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Int(i64),
    Bool(bool),
    List(Vec<Value>),
    Map(HashMap<String, Value>),
    /// Reference to another resource's attribute (binding_name, attribute_name)
    ResourceRef(String, String),
    /// Typed reference to another resource's attribute with optional type information
    TypedResourceRef {
        /// Binding name of the referenced resource (e.g., "vpc", "web_sg")
        binding_name: String,
        /// Attribute name being referenced (e.g., "id", "name")
        attribute_name: String,
        /// Optional resource type for type checking (e.g., aws.vpc)
        resource_type: Option<ResourceTypePath>,
    },
    /// Unresolved identifier that will be resolved during schema validation
    /// This allows shorthand enum values like `dedicated` to be resolved to
    /// `aws.vpc.InstanceTenancy.dedicated` based on schema context.
    /// The tuple contains (identifier, optional_member) for forms like:
    /// - `dedicated` -> ("dedicated", None)
    /// - `InstanceTenancy.dedicated` -> ("InstanceTenancy", Some("dedicated"))
    UnresolvedIdent(String, Option<String>),
}

/// Desired state declared in DSL
#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    pub id: ResourceId,
    pub attributes: HashMap<String, Value>,
    /// If true, this is a data source (read-only) that won't be modified
    pub read_only: bool,
}

impl Resource {
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: ResourceId::new(resource_type, name),
            attributes: HashMap::new(),
            read_only: false,
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
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub id: ResourceId,
    pub attributes: HashMap<String, Value>,
    /// Whether this state exists
    pub exists: bool,
}

impl State {
    pub fn not_found(id: ResourceId) -> Self {
        Self {
            id,
            attributes: HashMap::new(),
            exists: false,
        }
    }

    pub fn existing(id: ResourceId, attributes: HashMap<String, Value>) -> Self {
        Self {
            id,
            attributes,
            exists: true,
        }
    }
}
