//! State file structures for persisting infrastructure state

use carina_core::deps::get_resource_dependencies;
use carina_core::differ::Orphan;
use carina_core::explicit::{self, ExplicitFields};
pub use carina_core::name_override::{ApplyDecision, NameOverride, should_apply_override};
use carina_core::override_aware::NameOverrideSource;
pub use carina_core::provider::PreviousSecretHashAuthority;
use carina_core::provider::{
    EncodedStateRowAttributes, PersistedAttributes, PersistedAttributesError, RawSavedAttrs,
};
pub use carina_core::resource::DeposedKey;
use carina_core::resource::{
    ConcreteValue, Directives, PartialReadMarker, ResolvedResource, Resource, ResourceId,
    ResourceIdentity, State, Value,
};
use carina_core::schema::ResourceSchema;
use carina_core::value::{PersistedExportValue, PersistedValue, SerializationError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::backend::{BackendError, InvalidResourceIdentityError};

#[cfg(any(test, feature = "test-utils"))]
fn persisted_attributes_for_test_state_row(
    row_id: &ResourceId,
    attributes: HashMap<String, serde_json::Value>,
) -> PersistedAttributes {
    let encoded: EncodedStateRowAttributes = serde_json::from_value(serde_json::json!({
        "provider": row_id.provider,
        "resource_type": row_id.resource_type,
        "identity": row_id.identity_str().expect("test state row has a resolved identity"),
        "attributes": attributes,
    }))
    .expect("test state row uses the encoded state shape");
    let decoded = encoded
        .decode()
        .expect("test state row legacy salt is representable");
    let (_, _, _, attributes) = decoded.into_parts();
    attributes
}

/// A provider or desired value could not be encoded into persisted state.
#[derive(Debug, thiserror::Error)]
pub enum ResourceStateEncodingError {
    #[error(transparent)]
    Serialization(#[from] SerializationError),
    #[error(transparent)]
    PersistedAttributes(#[from] PersistedAttributesError),
}

/// The main state file structure that persists to the backend
#[derive(Debug, Clone, Serialize)]
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
    resources: Vec<ResourceState>,
    /// Published exports for remote_state consumers
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub exports: HashMap<String, PersistedExportValue>,
}

#[derive(Deserialize)]
struct UncheckedStateFile {
    version: u32,
    serial: u64,
    lineage: String,
    carina_version: String,
    resources: Vec<serde_json::Value>,
    #[serde(default)]
    exports: HashMap<String, serde_json::Value>,
}

struct ErrorSourceChain<'a>(&'a (dyn std::error::Error + 'static));

impl std::fmt::Display for ErrorSourceChain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(error) = source {
            write!(f, ": {error}")?;
            source = error.source();
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum StateFileDecodeError {
    Json(serde_json::Error),
    InvalidResourceIdentity(InvalidResourceIdentityError),
    Validation(String),
}

fn map_state_file_decode_error(
    error: StateFileDecodeError,
    migration_from: Option<u32>,
) -> BackendError {
    match error {
        StateFileDecodeError::InvalidResourceIdentity(error) => error.into(),
        StateFileDecodeError::Json(error) => match migration_from {
            Some(version) => BackendError::InvalidState(format!(
                "Failed to migrate state file from v{version}: {error}"
            )),
            None => BackendError::InvalidState(format!("Failed to parse state file: {error}")),
        },
        StateFileDecodeError::Validation(message) => match migration_from {
            Some(version) => BackendError::InvalidState(format!(
                "Failed to migrate state file from v{version}: {message}"
            )),
            None => BackendError::InvalidState(format!("Failed to parse state file: {message}")),
        },
    }
}

impl StateFile {
    /// Crate-private validated parse seam. `StateFile` intentionally has no
    /// public `Deserialize` impl: every external parse must go through the
    /// version gate ([`check_and_migrate`] or [`check_and_migrate_bytes`]), so
    /// consumers cannot read future-version or unmigrated state files
    /// (carina#3731). Parsing here still validates duplicate identities on
    /// every parse (carina#2858).
    pub(crate) fn from_json_str(content: &str) -> Result<Self, StateFileDecodeError> {
        let unchecked: UncheckedStateFile =
            serde_json::from_str(content).map_err(StateFileDecodeError::Json)?;
        let resources = unchecked
            .resources
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let identity = value
                    .get("identity")
                    .or_else(|| value.get("name"))
                    .and_then(serde_json::Value::as_str);
                if let Some(identity) = identity
                    && let Err(source) = ResourceIdentity::try_from(identity.to_owned())
                {
                    let provider = value
                        .get("provider")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("<missing>")
                        .to_owned();
                    let resource_type = value
                        .get("resource_type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("<missing>")
                        .to_owned();
                    let identifier = value
                        .get("identifier")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    return Err(StateFileDecodeError::InvalidResourceIdentity(
                        InvalidResourceIdentityError::new(
                            index,
                            provider,
                            resource_type,
                            identifier,
                            source,
                        ),
                    ));
                }
                serde_json::from_value(value).map_err(StateFileDecodeError::Json)
            })
            .collect::<Result<Vec<ResourceState>, StateFileDecodeError>>()?;
        let exports = unchecked
            .exports
            .into_iter()
            .map(|(name, value)| {
                serde_json::from_value(value)
                    .map(|value| (name.clone(), value))
                    .map_err(|error| {
                        StateFileDecodeError::Validation(format!(
                            "persisted export {name:?} is not valid persisted JSON: {error}"
                        ))
                    })
            })
            .collect::<Result<_, _>>()?;
        let state = Self {
            version: unchecked.version,
            serial: unchecked.serial,
            lineage: unchecked.lineage,
            carina_version: unchecked.carina_version,
            resources,
            exports,
        };
        state
            .validate_unique_identities()
            .map_err(StateFileDecodeError::Validation)?;
        Ok(state)
    }

    /// Current state file format version
    /// v2: Added identifier field to ResourceState
    /// v3: Added binding and dependency_bindings fields to ResourceState
    /// v4: Instance path addressing (dot notation instead of underscore prefix)
    /// v5: Added exports field for remote_state output
    /// v6: Replaced flat `desired_keys: Vec<String>` with recursive
    ///     `explicit: ExplicitFields` (refs awscc#206). Reads of v5
    ///     state lift each top-level key to a `Leaf` child of the
    ///     root `Struct`; the next plan/apply rebuilds a full tree
    ///     from the resource's authored `Value`.
    /// v7: Replaced top-level empty `ExplicitFields::Struct` with
    ///     `ExplicitFields::Unrecorded`.
    /// v8: Renamed `ResourceState.name` to `ResourceState.identity`.
    /// v9: Added `ResourceState.deposed` (pre-replacement instances
    ///     pending delete, including provider-instance routing); rows
    ///     with `identifier: None` are retained while `deposed` is
    ///     non-empty.
    /// v10: Added `ExplicitFields::ListElements`, aligned by index with the
    ///      stored list value in the same resource row.
    /// v11: Removed legacy host-reserved `_`-prefixed keys from current and
    ///      deposed provider attributes and from the top-level explicit tree.
    /// v12: Persisted secret markers carry their own salt. Raw v11 legacy
    ///      markers are upgraded while each resource row is deserialized.
    /// v13: Persisted canonical enums use the host-reserved `$enum` tag and
    ///      user map keys beginning with `$` are escaped with one extra `$`.
    pub const CURRENT_VERSION: u32 = 13;

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

    /// Create a fresh state file pre-seeded with a backend-owned state
    /// bucket recorded as a protected, managed resource. This is the seed
    /// the state backend writes after auto-creating its own storage —
    /// see [`ResourceState::managed_state_bucket`] for the shape.
    ///
    /// `resource_identity` must equal the desired resource's resolved identity
    /// (e.g. `aws_s3_bucket_<hash>` for the auto-injected anonymous block);
    /// `bucket_name` is the AWS bucket identifier the provider acts on.
    /// Conflating the two reproduces #2533.
    ///
    /// Single-resource by design: today only the S3 backend bootstraps a
    /// single storage resource. A backend that needs to seed multiple
    /// resources (e.g. a DynamoDB lock table alongside an S3 bucket)
    /// will need a different API.
    ///
    /// ```
    /// use carina_state::StateFile;
    ///
    /// let state = StateFile::with_managed_state_bucket(
    ///     "aws",
    ///     "s3.Bucket",
    ///     "aws_s3_bucket_a3f2b1c8",
    ///     "my-state-bucket",
    /// );
    /// assert_eq!(state.resources().len(), 1);
    /// assert_eq!(state.resources()[0].identity.as_str(), "aws_s3_bucket_a3f2b1c8");
    /// ```
    pub fn with_managed_state_bucket(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        resource_identity: impl Into<ResourceIdentity>,
        bucket_name: impl Into<String>,
    ) -> Self {
        let mut state = Self::new();
        state
            .upsert_resource(ResourceState::managed_state_bucket(
                provider,
                resource_type,
                resource_identity,
                bucket_name,
            ))
            .expect("fresh state file cannot contain duplicates");
        state
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

    pub(crate) fn validate_unique_identities(&self) -> Result<(), String> {
        validate_resource_identities(&self.resources)
    }

    /// Borrow all managed resource rows.
    ///
    /// Mutation is intentionally routed through the identity-preserving state
    /// APIs rather than exposing the backing vector.
    pub fn resources(&self) -> &[ResourceState] {
        &self.resources
    }

    /// Insert already-encoded export JSON in cross-crate tests while still
    /// enforcing the current persisted-value invariant.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn insert_export_json_for_test(
        &mut self,
        name: impl Into<String>,
        value: serde_json::Value,
    ) {
        self.exports.insert(
            name.into(),
            serde_json::from_value(value).expect("test export uses valid persisted JSON"),
        );
    }

    #[cfg(test)]
    pub(crate) fn from_unchecked_resources_for_test(resources: Vec<ResourceState>) -> Self {
        let mut state = Self::new();
        state.resources = resources;
        state
    }

    /// Find a resource by provider, type, and identity
    pub fn find_resource(
        &self,
        provider: &str,
        resource_type: &str,
        identity: &str,
    ) -> Option<&ResourceState> {
        self.resources.iter().find(|r| {
            r.provider == provider
                && r.resource_type == resource_type
                && r.identity.as_str() == identity
        })
    }

    /// Find the persisted row addressed by a full resource ID, including its
    /// provider-instance routing dimension.
    pub fn find_resource_for_id(&self, id: &ResourceId) -> Option<&ResourceState> {
        let identity = id.identity_str()?;
        self.find_resource(&id.provider, &id.resource_type, identity)
            .filter(|row| row.directives.provider_instance == id.provider_instance)
    }

    /// Find all resources matching a provider and resource type
    pub fn resources_by_type(&self, provider: &str, resource_type: &str) -> Vec<&ResourceState> {
        self.resources
            .iter()
            .filter(|r| r.provider == provider && r.resource_type == resource_type)
            .collect()
    }

    /// Add or update a resource in the state.
    ///
    /// Existing deposed generations are carried through this single row-write
    /// seam so callers cannot accidentally drop still-live old instances.
    /// Incoming deposed entries win on `(identifier, provider_instance)`, and
    /// any entry matching the current identifier is removed so the live current
    /// instance cannot also be scheduled as deposed.
    pub fn upsert_resource(&mut self, mut resource: ResourceState) -> Result<(), BackendError> {
        validate_deposed_identities(&resource).map_err(BackendError::InvalidState)?;
        if let Some(pos) = self.resources.iter().position(|existing| {
            existing.provider == resource.provider
                && existing.resource_type == resource.resource_type
                && existing.identity == resource.identity
        }) {
            let existing_deposed = self.resources[pos].deposed.clone();
            merge_deposed_generations(&mut resource, existing_deposed)
                .map_err(BackendError::InvalidState)?;
            remove_current_identifier_from_deposed(&mut resource);
            self.resources[pos] = resource;
        } else {
            remove_current_identifier_from_deposed(&mut resource);
            self.resources.push(resource);
        }
        Ok(())
    }

    /// Add or refresh one deposed generation through the row-write seam.
    ///
    /// Existing entries are matched on both their system-assigned key and
    /// `(identifier, provider_instance)`. The final write uses
    /// [`Self::upsert_resource`] so both deposed uniqueness axes and the
    /// invariant that a current identifier cannot also be deposed are applied
    /// uniformly.
    pub fn upsert_deposed_generation(
        &mut self,
        provider: &str,
        resource_type: &str,
        identity: &ResourceIdentity,
        row_provider_instance: Option<String>,
        instance: DeposedInstance,
    ) -> Result<(), BackendError> {
        let mut row = self
            .find_resource(provider, resource_type, identity.as_str())
            .cloned()
            .unwrap_or_else(|| {
                let mut row =
                    ResourceState::new(resource_type.to_string(), identity.clone(), provider);
                row.directives.provider_instance = row_provider_instance;
                row
            });

        let key_position = row
            .deposed
            .iter()
            .position(|entry| entry.key == instance.key);
        let identity_position = row.deposed.iter().position(|entry| {
            entry.identifier == instance.identifier
                && entry.provider_instance == instance.provider_instance
        });
        let replace_position = match (key_position, identity_position) {
            (Some(key_pos), Some(identity_pos)) if key_pos != identity_pos => {
                return Err(BackendError::InvalidState(format!(
                    "cannot upsert deposed generation key {:?} with identity \
                     (identifier={:?}, provider_instance={:?}) for resource \
                     (provider={provider:?}, resource_type={resource_type:?}, \
                    identity={:?}): the key and identity match different \
                     existing generations",
                    instance.key,
                    instance.identifier,
                    instance.provider_instance,
                    identity.as_str(),
                )));
            }
            (Some(position), _) | (_, Some(position)) => Some(position),
            (None, None) => None,
        };
        if let Some(position) = replace_position {
            row.deposed[position] = instance;
        } else {
            row.deposed.push(instance);
        }
        self.upsert_resource(row)
    }

    /// Rekey resource identities while preserving the state-wide uniqueness
    /// invariant.
    pub fn rename_resource_identities(
        &mut self,
        renames: &[(String, String)],
    ) -> Result<(), BackendError> {
        self.validate_unique_identities()
            .map_err(BackendError::InvalidState)?;
        let by_old: HashMap<&str, &str> = renames
            .iter()
            .map(|(old, new)| (old.as_str(), new.as_str()))
            .collect();
        let mut renamed = self.resources.clone();
        for resource in &mut renamed {
            if let Some(new_identity) = by_old.get(resource.identity.as_str()) {
                resource.identity = ResourceIdentity::try_from((*new_identity).to_owned())
                    .map_err(|error| BackendError::InvalidStateIdentity {
                        context: format!(
                            "cannot rename resource identity {:?} to {:?}",
                            resource.identity.as_str(),
                            new_identity
                        ),
                        error,
                    })?;
            }
        }
        validate_resource_identities(&renamed).map_err(BackendError::InvalidState)?;
        self.resources = renamed;
        Ok(())
    }

    /// Remove one deposed generation after that exact generation's Delete
    /// succeeded. If this leaves a shell row with no current identifier, the
    /// row is dropped.
    pub fn remove_deposed_generation(
        &mut self,
        provider: &str,
        resource_type: &str,
        identity: &str,
        key: &DeposedKey,
    ) -> Option<DeposedInstance> {
        let row_pos = self.resources.iter().position(|r| {
            r.provider == provider
                && r.resource_type == resource_type
                && r.identity.as_str() == identity
        })?;
        let deposed_pos = self.resources[row_pos]
            .deposed
            .iter()
            .position(|entry| &entry.key == key)?;
        let removed = self.resources[row_pos].deposed.remove(deposed_pos);
        if self.resources[row_pos].identifier.is_none()
            && self.resources[row_pos].deposed.is_empty()
        {
            self.resources.remove(row_pos);
        }
        Some(removed)
    }

    /// Get the identifier for a resource from state.
    pub fn get_identifier_for_resource(&self, resource: &Resource) -> Option<String> {
        let identity = resource.id.identity_str()?;
        if let Some(resource_state) =
            self.find_resource(&resource.id.provider, &resource.id.resource_type, identity)
        {
            return resource_state.identifier.clone();
        }
        None
    }

    /// Build a map of ResourceId -> Directives from this state file.
    pub fn build_directives(&self) -> HashMap<ResourceId, Directives> {
        let mut directives_map = HashMap::new();
        for rs in &self.resources {
            let id = Self::id_for_resource_state(rs);
            directives_map.insert(id, rs.directives.clone());
        }
        directives_map
    }

    fn id_for_resource_state(rs: &ResourceState) -> ResourceId {
        ResourceId::with_provider_identity(
            rs.provider.clone(),
            rs.resource_type.clone(),
            rs.identity.clone(),
            rs.directives.provider_instance.clone(),
        )
    }

    /// Build a map of saved attributes, converting JSON values to DSL values.
    pub fn build_saved_attrs(&self) -> RawSavedAttrs {
        let mut result = HashMap::new();
        for rs in &self.resources {
            let id = Self::id_for_resource_state(rs);
            result.insert(id, rs.persisted_attributes().clone());
        }
        RawSavedAttrs::from_persisted(result)
    }

    /// Restore partial-read markers from the state file onto refreshed states.
    pub fn restore_partial_read_markers(&self, states: &mut HashMap<ResourceId, State>) {
        for (id, current) in states.iter_mut() {
            if !current.exists {
                current.partial_read = None;
                continue;
            }
            let marker = id
                .identity_str()
                .and_then(|identity| self.find_resource(&id.provider, &id.resource_type, identity))
                .and_then(|rs| rs.partial_read.clone());
            if let Some(mut marker) = marker {
                marker
                    .missing_attributes
                    .retain(|attr| !current.attributes.contains_key(attr));
                if marker.missing_attributes.is_empty() {
                    current.partial_read = None;
                } else {
                    current.partial_read = Some(marker);
                }
            }
        }
    }

    /// Build a map of `ExplicitFields` trees (one per resource) recording
    /// which fields the user explicitly wrote in their `.crn`. The differ
    /// uses these trees both to detect attribute removals and to project
    /// the actual-state side before computing diffs (refs awscc#206).
    pub fn build_explicit(&self) -> HashMap<ResourceId, ExplicitFields> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            if !is_empty_explicit(&rs.explicit) {
                let id = Self::id_for_resource_state(rs);
                result.insert(id, rs.explicit.clone());
            }
        }
        result
    }

    /// Build a `State` for a resource id from the state file data.
    /// Returns a non-existing state if the resource is not found in the
    /// state file. Takes a `&ResourceId` so it works for managed
    /// resources and data sources alike (carina#3181).
    pub fn build_state_for_resource(&self, id: &ResourceId) -> State {
        let rs = id
            .identity_str()
            .and_then(|identity| self.find_resource(&id.provider, &id.resource_type, identity));
        if let Some(identifier) = rs.and_then(|r| r.identifier.as_deref()) {
            let rs = rs.expect("resource row was found above");
            let attrs = rs.persisted_attributes().to_values();
            return State {
                id: id.clone(),
                identifier: Some(identifier.to_string()),
                attributes: attrs,
                exists: true,
                dependency_bindings: rs.dependency_bindings.clone(),
                partial_read: rs.partial_read.clone(),
            };
        }
        State::not_found(id.clone())
    }

    /// Build each persisted orphan as one typed value containing its
    /// provider-facing state and host-owned binding/directive metadata.
    pub fn build_orphans(
        &self,
        desired_ids: &std::collections::HashSet<ResourceId>,
    ) -> carina_core::differ::Orphans {
        self.resources
            .iter()
            .filter_map(|rs| {
                let id = Self::id_for_resource_state(rs);
                if desired_ids.contains(&id) {
                    return None;
                }
                let identifier = rs.identifier.clone()?;
                let state = State {
                    id: id.clone(),
                    identifier: Some(identifier),
                    attributes: rs.persisted_attributes().to_values(),
                    exists: true,
                    dependency_bindings: rs.dependency_bindings.clone(),
                    partial_read: rs.partial_read.clone(),
                };
                Some(Orphan::new(
                    state,
                    rs.binding.clone(),
                    rs.directives.clone(),
                ))
            })
            .collect()
    }

    /// Build a map of ResourceId -> name overrides from this state file.
    /// Name overrides come from create_before_destroy with non-renameable attributes.
    pub fn build_name_overrides(&self) -> HashMap<ResourceId, HashMap<String, String>> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            if !rs.name_overrides.is_empty() {
                let id = Self::id_for_resource_state(rs);
                let overrides = rs
                    .name_overrides
                    .iter()
                    .map(|(attribute, override_)| (attribute.clone(), override_.temp_value.clone()))
                    .collect();
                result.insert(id, overrides);
            }
        }
        result
    }

    pub fn has_legacy_name_overrides(&self) -> bool {
        self.resources.iter().any(has_legacy_name_override)
    }

    pub fn legacy_name_override_resources(&self) -> Vec<&ResourceState> {
        self.resources
            .iter()
            .filter(|rs| has_legacy_name_override(rs))
            .collect()
    }

    /// Build a map of resource bindings for use as a remote state data source.
    ///
    /// Returns a map where each key is a resource binding name and the value is a
    /// `Value::Concrete(ConcreteValue::Map)` containing that resource's attributes (converted from JSON to DSL values).
    /// Resources without a binding name are skipped.
    ///
    /// This is used by `remote_state` blocks to expose another project's state
    /// as a nested map: `remote_binding.resource_binding.attribute`.
    pub fn build_remote_bindings(&self) -> HashMap<String, Value> {
        // Return only exports — no fallback to let bindings
        self.exports
            .iter()
            .filter_map(|(key, value)| value.decode().map(|value| (key.clone(), value)))
            .collect()
    }

    /// Rewrite every map-key address in the state to its canonical
    /// shape (#1903). State files written by older Carina builds may
    /// store `binding["key"]`; new emissions use `binding.key` (or
    /// `binding['key']` for non-identifier-safe keys). Running this on
    /// load lets old state resolve against new desired-state addresses
    /// without a `moved` block.
    fn canonicalize_addresses(&mut self) -> Result<(), BackendError> {
        use carina_core::utils::canonicalize_map_key_address;
        for r in &mut self.resources {
            r.identity =
                ResourceIdentity::try_from(canonicalize_map_key_address(r.identity.as_str()))
                    .map_err(|error| BackendError::InvalidStateIdentity {
                        context: format!(
                            "cannot canonicalize identity {:?} for {}.{}",
                            r.identity.as_str(),
                            r.provider,
                            r.resource_type
                        ),
                        error,
                    })?;
            if let Some(b) = r.binding.as_ref() {
                r.binding = Some(canonicalize_map_key_address(b));
            }
            r.dependency_bindings = r
                .dependency_bindings
                .iter()
                .map(|d| canonicalize_map_key_address(d))
                .collect();
        }
        Ok(())
    }

    /// Remove a resource's current instance from the state.
    ///
    /// Rows with deposed generations are retained so those still-live
    /// instances remain recorded until their own Deletes succeed.
    pub fn remove_resource(
        &mut self,
        provider: &str,
        resource_type: &str,
        identity: &str,
    ) -> Option<ResourceState> {
        if let Some(pos) = self.resources.iter().position(|r| {
            r.provider == provider
                && r.resource_type == resource_type
                && r.identity.as_str() == identity
        }) {
            if self.resources[pos].deposed.is_empty() {
                Some(self.resources.remove(pos))
            } else {
                let removed = self.resources[pos].clone();
                let deposed = std::mem::take(&mut self.resources[pos].deposed);
                self.resources[pos] = ResourceState::new(
                    removed.resource_type.clone(),
                    removed.identity.clone(),
                    removed.provider.clone(),
                );
                self.resources[pos].directives.provider_instance =
                    removed.directives.provider_instance.clone();
                self.resources[pos].deposed = deposed;
                Some(removed)
            }
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

fn validate_resource_identities(resources: &[ResourceState]) -> Result<(), String> {
    let mut resource_identities = HashSet::new();
    for resource in resources {
        if !resource_identities.insert((
            resource.provider.as_str(),
            resource.resource_type.as_str(),
            resource.identity.as_str(),
        )) {
            return Err(format!(
                "duplicate resource identity (provider={:?}, resource_type={:?}, identity={:?})",
                resource.provider, resource.resource_type, resource.identity
            ));
        }
        validate_deposed_identities(resource)?;
    }
    Ok(())
}

fn validate_deposed_identities(resource: &ResourceState) -> Result<(), String> {
    let mut keys = HashSet::new();
    let mut instance_identities = HashSet::new();
    for instance in &resource.deposed {
        if !keys.insert(instance.key.as_str()) {
            return Err(format!(
                "duplicate deposed generation key {:?} in resource \
                 (provider={:?}, resource_type={:?}, identity={:?})",
                instance.key, resource.provider, resource.resource_type, resource.identity
            ));
        }
        if !instance_identities.insert((
            instance.identifier.as_str(),
            instance.provider_instance.as_deref(),
        )) {
            return Err(format!(
                "duplicate deposed generation identity \
                 (identifier={:?}, provider_instance={:?}) in resource \
                 (provider={:?}, resource_type={:?}, identity={:?})",
                instance.identifier,
                instance.provider_instance,
                resource.provider,
                resource.resource_type,
                resource.identity
            ));
        }
    }
    Ok(())
}

fn merge_deposed_generations(
    resource: &mut ResourceState,
    existing_deposed: Vec<DeposedInstance>,
) -> Result<(), String> {
    for existing in existing_deposed {
        let same_instance_is_incoming = resource.deposed.iter().any(|incoming| {
            incoming.identifier == existing.identifier
                && incoming.provider_instance == existing.provider_instance
        });
        if same_instance_is_incoming {
            continue;
        }
        if let Some(incoming) = resource
            .deposed
            .iter()
            .find(|incoming| incoming.key == existing.key)
        {
            return Err(format!(
                "deposed generation key {:?} identifies both \
                 (identifier={:?}, provider_instance={:?}) and \
                 (identifier={:?}, provider_instance={:?}) in resource \
                 (provider={:?}, resource_type={:?}, identity={:?})",
                existing.key,
                incoming.identifier,
                incoming.provider_instance,
                existing.identifier,
                existing.provider_instance,
                resource.provider,
                resource.resource_type,
                resource.identity
            ));
        }
        resource.deposed.push(existing);
    }
    Ok(())
}

fn remove_current_identifier_from_deposed(resource: &mut ResourceState) {
    if let Some(identifier) = resource.identifier.as_deref() {
        let provider_instance = resource.directives.provider_instance.clone();
        resource.deposed.retain(|deposed| {
            deposed.identifier != identifier || deposed.provider_instance != provider_instance
        });
    }
}

fn has_legacy_name_override(resource: &ResourceState) -> bool {
    resource
        .name_overrides
        .values()
        .any(|override_| override_.original_value.is_none())
}

impl NameOverrideSource for StateFile {
    fn name_overrides_for(
        &self,
        resource_id: &ResourceId,
    ) -> Option<&HashMap<String, NameOverride>> {
        self.resources
            .iter()
            .find(|resource| Self::id_for_resource_state(resource) == *resource_id)
            .and_then(|resource| {
                if resource.name_overrides.is_empty() {
                    None
                } else {
                    Some(&resource.name_overrides)
                }
            })
    }
}

/// Minimal struct for extracting just the version field from a state file.
#[derive(Deserialize)]
struct VersionCheck {
    version: u32,
}

/// Reports an in-memory schema upgrade applied to a state file by
/// [`check_and_migrate`]. The function itself never writes to stderr —
/// the caller (a backend impl) decides whether and how often to log,
/// which lets `carina plan` emit the migration warning exactly once
/// per run even when state is read multiple times (T0 snapshot +
/// post-plan drift re-read, see carina#3283).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationInfo {
    /// On-disk version that was read.
    pub from: u32,
    /// Version the state was migrated to (always `StateFile::CURRENT_VERSION`).
    pub to: u32,
}

/// Successful outcome of [`check_and_migrate`].
///
/// Always carries the parsed (and possibly schema-upgraded) state.
/// `migration` is `Some` iff the on-disk version was older than
/// [`StateFile::CURRENT_VERSION`] and an in-memory upgrade was applied;
/// `None` for state files that were already current.
///
/// Intentionally not `Clone`: a [`StateFile`] can hold every resource
/// in the deployment, so accidentally cloning the wrapper would clone
/// the full state. The wrapper is consumed at the backend boundary
/// (`read_state` → caller takes `.into_parts()` or `.into_state()`). Its
/// fields are private so only the validating load seam can construct one.
#[derive(Debug)]
pub struct MigratedStateFile {
    state: StateFile,
    migration: Option<MigrationInfo>,
}

impl MigratedStateFile {
    /// Borrow the validated, fully migrated state.
    pub fn state(&self) -> &StateFile {
        &self.state
    }

    /// Report the in-memory schema migration, if one was applied.
    pub fn migration(&self) -> Option<MigrationInfo> {
        self.migration
    }

    /// Consume the validated outcome into its state and migration metadata.
    pub fn into_parts(self) -> (StateFile, Option<MigrationInfo>) {
        (self.state, self.migration)
    }

    /// Discard the migration info and return just the state. Convenience
    /// for call sites (e.g. unit tests, fixture loaders) that only need
    /// the parsed `StateFile` and never log the migration.
    pub fn into_state(self) -> StateFile {
        self.state
    }
}

/// A state file just read from a backend, tagged with whether
/// `check_and_migrate` had to lift the on-disk schema in memory.
/// Returned from [`crate::backend::StateBackend::read_state`].
///
/// The two-variant shape (rather than `StateFile + Option<MigrationInfo>`)
/// forces every consumer to `match` on the pristine vs. migrated case
/// at the boundary. Lock-held call sites (`apply`, `destroy`,
/// `state refresh`) bind the `Migrated { state, info }` arm and
/// persist the upgraded shape before any short-circuit return, so the
/// carina#3283 warning ("Disk state will be rewritten during the next
/// apply or state refresh of that project") matches reality —
/// carina#3315. Read-only call sites (`plan`, exports, `state
/// list/show/lookup`, etc.) bind both arms identically and discard
/// `info` explicitly — the discard is visible in the source rather
/// than hidden inside an `Option::None`.
///
/// Intentionally not `Clone`: a [`StateFile`] can hold every resource
/// in the deployment, so accidentally cloning the wrapper would clone
/// the full state. Consume the wrapper at the call site via
/// [`Self::into_state`] (read-only paths) or by destructuring the
/// enum directly (lock-held paths that need the `MigrationInfo`).
#[derive(Debug)]
#[must_use = "a loaded state must be either persisted (lock-held paths) or \
              explicitly consumed via .into_state() (read-only paths); \
              see LoadedState docs for the carina#3315 invariant"]
pub enum LoadedState {
    /// The on-disk state already matched [`StateFile::CURRENT_VERSION`];
    /// no in-memory migration was applied.
    Pristine(StateFile),
    /// `check_and_migrate` lifted the on-disk state to
    /// [`StateFile::CURRENT_VERSION`] in memory. Lock-held callers
    /// must persist `state` so the disk reflects the new schema.
    Migrated {
        state: StateFile,
        info: MigrationInfo,
    },
}

impl LoadedState {
    /// Consume the wrapper, returning the state and dropping any
    /// pending-migration indicator. Use this for read-only paths
    /// (`plan`, exports, `state list/show/lookup`) where the on-disk
    /// version is reported via a separate warning and not persisted.
    /// Lock-held paths destructure the enum directly so the
    /// `MigrationInfo` is a named binding rather than a hidden drop.
    pub fn into_state(self) -> StateFile {
        match self {
            Self::Pristine(s) => s,
            Self::Migrated { state, .. } => state,
        }
    }
}

/// Emit the state-schema migration warning to stderr at most once for
/// the given `OnceLock`-protected slot. Backends call this from
/// `read_state` so a single `carina plan` run — which reads state
/// twice (T0 snapshot plus post-plan drift re-read, carina#3283) —
/// surfaces the warning exactly once per backend instance.
///
/// `display_target` should identify the underlying state file (e.g. a
/// local path or `s3://bucket/key`) so an operator running with
/// `upstream_state` chains or multiple backends in one process can
/// tell which file the warning refers to.
///
/// Dedupe is per-backend-instance, not per `(from, to)` pair: once
/// any migration has been logged through this slot, no subsequent
/// `read_state` on the same backend logs again, even if a later read
/// observed a different `from` version. This matches the only
/// realistic case (each backend points at one physical state file)
/// and keeps the API trivially correct.
pub fn log_state_migration_once(
    slot: &std::sync::OnceLock<MigrationInfo>,
    info: MigrationInfo,
    display_target: &str,
) {
    if slot.set(info).is_ok() {
        eprintln!(
            "Warning: state file {} is v{} on disk; in-memory migration \
             to v{} applied for this run. Disk state will be rewritten \
             during the next apply or state refresh of that project.",
            display_target, info.from, info.to
        );
    }
}

/// Deserialize a state file from a JSON string, checking the version and
/// migrating from older formats if necessary.
///
/// - Current version: deserialized directly; returned with `migration = None`.
/// - Future version (newer than supported): returns a clear error asking the
///   user to upgrade Carina.
/// - Older version: attempts deserialization with serde defaults and bumps
///   the version to current. The from/to versions are returned as
///   [`MigrationInfo`] so the caller can log the event (carina#3283).
/// - Invalid JSON: returns a parse error.
pub fn check_and_migrate(content: &str) -> Result<MigratedStateFile, BackendError> {
    let check: VersionCheck = serde_json::from_str(content)
        .map_err(|e| BackendError::InvalidState(format!("Failed to parse state version: {}", e)))?;

    let mut migration: Option<MigrationInfo> = None;
    let mut state: StateFile = match check.version {
        v if v == StateFile::CURRENT_VERSION => StateFile::from_json_str(content)
            .map_err(|error| map_state_file_decode_error(error, None))?,
        v if v > StateFile::CURRENT_VERSION => {
            return Err(BackendError::StateVersionTooNew {
                found: v,
                supported: StateFile::CURRENT_VERSION,
            });
        }
        v => {
            migration = Some(MigrationInfo {
                from: v,
                to: StateFile::CURRENT_VERSION,
            });
            // v12 → v13: migrate every value tree before typed state
            // deserialization validates the reserved `$` namespace.
            let migrated_persisted_json = if v <= 12 {
                Some(migrate_v12_persisted_values(content)?)
            } else {
                None
            };
            let parse_content = migrated_persisted_json.as_deref().unwrap_or(content);
            let mut state = StateFile::from_json_str(parse_content)
                .map_err(|error| map_state_file_decode_error(error, Some(v)))?;
            // v5 → v6: lift the flat `desired_keys: Vec<String>` field
            // (already discarded by serde because the v6 struct no longer
            // declares it) back from the source JSON, and use it to
            // construct a top-level `ExplicitFields::Struct` whose
            // children are all `Leaf`. Mirrors the design's "first plan
            // after upgrade still surfaces nested-field spurious diffs;
            // first apply rebuilds the full tree" behavior.
            if v <= 5 {
                migrate_v5_desired_keys_to_explicit(content, &mut state)?;
            }
            // v6 → v7 (carina#3280): a top-level
            // `ExplicitFields::Struct { children: {} }` row is the
            // legacy-corruption shape produced by an older for-loop
            // expansion path; it is structurally ambiguous with "user
            // authored an empty struct at the top level" (which the
            // current code never legitimately emits — the resource
            // authoring builders produce this shape only when
            // `resource.attributes` is empty, and the writeback path emits `Unrecorded`
            // instead). Rewriting every top-level empty Struct to
            // `Unrecorded` on read makes the variant the single
            // source of truth and lets every `match` arm be exhaustive
            // again.
            if v <= 6 {
                migrate_v6_empty_struct_to_unrecorded(&mut state);
            }
            // v10 → v11: host metadata used to be stored inside provider
            // attributes. Once consumers stopped filtering `_`-prefixed keys,
            // those legacy values became provider-visible removals. Strip them
            // once at the state load boundary, including historical/deposed
            // display data and the matching top-level authoring record.
            if v <= 10 {
                migrate_v10_reserved_attributes(&mut state);
            }
            // v11 → v12 needs no additional tree walk. ResourceState's custom
            // deserializer has already converted every current and deposed raw
            // legacy marker using the row identity stored in the v11 file.
            // v12 → v13 was applied to raw JSON before deserialization above.
            state.version = StateFile::CURRENT_VERSION;
            state
        }
    };
    // Map-key addresses written under the legacy `["..."]` shape are
    // rewritten to the canonical form on read so existing state files
    // resolve cleanly against new emissions. See #1903.
    state.canonicalize_addresses()?;
    // carina#3266: `state.resources` is managed-only by invariant
    // (since #3181). Pre-#3181 versions of `carina state refresh` /
    // older apply paths persisted `read aws.*` data-source rows here;
    // those rows carry `identifier: null` (a data source has no
    // provider-side identity), survive every subsequent read, and
    // then silently overwrite the fresh phase-2 data-source read
    // when post-apply binding views overlay `state.resources` on top
    // of `current_states`. Consumer paths (apply, destroy, state
    // refresh, plan) each used the overlay and so each saw the stale
    // value — including export resolution, which then wrote the
    // pre-apply literal back to `state.exports` on every apply.
    //
    // Dropping these rows at the single read seam restores the
    // managed-only invariant for every downstream consumer in one
    // place; no per-site filter or post-write cleanup is required.
    //
    // v9 adds a legitimate identifier=None shape: the current instance
    // has been removed, but one or more deposed generations still need
    // delete tracking. Keep those rows until their deposed list is empty.
    state
        .resources
        .retain(|rs| rs.identifier.is_some() || !rs.deposed.is_empty());
    // Identity spellings can converge during migration (notably legacy map-key
    // address canonicalization), so the serde-time check is not sufficient.
    // Validate the fully migrated shape immediately before it crosses the load
    // boundary.
    state
        .validate_unique_identities()
        .map_err(BackendError::InvalidState)?;
    Ok(MigratedStateFile { state, migration })
}

/// Deserialize a state file from a byte slice, checking the version and
/// migrating from older formats if necessary.
///
/// This is the byte-slice equivalent of [`check_and_migrate`] for backends
/// that read raw bytes (e.g., S3).
pub fn check_and_migrate_bytes(bytes: &[u8]) -> Result<MigratedStateFile, BackendError> {
    let content = std::str::from_utf8(bytes)
        .map_err(|e| BackendError::InvalidState(format!("State file is not valid UTF-8: {}", e)))?;
    check_and_migrate(content)
}

/// Pre-replacement instance that still needs a successful Delete.
///
/// Persisted attributes cannot be read as ordinary JSON values. Callers must
/// use [`DeposedInstance::persisted_attributes`] so secret markers retain
/// their typed meaning.
///
/// ```compile_fail
/// use carina_state::state::DeposedInstance;
///
/// let instance: Option<DeposedInstance> = None;
/// let _raw = &instance.unwrap().attributes;
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct DeposedInstance {
    /// System-assigned unique key for this generation.
    pub key: DeposedKey,
    /// Provider-side identifier of the still-live old instance.
    pub identifier: String,
    /// Provider instance routing used when this generation was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_instance: Option<String>,
    /// Last known attributes of the old instance.
    attributes: PersistedAttributes,
    /// Binding dependencies inherited from the old row.
    pub dependency_bindings: BTreeSet<String>,
}

impl DeposedInstance {
    /// Construct a deposed generation from the opaque output of the state
    /// writeback encoder.
    pub fn from_persisted_attributes(
        key: DeposedKey,
        identifier: String,
        provider_instance: Option<String>,
        attributes: PersistedAttributes,
        dependency_bindings: BTreeSet<String>,
    ) -> Self {
        Self {
            key,
            identifier,
            provider_instance,
            attributes,
            dependency_bindings,
        }
    }

    /// Construct a deposed generation from raw state JSON in tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn from_persisted_json(
        key: DeposedKey,
        identifier: String,
        provider_instance: Option<String>,
        attributes: HashMap<String, serde_json::Value>,
        dependency_bindings: BTreeSet<String>,
    ) -> Self {
        Self::from_persisted_attributes(
            key,
            identifier,
            provider_instance,
            persisted_attributes_for_test_state_row(
                &ResourceId::with_provider_identity("test", "test.resource", "fixture-row", None),
                attributes,
            ),
            dependency_bindings,
        )
    }

    /// Construct a deposed generation from raw state JSON that may contain
    /// legacy secret markers. The containing resource row is mandatory so the
    /// raw marker can be upgraded with its historical salt bytes immediately.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn from_persisted_json_for_resource(
        row_id: &ResourceId,
        key: DeposedKey,
        identifier: String,
        provider_instance: Option<String>,
        attributes: HashMap<String, serde_json::Value>,
        dependency_bindings: BTreeSet<String>,
    ) -> Self {
        Self::from_persisted_attributes(
            key,
            identifier,
            provider_instance,
            persisted_attributes_for_test_state_row(row_id, attributes),
            dependency_bindings,
        )
    }

    /// Borrow this generation's attributes through the persisted decode seam.
    pub fn persisted_attributes(&self) -> &PersistedAttributes {
        &self.attributes
    }
}

