//! State file structures for persisting infrastructure state

use carina_core::deps::get_resource_dependencies;
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::value::{
    SecretHashContext, contains_secret, json_to_dsl_value, merge_secrets_into_provider_json,
    value_to_json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::backend::BackendError;

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
    /// v3: Added binding and dependency_bindings fields to ResourceState
    /// v4: Instance path addressing (dot notation instead of underscore prefix)
    pub const CURRENT_VERSION: u32 = 4;

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

    /// Find a resource by provider, type, and name
    pub fn find_resource(
        &self,
        provider: &str,
        resource_type: &str,
        name: &str,
    ) -> Option<&ResourceState> {
        self.resources
            .iter()
            .find(|r| r.provider == provider && r.resource_type == resource_type && r.name == name)
    }

    /// Find all resources matching a provider and resource type
    pub fn resources_by_type(&self, provider: &str, resource_type: &str) -> Vec<&ResourceState> {
        self.resources
            .iter()
            .filter(|r| r.provider == provider && r.resource_type == resource_type)
            .collect()
    }

    /// Find a resource mutably by provider, type, and name
    pub fn find_resource_mut(
        &mut self,
        provider: &str,
        resource_type: &str,
        name: &str,
    ) -> Option<&mut ResourceState> {
        self.resources
            .iter_mut()
            .find(|r| r.provider == provider && r.resource_type == resource_type && r.name == name)
    }

    /// Add or update a resource in the state
    pub fn upsert_resource(&mut self, resource: ResourceState) {
        if let Some(existing) =
            self.find_resource_mut(&resource.provider, &resource.resource_type, &resource.name)
        {
            *existing = resource;
        } else {
            self.resources.push(resource);
        }
    }

    /// Get the identifier for a resource from state.
    pub fn get_identifier_for_resource(&self, resource: &Resource) -> Option<String> {
        if let Some(resource_state) = self.find_resource(
            &resource.id.provider,
            &resource.id.resource_type,
            &resource.id.name,
        ) {
            return resource_state.identifier.clone();
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
                .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                .collect();
            result.insert(id, attrs);
        }
        result
    }

    /// Build a map of desired attribute keys (user-specified in .crn) from this state file.
    pub fn build_desired_keys(&self) -> HashMap<ResourceId, Vec<String>> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            if !rs.desired_keys.is_empty() {
                let id = ResourceId::with_provider(&rs.provider, &rs.resource_type, &rs.name);
                result.insert(id, rs.desired_keys.clone());
            }
        }
        result
    }

    /// Build a `State` for a resource from the state file data.
    /// Returns a non-existing state if the resource is not found in the state file.
    pub fn build_state_for_resource(&self, resource: &Resource) -> State {
        let rs = self.find_resource(
            &resource.id.provider,
            &resource.id.resource_type,
            &resource.id.name,
        );
        if let Some(identifier) = rs.and_then(|r| r.identifier.as_deref()) {
            let attrs: HashMap<String, Value> = rs
                .unwrap()
                .attributes
                .iter()
                .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                .collect();
            return State {
                id: resource.id.clone(),
                identifier: Some(identifier.to_string()),
                attributes: attrs,
                exists: true,
            };
        }
        State::not_found(resource.id.clone())
    }

    /// Build state entries for resources tracked in the state file but absent from the
    /// desired resource set.  These "orphan" entries are injected into `current_states`
    /// so that `create_plan()` can detect them and emit Delete effects.
    pub fn build_orphan_states(
        &self,
        desired_ids: &std::collections::HashSet<ResourceId>,
    ) -> HashMap<ResourceId, State> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            let id = ResourceId::with_provider(&rs.provider, &rs.resource_type, &rs.name);
            if desired_ids.contains(&id) {
                continue;
            }
            // Only include resources that actually have an identifier (i.e. exist in infra)
            if let Some(ref identifier) = rs.identifier {
                let mut attrs: HashMap<String, Value> = rs
                    .attributes
                    .iter()
                    .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                    .collect();
                // Inject _binding so orphan Delete effects can have tree structure
                if let Some(ref binding) = rs.binding {
                    attrs.insert("_binding".to_string(), Value::String(binding.clone()));
                }
                let state = State {
                    id: id.clone(),
                    identifier: Some(identifier.clone()),
                    attributes: attrs,
                    exists: true,
                };
                result.insert(id, state);
            }
        }
        result
    }

    /// Build a map of ResourceId -> dependency binding names for orphaned resources.
    /// Used by the differ to set dependencies on orphan Delete effects.
    pub fn build_orphan_dependencies(
        &self,
        desired_ids: &std::collections::HashSet<ResourceId>,
    ) -> HashMap<ResourceId, Vec<String>> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            let id = ResourceId::with_provider(&rs.provider, &rs.resource_type, &rs.name);
            if desired_ids.contains(&id) {
                continue;
            }
            if rs.identifier.is_some() && !rs.dependency_bindings.is_empty() {
                result.insert(id, rs.dependency_bindings.clone());
            }
        }
        result
    }

    /// Build a map of ResourceId -> name overrides from this state file.
    /// Name overrides come from create_before_destroy with non-renameable attributes.
    pub fn build_name_overrides(&self) -> HashMap<ResourceId, HashMap<String, String>> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            if !rs.name_overrides.is_empty() {
                let id = ResourceId::with_provider(&rs.provider, &rs.resource_type, &rs.name);
                result.insert(id, rs.name_overrides.clone());
            }
        }
        result
    }

    /// Build a map of resource bindings for use as a remote state data source.
    ///
    /// Returns a map where each key is a resource binding name and the value is a
    /// `Value::Map` containing that resource's attributes (converted from JSON to DSL values).
    /// Resources without a binding name are skipped.
    ///
    /// This is used by `remote_state` blocks to expose another project's state
    /// as a nested map: `remote_binding.resource_binding.attribute`.
    pub fn build_remote_bindings(&self) -> HashMap<String, Value> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            if let Some(ref binding) = rs.binding {
                let attrs: HashMap<String, Value> = rs
                    .attributes
                    .iter()
                    .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                    .collect();
                result.insert(binding.clone(), Value::Map(attrs));
            }
        }
        result
    }

    /// Remove a resource from the state
    pub fn remove_resource(
        &mut self,
        provider: &str,
        resource_type: &str,
        name: &str,
    ) -> Option<ResourceState> {
        if let Some(pos) = self.resources.iter().position(|r| {
            r.provider == provider && r.resource_type == resource_type && r.name == name
        }) {
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

/// Minimal struct for extracting just the version field from a state file.
#[derive(Deserialize)]
struct VersionCheck {
    version: u32,
}

/// Deserialize a state file from a JSON string, checking the version and
/// migrating from older formats if necessary.
///
/// - Current version: deserialized directly.
/// - Future version (newer than supported): returns a clear error asking the
///   user to upgrade Carina.
/// - Older version: attempts deserialization with serde defaults and bumps
///   the version to current. When a future version introduces breaking changes,
///   explicit migration functions should be added here.
/// - Invalid JSON: returns a parse error.
pub fn check_and_migrate(content: &str) -> Result<StateFile, BackendError> {
    let check: VersionCheck = serde_json::from_str(content)
        .map_err(|e| BackendError::InvalidState(format!("Failed to parse state version: {}", e)))?;

    match check.version {
        v if v == StateFile::CURRENT_VERSION => serde_json::from_str(content)
            .map_err(|e| BackendError::InvalidState(format!("Failed to parse state file: {}", e))),
        v if v > StateFile::CURRENT_VERSION => Err(BackendError::InvalidState(format!(
            "State file version {} is newer than supported version {}. Please upgrade Carina.",
            v,
            StateFile::CURRENT_VERSION
        ))),
        v => {
            // Older version — for now, try to deserialize with serde defaults.
            // In the future, add explicit migration functions here.
            eprintln!(
                "Warning: Migrating state file from v{} to v{}",
                v,
                StateFile::CURRENT_VERSION
            );
            let mut state: StateFile = serde_json::from_str(content).map_err(|e| {
                BackendError::InvalidState(format!(
                    "Failed to migrate state file from v{}: {}",
                    v, e
                ))
            })?;
            state.version = StateFile::CURRENT_VERSION;
            Ok(state)
        }
    }
}

/// Deserialize a state file from a byte slice, checking the version and
/// migrating from older formats if necessary.
///
/// This is the byte-slice equivalent of [`check_and_migrate`] for backends
/// that read raw bytes (e.g., S3).
pub fn check_and_migrate_bytes(bytes: &[u8]) -> Result<StateFile, BackendError> {
    let content = std::str::from_utf8(bytes)
        .map_err(|e| BackendError::InvalidState(format!("State file is not valid UTF-8: {}", e)))?;
    check_and_migrate(content)
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
    /// Permanent name overrides from create_before_destroy with non-renameable attributes.
    /// Maps attribute name to the permanent temporary name (e.g., {"role_name": "my-role-abc123"}).
    #[serde(default)]
    pub name_overrides: HashMap<String, String>,
    /// Attribute keys that were explicitly specified by the user in the .crn file.
    /// Used to detect attribute removal: if a key was in desired_keys but is now absent
    /// from the desired state, it means the user intentionally removed it.
    #[serde(default)]
    pub desired_keys: Vec<String>,
    /// The binding name for this resource (from `let` bindings in DSL).
    /// Stored so orphan Delete effects can have tree structure.
    pub binding: Option<String>,
    /// Binding names of resources this resource depends on (via ResourceRef).
    /// Stored so orphan Delete effects can have tree structure.
    pub dependency_bindings: Vec<String>,
    /// Attribute names that are write-only (not returned by the provider API).
    /// Their values are persisted from the user's desired state so that changes
    /// to write-only attributes can be detected on subsequent plans.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_only_attributes: Vec<String>,
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
            name_overrides: HashMap::new(),
            desired_keys: Vec::new(),
            binding: None,
            dependency_bindings: Vec::new(),
            write_only_attributes: Vec::new(),
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

    /// Populate attributes from a provider-returned State
    pub fn with_attributes_from_state(mut self, state: &State) -> Self {
        for (key, value) in &state.attributes {
            if let Ok(json_value) = value_to_json(value) {
                self.attributes.insert(key.clone(), json_value);
            }
        }
        if let Some(identifier) = &state.identifier {
            self.identifier = Some(identifier.clone());
        }
        self
    }

    /// Mark this resource as protected
    pub fn with_protected(mut self, protected: bool) -> Self {
        self.protected = protected;
        self
    }

    /// Merge write-only attribute values from the desired resource into this state.
    ///
    /// Write-only attributes are not returned by the provider API after create/update,
    /// so their user-specified values must be persisted from the desired resource.
    /// This enables the differ to detect changes to write-only attribute values on
    /// subsequent plans.
    ///
    /// `write_only_keys` is the set of attribute names that are marked write-only
    /// in the resource schema.
    pub fn merge_write_only_attributes(&mut self, resource: &Resource, write_only_keys: &[String]) {
        let mut merged = Vec::new();
        for key in write_only_keys {
            // Only merge if the user specified this attribute and it's not already
            // in the provider-returned state
            if let Some(value) = resource.attributes.get(key)
                && !self.attributes.contains_key(key)
                && let Ok(json_value) = value_to_json(value)
            {
                self.attributes.insert(key.clone(), json_value);
                merged.push(key.clone());
            }
        }
        merged.sort();
        self.write_only_attributes = merged;
    }

    /// Build a ResourceState from a Resource and its provider-returned State.
    ///
    /// If `existing` is provided, the `protected` flag is preserved from it.
    ///
    /// Returns an error if any attribute value cannot be converted to JSON
    /// (e.g., non-finite float values).
    pub fn from_provider_state(
        resource: &Resource,
        state: &State,
        existing: Option<&ResourceState>,
    ) -> Result<Self, String> {
        let mut rs = Self::new(
            &resource.id.resource_type,
            &resource.id.name,
            resource.id.provider.clone(),
        );
        rs.identifier = state.identifier.clone();
        for (k, v) in &state.attributes {
            rs.attributes.insert(k.clone(), value_to_json(v)?);
        }
        // For secret attributes, override the provider-returned plain value
        // with the Argon2id hash. The provider returns the actual value (since
        // secrets are unwrapped before sending), but state should only store
        // the hash to avoid persisting sensitive data.
        // For nested secrets (inside Maps/Lists), merge the hashed values into
        // the provider-returned structure to preserve extra keys from the provider.
        for (k, v) in &resource.attributes {
            if contains_secret(v) {
                let ctx = SecretHashContext::new(resource.id.display_type(), &resource.id.name, k);
                if let Some(provider_json) = rs.attributes.get(k).cloned() {
                    rs.attributes.insert(
                        k.clone(),
                        merge_secrets_into_provider_json(v, &provider_json, Some(&ctx))?,
                    );
                } else {
                    rs.attributes.insert(
                        k.clone(),
                        carina_core::value::value_to_json_with_context(v, Some(&ctx))?,
                    );
                }
            }
        }
        if let Some(existing) = existing {
            rs.protected = existing.protected;
            rs.name_overrides = existing.name_overrides.clone();
        }
        rs.lifecycle = resource.lifecycle.clone();
        rs.prefixes = resource.prefixes.clone();
        // Record which attributes the user explicitly specified in their .crn file
        rs.desired_keys = resource
            .attributes
            .keys()
            .filter(|k| !k.starts_with('_'))
            .cloned()
            .collect();
        rs.desired_keys.sort();
        // Store binding name for tree structure in orphan Delete effects
        rs.binding = resource.binding.clone();
        // Store dependency bindings for tree structure in orphan Delete effects
        let deps = get_resource_dependencies(resource);
        if !deps.is_empty() {
            let mut dep_list: Vec<String> = deps.into_iter().collect();
            dep_list.sort();
            rs.dependency_bindings = dep_list;
        }
        Ok(rs)
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

        let removed = state.remove_resource("aws", "s3.bucket", "my-bucket");
        assert!(removed.is_some());
        assert_eq!(state.resources.len(), 0);

        // Removing non-existent resource returns None
        let removed = state.remove_resource("aws", "s3.bucket", "other-bucket");
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
    fn test_resource_state_serialization_with_binding_and_deps() {
        let json = r#"{
            "resource_type": "s3.bucket",
            "name": "my-bucket",
            "provider": "aws",
            "attributes": {"region": "ap-northeast-1"},
            "protected": false,
            "lifecycle": {},
            "prefixes": {},
            "name_overrides": {},
            "desired_keys": [],
            "binding": "my_bucket",
            "dependency_bindings": ["vpc", "subnet"]
        }"#;

        let deserialized: ResourceState = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.binding, Some("my_bucket".to_string()));
        assert_eq!(deserialized.dependency_bindings, vec!["vpc", "subnet"]);
    }

    #[test]
    fn test_from_provider_state() {
        use carina_core::resource::{Resource, State as ProviderState, Value};

        let mut resource = Resource::with_provider("awscc", "s3.bucket", "my-bucket");
        resource.lifecycle.force_delete = true;
        resource
            .prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());

        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("my-bucket-abcd1234".to_string()),
            attributes: [(
                "region".to_string(),
                Value::String("ap-northeast-1".to_string()),
            )]
            .into_iter()
            .collect(),
            exists: true,
        };

        let existing = ResourceState::new("s3.bucket", "my-bucket", "awscc").with_protected(true);

        let rs = ResourceState::from_provider_state(&resource, &provider_state, Some(&existing))
            .unwrap();

        assert_eq!(rs.identifier, Some("my-bucket-abcd1234".to_string()));
        assert_eq!(
            rs.attributes.get("region"),
            Some(&serde_json::json!("ap-northeast-1"))
        );
        assert!(rs.protected);
        assert!(rs.lifecycle.force_delete);
        assert_eq!(rs.prefixes.get("bucket_name"), Some(&"my-app-".to_string()));
    }

    #[test]
    fn test_from_provider_state_without_existing() {
        use carina_core::resource::{Resource, State as ProviderState, Value};

        let resource = Resource::with_provider("aws", "s3.bucket", "test");
        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("test-id".to_string()),
            attributes: [("name".to_string(), Value::String("test".to_string()))]
                .into_iter()
                .collect(),
            exists: true,
        };

        let rs = ResourceState::from_provider_state(&resource, &provider_state, None).unwrap();
        assert!(!rs.protected);
        assert_eq!(rs.identifier, Some("test-id".to_string()));
    }

    #[test]
    fn test_multi_provider_resources_do_not_collide() {
        use carina_core::resource::Resource;

        let mut state = StateFile::new();

        // Store two resources with the same resource_type and name but different providers
        let aws_resource =
            ResourceState::new("s3.bucket", "main", "aws").with_identifier("aws-bucket-id");
        let awscc_resource =
            ResourceState::new("s3.bucket", "main", "awscc").with_identifier("awscc-bucket-id");

        state.upsert_resource(aws_resource);
        state.upsert_resource(awscc_resource);

        // Both should be stored independently
        assert_eq!(state.resources.len(), 2);

        // find_resource should return the correct one for each provider
        let found_aws = state.find_resource("aws", "s3.bucket", "main").unwrap();
        assert_eq!(found_aws.identifier, Some("aws-bucket-id".to_string()));

        let found_awscc = state.find_resource("awscc", "s3.bucket", "main").unwrap();
        assert_eq!(found_awscc.identifier, Some("awscc-bucket-id".to_string()));

        // get_identifier_for_resource should return provider-scoped identifiers
        let aws_res = Resource::with_provider("aws", "s3.bucket", "main");
        assert_eq!(
            state.get_identifier_for_resource(&aws_res),
            Some("aws-bucket-id".to_string())
        );

        let awscc_res = Resource::with_provider("awscc", "s3.bucket", "main");
        assert_eq!(
            state.get_identifier_for_resource(&awscc_res),
            Some("awscc-bucket-id".to_string())
        );

        // Upsert should only update the matching provider's entry
        let updated_aws =
            ResourceState::new("s3.bucket", "main", "aws").with_identifier("aws-bucket-id-v2");
        state.upsert_resource(updated_aws);
        assert_eq!(state.resources.len(), 2);
        assert_eq!(
            state
                .find_resource("aws", "s3.bucket", "main")
                .unwrap()
                .identifier,
            Some("aws-bucket-id-v2".to_string())
        );
        assert_eq!(
            state
                .find_resource("awscc", "s3.bucket", "main")
                .unwrap()
                .identifier,
            Some("awscc-bucket-id".to_string())
        );

        // remove_resource should only remove the matching provider's entry
        let removed = state.remove_resource("aws", "s3.bucket", "main");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().provider, "aws");
        assert_eq!(state.resources.len(), 1);

        // The awscc entry should still exist
        assert!(state.find_resource("awscc", "s3.bucket", "main").is_some());
        assert!(state.find_resource("aws", "s3.bucket", "main").is_none());
    }

    #[test]
    fn test_build_lifecycles_provider_scoped() {
        use carina_core::resource::ResourceId;

        let mut state = StateFile::new();
        let mut aws_rs = ResourceState::new("s3.bucket", "main", "aws");
        aws_rs.lifecycle.force_delete = true;
        let awscc_rs = ResourceState::new("s3.bucket", "main", "awscc");

        state.upsert_resource(aws_rs);
        state.upsert_resource(awscc_rs);

        let lifecycles = state.build_lifecycles();
        let aws_id = ResourceId::with_provider("aws", "s3.bucket", "main");
        let awscc_id = ResourceId::with_provider("awscc", "s3.bucket", "main");

        assert!(lifecycles.get(&aws_id).unwrap().force_delete);
        assert!(!lifecycles.get(&awscc_id).unwrap().force_delete);
    }

    #[test]
    fn test_build_saved_attrs_provider_scoped() {
        use carina_core::resource::{ResourceId, Value};

        let mut state = StateFile::new();
        let aws_rs = ResourceState::new("s3.bucket", "main", "aws")
            .with_attribute("region".to_string(), serde_json::json!("us-east-1"));
        let awscc_rs = ResourceState::new("s3.bucket", "main", "awscc")
            .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"));

        state.upsert_resource(aws_rs);
        state.upsert_resource(awscc_rs);

        let saved = state.build_saved_attrs();
        let aws_id = ResourceId::with_provider("aws", "s3.bucket", "main");
        let awscc_id = ResourceId::with_provider("awscc", "s3.bucket", "main");

        assert_eq!(
            saved.get(&aws_id).unwrap().get("region"),
            Some(&Value::String("us-east-1".to_string()))
        );
        assert_eq!(
            saved.get(&awscc_id).unwrap().get("region"),
            Some(&Value::String("ap-northeast-1".to_string()))
        );
    }

    #[test]
    fn test_build_state_for_resource_existing() {
        use carina_core::resource::{Resource, Value};

        let mut state = StateFile::new();
        state.upsert_resource(
            ResourceState::new("s3.bucket", "my-bucket", "awscc")
                .with_identifier("my-bucket-id")
                .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1")),
        );

        let resource = Resource::with_provider("awscc", "s3.bucket", "my-bucket");
        let result = state.build_state_for_resource(&resource);

        assert!(result.exists);
        assert_eq!(result.identifier, Some("my-bucket-id".to_string()));
        assert_eq!(
            result.attributes.get("region"),
            Some(&Value::String("ap-northeast-1".to_string()))
        );
    }

    #[test]
    fn test_build_state_for_resource_not_found() {
        let state = StateFile::new();
        let resource =
            carina_core::resource::Resource::with_provider("awscc", "s3.bucket", "missing");
        let result = state.build_state_for_resource(&resource);

        assert!(!result.exists);
        assert!(result.identifier.is_none());
        assert!(result.attributes.is_empty());
    }

    #[test]
    fn test_build_state_for_resource_without_identifier() {
        let mut state = StateFile::new();
        // Resource in state but without identifier (not yet created)
        state.upsert_resource(
            ResourceState::new("s3.bucket", "pending", "awscc")
                .with_attribute("region".to_string(), serde_json::json!("us-east-1")),
        );

        let resource =
            carina_core::resource::Resource::with_provider("awscc", "s3.bucket", "pending");
        let result = state.build_state_for_resource(&resource);

        assert!(!result.exists);
        assert!(result.identifier.is_none());
    }

    #[test]
    fn test_from_provider_state_stores_binding_and_dependencies() {
        use carina_core::resource::{Resource, State as ProviderState, Value};

        let mut resource = Resource::with_provider("awscc", "ec2.subnet", "my-subnet");
        resource.binding = Some("my_subnet".to_string());
        resource.set_attr(
            "vpc_id".to_string(),
            Value::resource_ref("my_vpc".to_string(), "vpc_id".to_string(), vec![]),
        );

        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("subnet-123".to_string()),
            attributes: [("vpc_id".to_string(), Value::String("vpc-abc".to_string()))]
                .into_iter()
                .collect(),
            exists: true,
        };

        let rs = ResourceState::from_provider_state(&resource, &provider_state, None).unwrap();
        assert_eq!(rs.binding, Some("my_subnet".to_string()));
        assert_eq!(rs.dependency_bindings, vec!["my_vpc".to_string()]);
    }

    #[test]
    fn test_build_orphan_states_injects_binding() {
        use carina_core::resource::{ResourceId, Value};

        let mut state = StateFile::new();
        let mut rs = ResourceState::new("ec2.subnet", "orphan-subnet", "awscc")
            .with_identifier("subnet-123");
        rs.binding = Some("my_subnet".to_string());
        rs.dependency_bindings = vec!["my_vpc".to_string()];
        state.upsert_resource(rs);

        let desired_ids = std::collections::HashSet::new();
        let orphans = state.build_orphan_states(&desired_ids);

        let id = ResourceId::with_provider("awscc", "ec2.subnet", "orphan-subnet");
        let orphan_state = orphans.get(&id).unwrap();
        assert!(orphan_state.exists);
        assert_eq!(
            orphan_state.attributes.get("_binding"),
            Some(&Value::String("my_subnet".to_string()))
        );
    }

    #[test]
    fn test_build_orphan_dependencies() {
        use carina_core::resource::ResourceId;

        let mut state = StateFile::new();
        let mut rs = ResourceState::new("ec2.subnet", "orphan-subnet", "awscc")
            .with_identifier("subnet-123");
        rs.binding = Some("my_subnet".to_string());
        rs.dependency_bindings = vec!["my_vpc".to_string()];
        state.upsert_resource(rs);

        let desired_ids = std::collections::HashSet::new();
        let deps = state.build_orphan_dependencies(&desired_ids);

        let id = ResourceId::with_provider("awscc", "ec2.subnet", "orphan-subnet");
        assert_eq!(deps.get(&id).unwrap(), &vec!["my_vpc".to_string()]);
    }

    #[test]
    fn test_state_file_version_is_v4() {
        let state = StateFile::new();
        assert_eq!(state.version, 4);
    }

    #[test]
    fn test_build_orphan_dependencies_excludes_desired() {
        use carina_core::resource::ResourceId;

        let mut state = StateFile::new();
        let mut rs =
            ResourceState::new("ec2.subnet", "kept-subnet", "awscc").with_identifier("subnet-456");
        rs.dependency_bindings = vec!["my_vpc".to_string()];
        state.upsert_resource(rs);

        let id = ResourceId::with_provider("awscc", "ec2.subnet", "kept-subnet");
        let mut desired_ids = std::collections::HashSet::new();
        desired_ids.insert(id.clone());

        let deps = state.build_orphan_dependencies(&desired_ids);
        assert!(deps.is_empty());
    }

    #[test]
    fn test_check_and_migrate_current_version() {
        use super::check_and_migrate;

        let state = StateFile::new();
        let json = serde_json::to_string_pretty(&state).unwrap();
        let result = check_and_migrate(&json).unwrap();
        assert_eq!(result.version, StateFile::CURRENT_VERSION);
        assert_eq!(result.lineage, state.lineage);
    }

    #[test]
    fn test_check_and_migrate_future_version_returns_error() {
        use super::check_and_migrate;

        let json = r#"{
            "version": 999,
            "serial": 0,
            "lineage": "test-lineage",
            "carina_version": "0.1.0",
            "resources": []
        }"#;

        let result = check_and_migrate(json);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("999"),
            "error should mention the unsupported version"
        );
        assert!(
            err.contains("Please upgrade Carina"),
            "error should suggest upgrading"
        );
    }

    #[test]
    fn test_check_and_migrate_older_version_migrates() {
        use super::check_and_migrate;

        // v3 state file — should be migrated to current version
        let json = r#"{
            "version": 3,
            "serial": 5,
            "lineage": "old-lineage",
            "carina_version": "0.0.1",
            "resources": []
        }"#;

        let result = check_and_migrate(json).unwrap();
        assert_eq!(
            result.version,
            StateFile::CURRENT_VERSION,
            "version should be bumped to current"
        );
        assert_eq!(result.serial, 5, "serial should be preserved");
        assert_eq!(result.lineage, "old-lineage", "lineage should be preserved");
    }

    #[test]
    fn test_check_and_migrate_invalid_json_returns_error() {
        use super::check_and_migrate;

        let result = check_and_migrate("not valid json at all");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to parse state version"),
            "error should mention version parsing failure"
        );
    }

    #[test]
    fn test_check_and_migrate_bytes_works() {
        use super::check_and_migrate_bytes;

        let state = StateFile::new();
        let json = serde_json::to_string_pretty(&state).unwrap();
        let result = check_and_migrate_bytes(json.as_bytes()).unwrap();
        assert_eq!(result.version, StateFile::CURRENT_VERSION);
    }

    #[test]
    fn test_check_and_migrate_bytes_invalid_utf8() {
        use super::check_and_migrate_bytes;

        let bytes: &[u8] = &[0xff, 0xfe, 0xfd];
        let result = check_and_migrate_bytes(bytes);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("UTF-8"), "error should mention UTF-8 issue");
    }

    #[test]
    fn test_merge_write_only_attributes() {
        use carina_core::resource::{Resource, State as ProviderState, Value};

        // Simulate a VPC resource with a write-only attribute (ipv4_netmask_length)
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "my-vpc");
        resource.set_attr(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        resource.set_attr("ipv4_netmask_length".to_string(), Value::Int(16));

        // Provider returns state without write-only attributes (API doesn't return them)
        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("vpc-123".to_string()),
            attributes: [(
                "cidr_block".to_string(),
                Value::String("10.0.0.0/16".to_string()),
            )]
            .into_iter()
            .collect(),
            exists: true,
        };

        let mut rs = ResourceState::from_provider_state(&resource, &provider_state, None).unwrap();

        // Merge write-only attributes
        let write_only_keys = vec!["ipv4_netmask_length".to_string()];
        rs.merge_write_only_attributes(&resource, &write_only_keys);

        // The write-only attribute should be persisted in state
        assert_eq!(
            rs.attributes.get("ipv4_netmask_length"),
            Some(&serde_json::json!(16))
        );
        assert_eq!(rs.write_only_attributes, vec!["ipv4_netmask_length"]);

        // The regular attribute should still be there
        assert_eq!(
            rs.attributes.get("cidr_block"),
            Some(&serde_json::json!("10.0.0.0/16"))
        );
    }

    #[test]
    fn test_merge_write_only_attributes_not_in_desired() {
        use carina_core::resource::{Resource, State as ProviderState, Value};

        // Resource without write-only attribute specified
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "my-vpc");
        resource.set_attr(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("vpc-123".to_string()),
            attributes: [(
                "cidr_block".to_string(),
                Value::String("10.0.0.0/16".to_string()),
            )]
            .into_iter()
            .collect(),
            exists: true,
        };

        let mut rs = ResourceState::from_provider_state(&resource, &provider_state, None).unwrap();

        // Try to merge a write-only attribute that the user didn't specify
        let write_only_keys = vec!["ipv4_netmask_length".to_string()];
        rs.merge_write_only_attributes(&resource, &write_only_keys);

        // Should NOT be in state since user didn't specify it
        assert!(!rs.attributes.contains_key("ipv4_netmask_length"));
        assert!(rs.write_only_attributes.is_empty());
    }

    #[test]
    fn test_merge_write_only_skips_if_already_in_provider_state() {
        use carina_core::resource::{Resource, State as ProviderState, Value};

        // Resource with a write-only attribute
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "my-vpc");
        resource.set_attr(
            "some_attr".to_string(),
            Value::String("desired".to_string()),
        );

        // Provider happens to return this attribute (unusual for write-only but possible)
        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("vpc-123".to_string()),
            attributes: [(
                "some_attr".to_string(),
                Value::String("from-api".to_string()),
            )]
            .into_iter()
            .collect(),
            exists: true,
        };

        let mut rs = ResourceState::from_provider_state(&resource, &provider_state, None).unwrap();

        let write_only_keys = vec!["some_attr".to_string()];
        rs.merge_write_only_attributes(&resource, &write_only_keys);

        // Should keep the API-returned value, not overwrite with desired
        assert_eq!(
            rs.attributes.get("some_attr"),
            Some(&serde_json::json!("from-api"))
        );
        // Should NOT be recorded as write-only since the API returned it
        assert!(rs.write_only_attributes.is_empty());
    }

    #[test]
    fn test_write_only_attributes_serialization() {
        let mut rs = ResourceState::new("ec2.vpc", "my-vpc", "awscc")
            .with_identifier("vpc-123")
            .with_attribute("cidr_block".to_string(), serde_json::json!("10.0.0.0/16"))
            .with_attribute("ipv4_netmask_length".to_string(), serde_json::json!(16));
        rs.write_only_attributes = vec!["ipv4_netmask_length".to_string()];

        let json = serde_json::to_string_pretty(&rs).unwrap();
        let deserialized: ResourceState = serde_json::from_str(&json).unwrap();

        assert_eq!(
            deserialized.write_only_attributes,
            vec!["ipv4_netmask_length"]
        );
        assert_eq!(
            deserialized.attributes.get("ipv4_netmask_length"),
            Some(&serde_json::json!(16))
        );
    }

    #[test]
    fn test_write_only_attributes_omitted_when_empty() {
        let rs = ResourceState::new("s3.bucket", "my-bucket", "awscc");
        let json = serde_json::to_string(&rs).unwrap();

        // write_only_attributes should not appear in JSON when empty
        assert!(
            !json.contains("write_only_attributes"),
            "write_only_attributes should be omitted when empty"
        );
    }

    #[test]
    fn test_from_provider_state_secret_stored_as_hash() {
        use carina_core::resource::{Resource, State as ProviderState, Value};
        use carina_core::value::SECRET_PREFIX;

        let mut resource = Resource::with_provider("awscc", "rds.db_instance", "my-db");
        resource.set_attr(
            "master_password".to_string(),
            Value::Secret(Box::new(Value::String("my-password".to_string()))),
        );

        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("my-db-id".to_string()),
            // Provider returns the actual password (since secret was unwrapped before sending)
            attributes: [(
                "master_password".to_string(),
                Value::String("my-password".to_string()),
            )]
            .into_iter()
            .collect(),
            exists: true,
        };

        let rs = ResourceState::from_provider_state(&resource, &provider_state, None).unwrap();

        // State should store the hash, not the plain password
        let stored = rs
            .attributes
            .get("master_password")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(
            stored.starts_with(SECRET_PREFIX),
            "Expected secret hash, got: {}",
            stored
        );
        assert!(
            !stored.contains("my-password"),
            "State should not contain the plain password"
        );
    }

    #[test]
    fn test_from_provider_state_secret_in_map_stored_as_hash() {
        use carina_core::resource::{Resource, State as ProviderState, Value};
        use carina_core::value::SECRET_PREFIX;
        use std::collections::HashMap as StdHashMap;

        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "my-vpc");
        let mut tags_map = StdHashMap::new();
        tags_map.insert("Name".to_string(), Value::String("test".to_string()));
        tags_map.insert(
            "SecretTag".to_string(),
            Value::Secret(Box::new(Value::String("super-secret-value".to_string()))),
        );
        resource.set_attr("tags".to_string(), Value::Map(tags_map));

        let mut state_tags = StdHashMap::new();
        state_tags.insert("Name".to_string(), Value::String("test".to_string()));
        state_tags.insert(
            "SecretTag".to_string(),
            Value::String("super-secret-value".to_string()),
        );

        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("vpc-123".to_string()),
            attributes: [("tags".to_string(), Value::Map(state_tags))]
                .into_iter()
                .collect(),
            exists: true,
        };

        let rs = ResourceState::from_provider_state(&resource, &provider_state, None).unwrap();

        // The tags map in state should have the hash for SecretTag
        let tags_json = rs.attributes.get("tags").unwrap();
        let tags_obj = tags_json.as_object().unwrap();

        // Name should be plain
        assert_eq!(tags_obj.get("Name").unwrap().as_str().unwrap(), "test");

        // SecretTag should be stored as a hash, not the plain value
        let secret_stored = tags_obj.get("SecretTag").unwrap().as_str().unwrap();
        assert!(
            secret_stored.starts_with(SECRET_PREFIX),
            "Expected secret hash in map value, got: {}",
            secret_stored
        );
        assert!(
            !secret_stored.contains("super-secret-value"),
            "State should not contain the plain secret value in map"
        );
    }

    #[test]
    fn test_from_provider_state_secret_in_map_preserves_provider_extra_keys() {
        use carina_core::resource::{Resource, State as ProviderState, Value};
        use carina_core::value::SECRET_PREFIX;
        use std::collections::HashMap as StdHashMap;

        // User specifies only SecretTag in tags
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "my-vpc");
        let mut tags_map = StdHashMap::new();
        tags_map.insert(
            "SecretTag".to_string(),
            Value::Secret(Box::new(Value::String("super-secret-value".to_string()))),
        );
        resource.set_attr("tags".to_string(), Value::Map(tags_map));

        // Provider returns extra keys (e.g., CloudControl adds Name automatically)
        let mut state_tags = StdHashMap::new();
        state_tags.insert("Name".to_string(), Value::String("test".to_string()));
        state_tags.insert(
            "ExtraTag".to_string(),
            Value::String("extra-value".to_string()),
        );
        state_tags.insert(
            "SecretTag".to_string(),
            Value::String("super-secret-value".to_string()),
        );

        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("vpc-123".to_string()),
            attributes: [("tags".to_string(), Value::Map(state_tags))]
                .into_iter()
                .collect(),
            exists: true,
        };

        let rs = ResourceState::from_provider_state(&resource, &provider_state, None).unwrap();

        let tags_json = rs.attributes.get("tags").unwrap();
        let tags_obj = tags_json.as_object().unwrap();

        // Provider-only keys should be preserved from the provider state
        assert_eq!(tags_obj.get("Name").unwrap().as_str().unwrap(), "test");
        assert_eq!(
            tags_obj.get("ExtraTag").unwrap().as_str().unwrap(),
            "extra-value"
        );

        // SecretTag should be stored as a hash, not the plain value
        let secret_stored = tags_obj.get("SecretTag").unwrap().as_str().unwrap();
        assert!(
            secret_stored.starts_with(SECRET_PREFIX),
            "Expected secret hash in map value, got: {}",
            secret_stored
        );
        assert!(
            !secret_stored.contains("super-secret-value"),
            "State should not contain the plain secret value in map"
        );
    }

    #[test]
    fn test_from_provider_state_secret_in_list_stored_as_hash() {
        use carina_core::resource::{Resource, State as ProviderState, Value};
        use carina_core::value::SECRET_PREFIX;

        let mut resource = Resource::with_provider("awscc", "test.resource", "my-res");
        resource.set_attr(
            "values".to_string(),
            Value::List(vec![
                Value::String("public".to_string()),
                Value::Secret(Box::new(Value::String("secret-item".to_string()))),
            ]),
        );

        let provider_state = ProviderState {
            id: resource.id.clone(),
            identifier: Some("res-123".to_string()),
            attributes: [(
                "values".to_string(),
                Value::List(vec![
                    Value::String("public".to_string()),
                    Value::String("secret-item".to_string()),
                ]),
            )]
            .into_iter()
            .collect(),
            exists: true,
        };

        let rs = ResourceState::from_provider_state(&resource, &provider_state, None).unwrap();

        let values_json = rs.attributes.get("values").unwrap();
        let values_arr = values_json.as_array().unwrap();

        // First item should be plain
        assert_eq!(values_arr[0].as_str().unwrap(), "public");

        // Second item should be stored as a hash
        let secret_stored = values_arr[1].as_str().unwrap();
        assert!(
            secret_stored.starts_with(SECRET_PREFIX),
            "Expected secret hash in list value, got: {}",
            secret_stored
        );
    }
}
