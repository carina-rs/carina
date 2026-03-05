//! State file structures for persisting infrastructure state

use carina_core::resource::{LifecycleConfig, Resource, ResourceId, Value};
use carina_core::value::json_to_dsl_value;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The main state file structure that persists to the backend
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    /// State file format version
    pub version: u32,
    /// Monotonically increasing number for each state modification
    pub serial: u64,
    /// Unique identifier for this state lineage (prevents accidental overwrites)
    pub lineage: String,
    /// Version of Carina that last modified this state
    pub carina_version: String,
    /// All managed resources and their current state
    pub resources: Vec<ResourceState>,
}

impl StateFile {
    /// Current state file format version
    /// v2: Added identifier field to ResourceState
    pub const CURRENT_VERSION: u32 = 2;

    /// Create a new empty state file
    pub fn new() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            serial: 0,
            lineage: uuid::Uuid::new_v4().to_string(),
            carina_version: env!("CARGO_PKG_VERSION").to_string(),
            resources: Vec::new(),
        }
    }

    /// Create a new state file with a specific lineage (for initialization)
    pub fn with_lineage(lineage: String) -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            serial: 0,
            lineage,
            carina_version: env!("CARGO_PKG_VERSION").to_string(),
            resources: Vec::new(),
        }
    }

    /// Increment serial and update carina version for a new state write
    pub fn increment_serial(&mut self) {
        self.serial += 1;
        self.carina_version = env!("CARGO_PKG_VERSION").to_string();
    }

    /// Find a resource by type and name
    pub fn find_resource(&self, resource_type: &str, name: &str) -> Option<&ResourceState> {
        self.resources
            .iter()
            .find(|r| r.resource_type == resource_type && r.name == name)
    }

    /// Find a resource mutably by type and name
    pub fn find_resource_mut(
        &mut self,
        resource_type: &str,
        name: &str,
    ) -> Option<&mut ResourceState> {
        self.resources
            .iter_mut()
            .find(|r| r.resource_type == resource_type && r.name == name)
    }

    /// Add or update a resource in the state
    pub fn upsert_resource(&mut self, resource: ResourceState) {
        if let Some(existing) = self.find_resource_mut(&resource.resource_type, &resource.name) {
            *existing = resource;
        } else {
            self.resources.push(resource);
        }
    }

    /// Get the identifier for a resource from state, falling back to the name attribute.
    pub fn get_identifier_for_resource(&self, resource: &Resource) -> Option<String> {
        if let Some(resource_state) =
            self.find_resource(&resource.id.resource_type, &resource.id.name)
        {
            return resource_state.identifier.clone();
        }
        if let Some(Value::String(name)) = resource.attributes.get("name") {
            return Some(name.clone());
        }
        None
    }

    /// Build a map of ResourceId -> LifecycleConfig from this state file.
    pub fn build_lifecycles(&self) -> HashMap<ResourceId, LifecycleConfig> {
        let mut lifecycles = HashMap::new();
        for rs in &self.resources {
            let id = ResourceId::with_provider(&rs.provider, &rs.resource_type, &rs.name);
            lifecycles.insert(id, rs.lifecycle.clone());
        }
        lifecycles
    }

    /// Build a map of saved attributes, converting JSON values to DSL values.
    pub fn build_saved_attrs(&self) -> HashMap<ResourceId, HashMap<String, Value>> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            let id = ResourceId::with_provider(&rs.provider, &rs.resource_type, &rs.name);
            let attrs: HashMap<String, Value> = rs
                .attributes
                .iter()
                .map(|(k, v)| (k.clone(), json_to_dsl_value(v)))
                .collect();
            result.insert(id, attrs);
        }
        result
    }

    /// Remove a resource from the state
    pub fn remove_resource(&mut self, resource_type: &str, name: &str) -> Option<ResourceState> {
        if let Some(pos) = self
            .resources
            .iter()
            .position(|r| r.resource_type == resource_type && r.name == name)
        {
            Some(self.resources.remove(pos))
        } else {
            None
        }
    }
}