/// State of a single managed resource.
///
/// Persisted attributes cannot be read as ordinary JSON values. Callers must
/// use [`ResourceState::persisted_attributes`] so secret markers retain their
/// typed meaning.
///
/// ```compile_fail
/// use carina_state::state::ResourceState;
///
/// let state = ResourceState::new("mock.test.resource", "example", "mock");
/// let _raw = &state.attributes;
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct ResourceState {
    /// Resource type (e.g., "s3.Bucket", "ec2.Vpc")
    pub resource_type: String,
    /// Resource identity (from the DSL resource address identity)
    #[serde(alias = "name")]
    pub identity: ResourceIdentity,
    /// Provider name (e.g., "aws")
    pub provider: String,
    /// AWS internal identifier (e.g., vpc-xxx, subnet-xxx)
    /// If None, the resource is considered not to exist
    #[serde(default)]
    pub identifier: Option<String>,
    /// Attributes in their state-file JSON representation.
    attributes: PersistedAttributes,
    /// Old replacement generations that still need a successful Delete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deposed: Vec<DeposedInstance>,
    /// Whether this resource is protected from deletion (e.g., state bucket)
    #[serde(default)]
    pub protected: bool,
    /// Carina-side directives persisted from the DSL `directives` block.
    #[serde(default)]
    pub directives: Directives,
    /// Attribute prefixes used to generate names (e.g., {"bucket_name": "my-app-"})
    #[serde(default)]
    pub prefixes: HashMap<String, String>,
    /// Permanent name overrides from create_before_destroy with non-renameable attributes.
    /// Maps attribute name to the temporary cloud-side name and the DSL value recorded with it.
    #[serde(default)]
    pub name_overrides: HashMap<String, NameOverride>,
    /// Tree of fields the user explicitly wrote in their `.crn` for
    /// this resource. Used by the differ both to detect attribute
    /// removals and to project actual-state through the authoring
    /// shape so server-side default fields the user never wrote stop
    /// appearing as spurious removals (refs awscc#206).
    ///
    /// Replaces the flat `desired_keys: Vec<String>` (state ≤ v5);
    /// the v5 reader lifts each top-level key to a `Leaf` child of
    /// the root `Struct`. The next plan/apply rebuilds a full tree
    /// from the resource's authored `Value` via the aligned
    /// `carina_core::explicit::build_from_resource_for_stored_values`.
    #[serde(default, skip_serializing_if = "is_empty_explicit")]
    pub explicit: ExplicitFields,
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
    /// Marker for a state produced by a partial-success create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_read: Option<PartialReadMarker>,
}

