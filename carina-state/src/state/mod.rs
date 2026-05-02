//! State file structures for persisting infrastructure state

use carina_core::deps::get_resource_dependencies;
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::value::{
    SecretHashContext, contains_secret, json_to_dsl_value, merge_secrets_into_provider_json,
    value_to_json,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

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
    /// Published exports for remote_state consumers
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub exports: HashMap<String, serde_json::Value>,
}

impl StateFile {
    /// Current state file format version
    /// v2: Added identifier field to ResourceState
    /// v3: Added binding and dependency_bindings fields to ResourceState
    /// v4: Instance path addressing (dot notation instead of underscore prefix)
    /// v5: Added exports field for remote_state output
    pub const CURRENT_VERSION: u32 = 5;

    /// Create a new empty state file
    pub fn new() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            serial: 0,
            lineage: uuid::Uuid::new_v4().to_string(),
            carina_version: env!("CARGO_PKG_VERSION").to_string(),
            resources: Vec::new(),
            exports: HashMap::new(),
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
            exports: HashMap::new(),
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
            resource.id.name_str(),
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
            resource.id.name_str(),
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
                dependency_bindings: rs.unwrap().dependency_bindings.clone(),
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
                    dependency_bindings: rs.dependency_bindings.clone(),
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
    ) -> HashMap<ResourceId, BTreeSet<String>> {
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
        // Return only exports — no fallback to let bindings
        self.exports
            .iter()
            .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
            .collect()
    }

    /// Rewrite every map-key address in the state to its canonical
    /// shape (#1903). State files written by older Carina builds may
    /// store `binding["key"]`; new emissions use `binding.key` (or
    /// `binding['key']` for non-identifier-safe keys). Running this on
    /// load lets old state resolve against new desired-state addresses
    /// without a `moved` block.
    pub fn canonicalize_addresses(&mut self) {
        use carina_core::utils::canonicalize_map_key_address;
        for r in &mut self.resources {
            r.name = canonicalize_map_key_address(&r.name);
            if let Some(b) = r.binding.as_ref() {
                r.binding = Some(canonicalize_map_key_address(b));
            }
            r.dependency_bindings = r
                .dependency_bindings
                .iter()
                .map(|d| canonicalize_map_key_address(d))
                .collect();
        }
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

    let mut state: StateFile = match check.version {
        v if v == StateFile::CURRENT_VERSION => serde_json::from_str(content).map_err(|e| {
            BackendError::InvalidState(format!("Failed to parse state file: {}", e))
        })?,
        v if v > StateFile::CURRENT_VERSION => {
            return Err(BackendError::InvalidState(format!(
                "State file version {} is newer than supported version {}. Please upgrade Carina.",
                v,
                StateFile::CURRENT_VERSION
            )));
        }
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
            state
        }
    };
    // Map-key addresses written under the legacy `["..."]` shape are
    // rewritten to the canonical form on read so existing state files
    // resolve cleanly against new emissions. See #1903.
    state.canonicalize_addresses();
    Ok(state)
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
    /// Resource type (e.g., "s3.Bucket", "ec2.Vpc")
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
    #[serde(default)]
    pub binding: Option<String>,
    /// Binding names of resources this resource depends on (via ResourceRef).
    /// Stored so orphan Delete effects can have tree structure.
    ///
    /// Set semantics (BTreeSet) — see Resource::dependency_bindings (#2228).
    /// Old state files persisted as JSON arrays continue to deserialize
    /// (serde transparently coerces array → BTreeSet, deduping any
    /// duplicates and re-serializing in sorted order on next write).
    #[serde(default)]
    pub dependency_bindings: BTreeSet<String>,
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
            dependency_bindings: BTreeSet::new(),
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
            resource.id.name_str(),
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
                let ctx =
                    SecretHashContext::new(resource.id.display_type(), resource.id.name_str(), k);
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
        // Store dependency bindings for tree structure in orphan Delete effects.
        // BTreeSet gives us dedup and sorted iteration for free (#2228).
        let deps = get_resource_dependencies(resource);
        if !deps.is_empty() {
            rs.dependency_bindings = deps.into_iter().collect();
        }
        Ok(rs)
    }
}

#[cfg(test)]
mod tests;