impl Default for StateFile {
    fn default() -> Self {
        Self::new()
    }
}

/// State of a single managed resource
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceState {
    /// Resource type (e.g., "s3.bucket", "ec2.vpc")
    pub resource_type: String,
    /// Resource name (from the `name` attribute in DSL)
    pub name: String,
    /// Provider name (e.g., "aws")
    pub provider: String,
    /// AWS internal identifier (e.g., vpc-xxx, subnet-xxx)
    /// If None, the resource is considered not to exist
    #[serde(default)]
    pub identifier: Option<String>,
    /// All attributes of the resource as JSON values
    pub attributes: HashMap<String, serde_json::Value>,
    /// Whether this resource is protected from deletion (e.g., state bucket)
    #[serde(default)]
    pub protected: bool,
    /// Lifecycle configuration persisted from DSL
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    /// Attribute prefixes used to generate names (e.g., {"bucket_name": "my-app-"})
    #[serde(default)]
    pub prefixes: HashMap<String, String>,
}

impl ResourceState {
    /// Create a new resource state
    pub fn new(
        resource_type: impl Into<String>,
        name: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            resource_type: resource_type.into(),
            name: name.into(),
            provider: provider.into(),
            identifier: None,
            attributes: HashMap::new(),
            protected: false,
            lifecycle: LifecycleConfig::default(),
            prefixes: HashMap::new(),
        }
    }

    /// Set the identifier (AWS internal ID like vpc-xxx)
    pub fn with_identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    /// Set an attribute value
    pub fn with_attribute(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    /// Mark this resource as protected
    pub fn with_protected(mut self, protected: bool) -> Self {
        self.protected = protected;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_file_new() {
        let state = StateFile::new();
        assert_eq!(state.version, StateFile::CURRENT_VERSION);
        assert_eq!(state.serial, 0);
        assert!(!state.lineage.is_empty());
        assert!(state.resources.is_empty());
    }

    #[test]
    fn test_state_file_increment_serial() {
        let mut state = StateFile::new();
        assert_eq!(state.serial, 0);
        state.increment_serial();
        assert_eq!(state.serial, 1);
        state.increment_serial();
        assert_eq!(state.serial, 2);
    }

    #[test]
    fn test_state_file_upsert_resource() {
        let mut state = StateFile::new();

        let resource1 = ResourceState::new("s3.bucket", "my-bucket", "aws")
            .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"));

        state.upsert_resource(resource1);
        assert_eq!(state.resources.len(), 1);

        // Update the same resource
        let resource2 = ResourceState::new("s3.bucket", "my-bucket", "aws")
            .with_attribute("region".to_string(), serde_json::json!("us-west-2"));

        state.upsert_resource(resource2);
        assert_eq!(state.resources.len(), 1);
        assert_eq!(
            state.resources[0].attributes.get("region"),
            Some(&serde_json::json!("us-west-2"))
        );
    }

    #[test]
    fn test_state_file_remove_resource() {
        let mut state = StateFile::new();

        let resource = ResourceState::new("s3.bucket", "my-bucket", "aws");
        state.upsert_resource(resource);
        assert_eq!(state.resources.len(), 1);

        let removed = state.remove_resource("s3.bucket", "my-bucket");
        assert!(removed.is_some());
        assert_eq!(state.resources.len(), 0);

        // Removing non-existent resource returns None
        let removed = state.remove_resource("s3.bucket", "other-bucket");
        assert!(removed.is_none());
    }

    #[test]
    fn test_resource_state_protected() {
        let resource = ResourceState::new("s3.bucket", "state-bucket", "aws").with_protected(true);
        assert!(resource.protected);
    }

    #[test]
    fn test_state_file_serialization() {
        let mut state = StateFile::new();
        let resource = ResourceState::new("s3.bucket", "my-bucket", "aws")
            .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"))
            .with_attribute("versioning".to_string(), serde_json::json!("Enabled"));

        state.upsert_resource(resource);

        let json = serde_json::to_string_pretty(&state).unwrap();
        let deserialized: StateFile = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.version, state.version);
        assert_eq!(deserialized.serial, state.serial);
        assert_eq!(deserialized.lineage, state.lineage);
        assert_eq!(deserialized.resources.len(), 1);
    }

    #[test]
    fn test_resource_state_prefixes_serialization() {
        let mut resource = ResourceState::new("s3.bucket", "test-bucket", "awscc").with_attribute(
            "bucket_name".to_string(),
            serde_json::json!("my-app-abcd1234"),
        );
        resource
            .prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());

        let json = serde_json::to_string_pretty(&resource).unwrap();
        let deserialized: ResourceState = serde_json::from_str(&json).unwrap();

        assert_eq!(
            deserialized.prefixes.get("bucket_name"),
            Some(&"my-app-".to_string())
        );
    }

    #[test]
    fn test_get_identifier_for_resource_from_state() {
        use carina_core::resource::Resource;

        let mut state = StateFile::new();
        let rs = ResourceState::new("s3.bucket", "my-bucket", "awscc")
            .with_identifier("my-bucket-abcd1234");
        state.upsert_resource(rs);

        let resource = Resource::with_provider("awscc", "s3.bucket", "my-bucket");
        assert_eq!(
            state.get_identifier_for_resource(&resource),
            Some("my-bucket-abcd1234".to_string())
        );
    }

    #[test]
    fn test_get_identifier_for_resource_fallback_to_name_attr() {
        use carina_core::resource::{Resource, Value};

        let state = StateFile::new();

        let mut resource = Resource::with_provider("awscc", "s3.bucket", "my-bucket");
        resource.attributes.insert(
            "name".to_string(),
            Value::String("my-bucket-name".to_string()),
        );
        assert_eq!(
            state.get_identifier_for_resource(&resource),
            Some("my-bucket-name".to_string())
        );
    }

    #[test]
    fn test_get_identifier_for_resource_returns_none() {
        use carina_core::resource::Resource;

        let state = StateFile::new();
        let resource = Resource::with_provider("awscc", "s3.bucket", "my-bucket");
        assert_eq!(state.get_identifier_for_resource(&resource), None);
    }

    #[test]
    fn test_build_lifecycles() {
        use carina_core::resource::ResourceId;

        let mut state = StateFile::new();
        let mut rs = ResourceState::new("s3.bucket", "my-bucket", "awscc");
        rs.lifecycle.force_delete = true;
        state.upsert_resource(rs);

        let lifecycles = state.build_lifecycles();
        let id = ResourceId::with_provider("awscc", "s3.bucket", "my-bucket");
        assert!(lifecycles.get(&id).unwrap().force_delete);
    }

    #[test]
    fn test_build_saved_attrs() {
        use carina_core::resource::{ResourceId, Value};

        let mut state = StateFile::new();
        let rs = ResourceState::new("s3.bucket", "my-bucket", "awscc")
            .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"));
        state.upsert_resource(rs);

        let saved = state.build_saved_attrs();
        let id = ResourceId::with_provider("awscc", "s3.bucket", "my-bucket");
        let attrs = saved.get(&id).unwrap();
        assert_eq!(
            attrs.get("region"),
            Some(&Value::String("ap-northeast-1".to_string()))
        );
    }

    #[test]
    fn test_resource_state_backward_compatibility_without_prefixes() {
        // Simulate an old state file without the prefixes field
        let json = r#"{
            "resource_type": "s3.bucket",
            "name": "my-bucket",
            "provider": "aws",
            "attributes": {"region": "ap-northeast-1"},
            "protected": false
        }"#;

        let deserialized: ResourceState = serde_json::from_str(json).unwrap();
        assert!(deserialized.prefixes.is_empty());
    }
}