#[derive(Deserialize)]
struct EncodedDeposedInstance {
    key: DeposedKey,
    identifier: String,
    #[serde(default)]
    provider_instance: Option<String>,
    attributes: HashMap<String, serde_json::Value>,
    dependency_bindings: BTreeSet<String>,
}

#[derive(Deserialize)]
struct EncodedResourceState {
    #[serde(flatten)]
    persisted: EncodedStateRowAttributes,
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    deposed: Vec<EncodedDeposedInstance>,
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    directives: Directives,
    #[serde(default)]
    prefixes: HashMap<String, String>,
    #[serde(default)]
    name_overrides: HashMap<String, NameOverride>,
    #[serde(default)]
    explicit: ExplicitFields,
    #[serde(default)]
    binding: Option<String>,
    #[serde(default)]
    dependency_bindings: BTreeSet<String>,
    #[serde(default)]
    write_only_attributes: Vec<String>,
    #[serde(default)]
    partial_read: Option<PartialReadMarker>,
}

impl<'de> Deserialize<'de> for ResourceState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = EncodedResourceState::deserialize(deserializer)?;
        let persisted = encoded
            .persisted
            .decode()
            .map_err(|error| serde::de::Error::custom(ErrorSourceChain(&error)))?;
        let deposed = encoded
            .deposed
            .into_iter()
            .map(|instance| {
                Ok(DeposedInstance {
                    key: instance.key,
                    identifier: instance.identifier,
                    provider_instance: instance.provider_instance,
                    attributes: persisted
                        .decode_related_attributes(instance.attributes)
                        .map_err(|error| serde::de::Error::custom(ErrorSourceChain(&error)))?,
                    dependency_bindings: instance.dependency_bindings,
                })
            })
            .collect::<Result<Vec<_>, D::Error>>()?;
        let (provider, resource_type, identity, attributes) = persisted.into_parts();
        Ok(Self {
            resource_type,
            identity,
            provider,
            identifier: encoded.identifier,
            attributes,
            deposed,
            protected: encoded.protected,
            directives: encoded.directives,
            prefixes: encoded.prefixes,
            name_overrides: encoded.name_overrides,
            explicit: encoded.explicit,
            binding: encoded.binding,
            dependency_bindings: encoded.dependency_bindings,
            write_only_attributes: encoded.write_only_attributes,
            partial_read: encoded.partial_read,
        })
    }
}

fn refreshed_explicit_without_desired_values(
    prior: Option<&ExplicitFields>,
    attributes: &HashMap<String, Value>,
) -> ExplicitFields {
    match prior {
        Some(ExplicitFields::Unrecorded) if !attributes.is_empty() => {
            let children = attributes
                .iter()
                .map(|(key, value)| (key.clone(), explicit::build_from_value(value)))
                .collect();
            ExplicitFields::Struct { children }
        }
        Some(prior @ ExplicitFields::Struct { children }) if !children.is_empty() => {
            explicit::demote_list_elements_to_union(prior)
        }
        Some(prior @ ExplicitFields::List { .. })
        | Some(prior @ ExplicitFields::ListElements { .. }) => {
            explicit::demote_list_elements_to_union(prior)
        }
        _ => ExplicitFields::Unrecorded,
    }
}

impl ResourceState {
    /// Borrow this row's attributes through the persisted decode seam.
    pub fn persisted_attributes(&self) -> &PersistedAttributes {
        &self.attributes
    }

    /// Set one raw persisted attribute in tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn set_persisted_attribute(&mut self, key: impl Into<String>, value: serde_json::Value) {
        let key = key.into();
        let id = ResourceId::with_provider_identity(
            self.provider.clone(),
            self.resource_type.clone(),
            self.identity.clone(),
            self.directives.provider_instance.clone(),
        );
        let upgraded =
            persisted_attributes_for_test_state_row(&id, HashMap::from([(key.clone(), value)]));
        self.attributes.extend(upgraded);
    }

    /// Render the sorted attribute map for `carina state show` without
    /// exposing the encoded values to value-layer callers.
    pub fn format_attributes_for_state_show(&self) -> String {
        let sorted: std::collections::BTreeMap<String, serde_json::Value> =
            self.attributes.to_decoded_json().into_iter().collect();
        serde_json::to_string_pretty(&sorted).expect("state JSON values are serializable")
    }

    /// Render one stored attribute for `carina state show`.
    pub fn format_attribute_for_state_show(&self, key: &str, json_output: bool) -> Option<String> {
        self.attributes.decoded_json_value(key).map(|value| {
            if json_output {
                serde_json::to_string_pretty(&value).expect("state JSON values are serializable")
            } else {
                match &value {
                    serde_json::Value::String(value) => value.clone(),
                    serde_json::Value::Bool(value) => value.to_string(),
                    serde_json::Value::Number(value) => value.to_string(),
                    serde_json::Value::Null => "null".to_string(),
                    other => serde_json::to_string_pretty(other)
                        .expect("state JSON values are serializable"),
                }
            }
        })
    }

    /// Render current and deposed generations for `carina state show`.
    pub fn format_generations_for_state_show(&self) -> String {
        fn sorted_attributes(attributes: &PersistedAttributes) -> serde_json::Value {
            let sorted: std::collections::BTreeMap<String, serde_json::Value> =
                attributes.to_decoded_json().into_iter().collect();
            serde_json::to_value(sorted).expect("state JSON values are serializable")
        }

        let current = if self.identifier.is_none() && self.attributes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({
                "identifier": self.identifier.as_ref(),
                "attributes": sorted_attributes(&self.attributes),
            })
        };
        let deposed: Vec<_> = self
            .deposed
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "key": entry.key.as_str(),
                    "identifier": entry.identifier,
                    "marker": format!("(deposed {} {})", entry.key.as_str(), entry.identifier),
                    "provider_instance": entry.provider_instance,
                    "attributes": sorted_attributes(&entry.attributes),
                })
            })
            .collect();
        serde_json::to_string_pretty(&serde_json::json!({
            "current": current,
            "deposed": deposed,
        }))
        .expect("state JSON values are serializable")
    }

    /// Create a new resource state
    pub fn new(
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
        provider: impl Into<String>,
    ) -> Self {
        let resource_type = resource_type.into();
        let identity = identity.into();
        let provider = provider.into();
        Self {
            resource_type,
            identity,
            provider,
            identifier: None,
            attributes: PersistedAttributes::default(),
            deposed: Vec::new(),
            protected: false,
            directives: Directives::default(),
            prefixes: HashMap::new(),
            name_overrides: HashMap::new(),
            explicit: ExplicitFields::default(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            write_only_attributes: Vec::new(),
            partial_read: None,
        }
    }

    /// Set the identifier (AWS internal ID like vpc-xxx)
    pub fn with_identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    /// Set a raw persisted attribute in tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn with_attribute(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.set_persisted_attribute(key, value);
        self
    }

    /// Populate attributes from a provider-returned State
    pub fn with_attributes_from_state(
        mut self,
        state: &State,
    ) -> Result<Self, ResourceStateEncodingError> {
        let encoded = state
            .attributes
            .iter()
            .filter_map(|(key, value)| {
                PersistedValue::encode_provider_value(value, self.attributes.encoded_value(key))
                    .ok()
                    .map(|value| (key.clone(), value))
            })
            .collect();
        let attributes = PersistedAttributes::from_persisted_values(encoded)?;
        self.attributes.extend(attributes);
        if let Some(identifier) = &state.identifier {
            self.identifier = Some(identifier.clone());
        }
        self.partial_read = state.partial_read.clone();
        Ok(self)
    }

    /// Mark this resource as protected
    pub fn with_protected(mut self, protected: bool) -> Self {
        self.protected = protected;
        self
    }

    /// Build the seed `ResourceState` for a backend-owned state bucket.
    ///
    /// This is the canonical shape the state backend records when it
    /// auto-creates its own storage (e.g. the S3 state bucket): protected,
    /// with `identifier` populated so the differ recognises the resource as
    /// existing — without `identifier`, `StateFile::build_state_for_resource`
    /// returns "not found" and the next apply re-issues `CreateBucket`,
    /// reproducing #2533 (`BucketAlreadyOwnedByYou`).
    ///
    /// `resource_identity` is the state-side resource identity and must equal the
    /// resolved identity of the desired resource (anonymous resources get a
    /// hash-derived id like `aws_s3_bucket_<hash>`); `bucket_name` is the
    /// AWS bucket name used as the provider identifier and as the value
    /// of the `bucket` attribute. Mixing the two breaks the differ — see
    /// #2533 for the failure mode.
    pub fn managed_state_bucket(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        resource_identity: impl Into<ResourceIdentity>,
        bucket_name: impl Into<String>,
    ) -> Self {
        let bucket_name = bucket_name.into();
        let mut state = Self::new(resource_type, resource_identity, provider)
            .with_identifier(&bucket_name)
            .with_protected(true);
        let attributes = PersistedAttributes::from_persisted_values(HashMap::from([(
            "bucket".to_string(),
            PersistedValue::encode_provider_value(
                &Value::Concrete(ConcreteValue::String(bucket_name.into())),
                None,
            )
            .expect("a backend bucket name is persistable"),
        )]))
        .expect("encoded backend bucket attributes are valid");
        state.attributes.extend(attributes);
        state
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
    pub fn merge_write_only_attributes(
        &mut self,
        resource: &Resource,
        write_only_keys: &[String],
    ) -> Result<(), ResourceStateEncodingError> {
        let mut merged = Vec::new();
        let mut encoded = HashMap::new();
        for key in write_only_keys {
            // Only merge if the user specified this attribute and it's not already
            // in the provider-returned state
            if let Some(value) = resource.attributes.get(key)
                && !self.attributes.contains_key(key)
                && let Ok(json_value) = PersistedValue::encode_desired_value(value, None)
            {
                encoded.insert(key.clone(), json_value);
                merged.push(key.clone());
            }
        }
        let attributes = PersistedAttributes::from_persisted_values(encoded)?;
        self.attributes.extend(attributes);
        merged.sort();
        self.write_only_attributes = merged;
        Ok(())
    }

    /// Serialize provider attributes with desired-resource and schema-derived
    /// secret overrides.
    ///
    /// When the new desired resource no longer contains an old protected key,
    /// the schema still prevents persisting provider-returned plaintext for
    /// write-only attributes. Such keys are dropped unless the desired value
    /// carries a `Secret(_)` wrapper that can be merged into a hash.
    pub fn attributes_to_state_json_lossy_for_resource_and_schema(
        resource: &ResolvedResource,
        schema: Option<&ResourceSchema>,
        attributes: &HashMap<String, Value>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
    ) -> PersistedAttributes {
        PersistedAttributes::encode_values_for_resource_and_schema(
            resource,
            schema,
            attributes,
            previous_hash_authority,
        )
    }

    /// Apply desired-resource and schema-derived secret protection to an
    /// opaque persisted attribute map.
    pub fn protect_persisted_attributes_for_resource_and_schema(
        resource: &ResolvedResource,
        schema: Option<&ResourceSchema>,
        serialized: PersistedAttributes,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
    ) -> PersistedAttributes {
        serialized.protect_for_resource_and_schema(resource, schema, previous_hash_authority)
    }

    /// Replace the provider-observed portion of an orphan row while retaining
    /// every host-owned field from the existing state entry.
    ///
    /// Unlike desired-resource writeback, this path has no `Resource` from the
    /// current configuration. Directives, prefixes, binding metadata,
    /// write-only metadata, protection flags, name overrides, and deposed
    /// generations therefore remain untouched. The explicit-authoring tree is
    /// refreshed through the same conservative no-desired-values path as a
    /// desired row: aligned list elements are demoted to a union, and legacy
    /// `Unrecorded` rows self-heal from the provider attributes.
    pub fn refreshed_from_provider_state(
        &self,
        state: &State,
        schema: Option<&ResourceSchema>,
    ) -> Result<Self, ResourceStateEncodingError> {
        let mut refreshed = self.clone();
        refreshed.identifier = state.identifier.clone();
        refreshed.partial_read = state.partial_read.clone();

        let mut serialized = HashMap::new();
        for (key, value) in &state.attributes {
            serialized.insert(
                key.clone(),
                PersistedValue::encode_provider_value(value, self.attributes.encoded_value(key))?,
            );
        }
        let attributes = PersistedAttributes::from_persisted_values(serialized)?;
        refreshed.attributes = attributes.protect_for_orphan_refresh(schema, &self.attributes);
        refreshed.explicit =
            refreshed_explicit_without_desired_values(Some(&self.explicit), &state.attributes);
        Ok(refreshed)
    }

    /// Build a `ResourceState` from a desired resource, provider-returned
    /// state, optional previous row state, and optional schema.
    ///
    /// Provider attributes are serialized into state JSON, desired secret
    /// values are masked before storage, previously-stored secret hashes can
    /// protect keys removed from desired state, and schema write-only
    /// plaintext is removed unless a desired secret already supplied a masked
    /// value. When `existing` is provided, protected/name override metadata and
    /// existing deposed generations are preserved by the row-write path.
    ///
    /// Returns an error if any provider attribute value cannot be converted to
    /// JSON, such as non-finite float values.
    pub fn from_provider_state_for_resource_and_schema(
        resource: &ResolvedResource,
        state: &State,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        preferred_hashes: Option<&PersistedAttributes>,
    ) -> Result<Self, ResourceStateEncodingError> {
        let mut rs = Self::new(
            &resource.id.resource_type,
            resource.identity().clone(),
            resource.id.provider.clone(),
        );
        rs.identifier = state.identifier.clone();
        rs.partial_read = state.partial_read.clone();
        let mut serialized = HashMap::new();
        for (k, v) in &state.attributes {
            serialized.insert(
                k.clone(),
                PersistedValue::encode_provider_value(
                    v,
                    preferred_hashes
                        .and_then(|attributes| attributes.encoded_value(k))
                        .or_else(|| existing.and_then(|row| row.attributes.encoded_value(k))),
                )?,
            );
        }
        let previous_hash_authority = existing
            .map(ResourceState::persisted_attributes)
            .map(PreviousSecretHashAuthority::OnlyKeysAbsentFromDesired)
            .unwrap_or(PreviousSecretHashAuthority::None);
        if let Some(existing) = existing {
            rs.protected = existing.protected;
            rs.name_overrides = existing.name_overrides.clone();
        }
        rs.attributes = PersistedAttributes::from_persisted_values(serialized)?
            .try_protect_for_resource_and_schema(
                resource,
                schema,
                previous_hash_authority,
                preferred_hashes,
            )?;
        rs.directives = resource.directives.clone();
        if rs.directives.provider_instance.is_none() {
            rs.directives.provider_instance = resource.id.provider_instance.clone();
        }
        rs.prefixes = resource.prefixes.clone();
        // Record the structural shape the user wrote in their .crn,
        // so the differ can project actual-state through it and skip
        // server-side defaults the user never authored (refs awscc#206).
        // carina#3280: when the aligned resource builder would produce an
        // empty top-level `Struct` (the user authored no attributes —
        // bodyless resource like `aws.sts.CallerIdentity {}`, or the
        // `carina state import` path that constructs a `Resource`
        // with no DSL attributes), emit `Unrecorded` instead. The
        // empty-Struct-at-top-level shape used to double as the
        // legacy-corruption marker, which forced callers to
        // disambiguate by runtime convention; `Unrecorded` is the
        // typed signal for "no authoring record".
        let built =
            explicit::build_from_resource_for_stored_values(resource, &state.attributes, schema);
        rs.explicit = match built {
            ExplicitFields::Struct { ref children } if children.is_empty() => {
                // Three sub-cases when `resource.attributes` is empty:
                //
                // 1. Self-heal: prior on-disk row is `Unrecorded` (the
                //    legacy-corruption marker, possibly from the v6→v7
                //    migration). Promote freshly-read state attributes
                //    back into an authoring record so the next plan
                //    sees a populated `Struct` and the runtime
                //    pass-through path no longer fires for this row.
                //
                // 2. Idempotent re-apply: prior on-disk row already
                //    carries a populated `Struct` (e.g. the row was
                //    self-healed in a previous apply, or the resource
                //    legitimately has no DSL body but was applied
                //    once before and recorded an authoring tree from
                //    the provider's read). Preserve its authoring
                //    restriction, demoting aligned lists to unions;
                //    collapsing the whole tree to `Unrecorded` would
                //    flip-flop the row on every apply.
                //
                // 3. Green-field write: prior row is `None` or
                //    `Leaf`. There is genuinely no authoring record
                //    yet. Emit `Unrecorded`.
                refreshed_explicit_without_desired_values(
                    existing.map(|entry| &entry.explicit),
                    &state.attributes,
                )
            }
            other => other,
        };
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

/// Returns true if the `ExplicitFields` is the default (`Leaf`) — used
/// as a `skip_serializing_if` predicate so resources that have not yet
/// been touched by `from_provider_state_for_resource_and_schema` (or that legitimately have no
/// authored attributes) don't emit a verbose `"explicit": {"kind": "leaf"}`
/// line.
fn is_empty_explicit(e: &ExplicitFields) -> bool {
    matches!(e, ExplicitFields::Leaf)
}

/// v12 → v13: migrate every JSON tree that is later decoded as a Carina
/// `Value`. Host metadata is deliberately left untouched.
fn migrate_v12_persisted_values(content: &str) -> Result<String, BackendError> {
    let mut raw: serde_json::Value = serde_json::from_str(content).map_err(|error| {
        BackendError::InvalidState(format!(
            "Failed to re-parse state file for v12 persisted-value migration: {error}"
        ))
    })?;

    if let Some(exports) = raw
        .get_mut("exports")
        .and_then(serde_json::Value::as_object_mut)
    {
        for value in exports.values_mut() {
            carina_core::value::migrate_legacy_persisted_json(value);
        }
    }

    if let Some(resources) = raw
        .get_mut("resources")
        .and_then(serde_json::Value::as_array_mut)
    {
        for resource in resources {
            if let Some(attributes) = resource
                .get_mut("attributes")
                .and_then(serde_json::Value::as_object_mut)
            {
                for value in attributes.values_mut() {
                    carina_core::value::migrate_legacy_persisted_json(value);
                }
            }
            if let Some(deposed) = resource
                .get_mut("deposed")
                .and_then(serde_json::Value::as_array_mut)
            {
                for generation in deposed {
                    if let Some(attributes) = generation
                        .get_mut("attributes")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        for value in attributes.values_mut() {
                            carina_core::value::migrate_legacy_persisted_json(value);
                        }
                    }
                }
            }
        }
    }

    serde_json::to_string(&raw).map_err(|error| {
        BackendError::InvalidState(format!(
            "Failed to serialize state file after v12 persisted-value migration: {error}"
        ))
    })
}

/// v5 → v6: state files written under v5 carry a `"desired_keys"`
/// array per resource (flat list of top-level user-authored keys).
/// v6's `ResourceState` no longer has that field, so serde silently
/// discards it during the initial deserialization. This helper
/// re-reads the source JSON to recover those arrays and lifts each
/// entry into a `Leaf` child of the v6 `explicit: ExplicitFields::Struct`
/// tree.
///
/// Resources are matched by `(provider, resource_type, identity)` because
/// state files use that triple as the canonical identity.
fn migrate_v5_desired_keys_to_explicit(
    content: &str,
    state: &mut StateFile,
) -> Result<(), BackendError> {
    let raw: serde_json::Value = serde_json::from_str(content).map_err(|e| {
        BackendError::InvalidState(format!(
            "Failed to re-parse state file for v5 desired_keys recovery: {}",
            e
        ))
    })?;
    let Some(raw_resources) = raw.get("resources").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    for raw_rs in raw_resources {
        let provider = raw_rs.get("provider").and_then(|v| v.as_str());
        let resource_type = raw_rs.get("resource_type").and_then(|v| v.as_str());
        let identity = raw_rs
            .get("identity")
            .or_else(|| raw_rs.get("name"))
            .and_then(|v| v.as_str());
        let Some(((provider, resource_type), identity)) = provider.zip(resource_type).zip(identity)
        else {
            continue;
        };
        let Some(keys) = raw_rs.get("desired_keys").and_then(|v| v.as_array()) else {
            continue;
        };
        let children: std::collections::HashMap<String, ExplicitFields> = keys
            .iter()
            .filter_map(|v| v.as_str().map(|s| (s.to_string(), ExplicitFields::Leaf)))
            .collect();
        if children.is_empty() {
            continue;
        }
        if let Some(rs) = state.resources.iter_mut().find(|rs| {
            rs.provider == provider
                && rs.resource_type == resource_type
                && rs.identity.as_str() == identity
        }) {
            rs.explicit = ExplicitFields::Struct { children };
        }
    }
    Ok(())
}

/// v6 → v7 (carina#3280): rewrite every top-level
/// `ExplicitFields::Struct { children: {} }` row to
/// `ExplicitFields::Unrecorded`. The empty-Struct shape on disk is
/// always the legacy-corruption pattern (the older for-loop expansion
/// path that lost child attributes before reaching writeback), never a
/// legitimate "user authored an empty struct at top level" — the
/// the resource authoring builders produce this shape only when
/// `resource.attributes` is empty, and the writeback path emits `Unrecorded`
/// for that case instead. Migrating eliminates the
/// runtime ambiguity that callers used to disambiguate by convention.
fn migrate_v6_empty_struct_to_unrecorded(state: &mut StateFile) {
    for rs in state.resources.iter_mut() {
        if let ExplicitFields::Struct { children } = &rs.explicit
            && children.is_empty()
        {
            rs.explicit = ExplicitFields::Unrecorded;
        }
    }
}

/// v10 → v11: remove host-reserved top-level attributes written by older
/// Carina versions and the built-in mock provider.
fn migrate_v10_reserved_attributes(state: &mut StateFile) {
    for resource in &mut state.resources {
        resource.attributes.retain_keys(|key| !key.starts_with('_'));
        for deposed in &mut resource.deposed {
            deposed.attributes.retain_keys(|key| !key.starts_with('_'));
        }
        if let ExplicitFields::Struct { children } = &mut resource.explicit {
            children.retain(|key, _| !key.starts_with('_'));
        }
    }
}

#[cfg(test)]
mod tests;
