//! State file structures for persisting infrastructure state

use carina_core::deps::get_resource_dependencies;
use carina_core::explicit::{self, ExplicitFields};
pub use carina_core::name_override::{ApplyDecision, NameOverride, should_apply_override};
use carina_core::override_aware::NameOverrideSource;
pub use carina_core::resource::DeposedKey;
use carina_core::resource::{
    ConcreteValue, DeferredValue, Directives, PartialReadMarker, Resource, ResourceId, State, Value,
};
use carina_core::schema::ResourceSchema;
use carina_core::value::{
    SECRET_PREFIX, SecretHashContext, contains_secret, json_to_dsl_value,
    merge_secrets_into_provider_json, value_to_json, value_to_json_with_context,
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
    pub const CURRENT_VERSION: u32 = 9;

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
    /// assert_eq!(state.resources.len(), 1);
    /// assert_eq!(state.resources[0].identity, "aws_s3_bucket_a3f2b1c8");
    /// ```
    pub fn with_managed_state_bucket(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        resource_identity: impl Into<String>,
        bucket_name: impl Into<String>,
    ) -> Self {
        let mut state = Self::new();
        state.upsert_resource(ResourceState::managed_state_bucket(
            provider,
            resource_type,
            resource_identity,
            bucket_name,
        ));
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

    /// Find a resource by provider, type, and identity
    pub fn find_resource(
        &self,
        provider: &str,
        resource_type: &str,
        identity: &str,
    ) -> Option<&ResourceState> {
        self.resources.iter().find(|r| {
            r.provider == provider && r.resource_type == resource_type && r.identity == identity
        })
    }

    /// Find all resources matching a provider and resource type
    pub fn resources_by_type(&self, provider: &str, resource_type: &str) -> Vec<&ResourceState> {
        self.resources
            .iter()
            .filter(|r| r.provider == provider && r.resource_type == resource_type)
            .collect()
    }

    /// Find a resource mutably by provider, type, and identity
    pub fn find_resource_mut(
        &mut self,
        provider: &str,
        resource_type: &str,
        identity: &str,
    ) -> Option<&mut ResourceState> {
        self.resources.iter_mut().find(|r| {
            r.provider == provider && r.resource_type == resource_type && r.identity == identity
        })
    }

    /// Add or update a resource in the state.
    ///
    /// Existing deposed generations are carried through this single row-write
    /// seam so callers cannot accidentally drop still-live old instances.
    /// Incoming deposed entries win on `(identifier, provider_instance)`, and
    /// any entry matching the current identifier is removed so the live current
    /// instance cannot also be scheduled as deposed.
    pub fn upsert_resource(&mut self, mut resource: ResourceState) {
        if let Some(pos) = self.resources.iter().position(|existing| {
            existing.provider == resource.provider
                && existing.resource_type == resource.resource_type
                && existing.identity == resource.identity
        }) {
            let existing_deposed = self.resources[pos].deposed.clone();
            merge_deposed_generations(&mut resource, existing_deposed);
            remove_current_identifier_from_deposed(&mut resource);
            self.resources[pos] = resource;
        } else {
            remove_current_identifier_from_deposed(&mut resource);
            self.resources.push(resource);
        }
    }

    /// Add or refresh one deposed generation through the row-write seam.
    ///
    /// Existing entries are keyed by `(identifier, provider_instance)`. The
    /// final write uses [`Self::upsert_resource`] so the invariant that a
    /// current identifier cannot also be deposed is applied uniformly.
    pub fn upsert_deposed_generation(
        &mut self,
        provider: &str,
        resource_type: &str,
        identity: &str,
        row_provider_instance: Option<String>,
        instance: DeposedInstance,
    ) {
        let mut row = self
            .find_resource(provider, resource_type, identity)
            .cloned()
            .unwrap_or_else(|| {
                let mut row = ResourceState::new(resource_type.to_string(), identity, provider);
                row.directives.provider_instance = row_provider_instance;
                row
            });

        if let Some(existing) = row.deposed.iter_mut().find(|entry| {
            entry.identifier == instance.identifier
                && entry.provider_instance == instance.provider_instance
        }) {
            *existing = instance;
        } else {
            row.deposed.push(instance);
        }
        self.upsert_resource(row);
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
            r.provider == provider && r.resource_type == resource_type && r.identity == identity
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
        if let Some(resource_state) = self.find_resource(
            &resource.id.provider,
            &resource.id.resource_type,
            resource.id.identity_or_empty(),
        ) {
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
        ResourceId::with_provider_name_compat(
            &rs.provider,
            &rs.resource_type,
            &rs.identity,
            rs.directives.provider_instance.clone(),
        )
    }

    /// Build a map of saved attributes, converting JSON values to DSL values.
    pub fn build_saved_attrs(&self) -> HashMap<ResourceId, HashMap<String, Value>> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            let id = Self::id_for_resource_state(rs);
            let attrs: HashMap<String, Value> = rs
                .attributes
                .iter()
                .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                .collect();
            result.insert(id, attrs);
        }
        result
    }

    /// Restore partial-read markers from the state file onto refreshed states.
    pub fn restore_partial_read_markers(&self, states: &mut HashMap<ResourceId, State>) {
        for (id, current) in states.iter_mut() {
            if !current.exists {
                current.partial_read = None;
                continue;
            }
            let marker = self
                .find_resource(&id.provider, &id.resource_type, id.identity_or_empty())
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
        let rs = self.find_resource(&id.provider, &id.resource_type, id.identity_or_empty());
        if let Some(identifier) = rs.and_then(|r| r.identifier.as_deref()) {
            let attrs: HashMap<String, Value> = rs
                .unwrap()
                .attributes
                .iter()
                .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                .collect();
            return State {
                id: id.clone(),
                identifier: Some(identifier.to_string()),
                attributes: attrs,
                exists: true,
                dependency_bindings: rs.unwrap().dependency_bindings.clone(),
                partial_read: rs.unwrap().partial_read.clone(),
            };
        }
        State::not_found(id.clone())
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
            let id = Self::id_for_resource_state(rs);
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
                    attrs.insert(
                        "_binding".to_string(),
                        Value::Concrete(ConcreteValue::String(binding.clone())),
                    );
                }
                let state = State {
                    id: id.clone(),
                    identifier: Some(identifier.clone()),
                    attributes: attrs,
                    exists: true,
                    dependency_bindings: rs.dependency_bindings.clone(),
                    partial_read: rs.partial_read.clone(),
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
            let id = Self::id_for_resource_state(rs);
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
            r.identity = canonicalize_map_key_address(&r.identity);
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
            r.provider == provider && r.resource_type == resource_type && r.identity == identity
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

fn merge_deposed_generations(resource: &mut ResourceState, existing_deposed: Vec<DeposedInstance>) {
    for existing in existing_deposed {
        let exists_in_incoming = resource.deposed.iter().any(|incoming| {
            incoming.identifier == existing.identifier
                && incoming.provider_instance == existing.provider_instance
        });
        if !exists_in_incoming {
            resource.deposed.push(existing);
        }
    }
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
/// (`read_state` → caller takes `.state` or `.into_state()`).
#[derive(Debug)]
pub struct MigratedStateFile {
    pub state: StateFile,
    pub migration: Option<MigrationInfo>,
}

impl MigratedStateFile {
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
/// carina#3283 warning ("Disk state will be rewritten on the next
/// `carina apply` or `carina state refresh`") matches reality —
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
             on the next `carina apply` or `carina state refresh` in \
             that directory.",
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
            migration = Some(MigrationInfo {
                from: v,
                to: StateFile::CURRENT_VERSION,
            });
            let mut state: StateFile = serde_json::from_str(content).map_err(|e| {
                BackendError::InvalidState(format!(
                    "Failed to migrate state file from v{}: {}",
                    v, e
                ))
            })?;
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
            // current code never legitimately emits — `build_from_resource`
            // produces this shape only when `resource.attributes` is
            // empty, and the v8 writeback path emits `Unrecorded`
            // instead). Rewriting every top-level empty Struct to
            // `Unrecorded` on read makes the variant the single
            // source of truth and lets every `match` arm be exhaustive
            // again.
            if v <= 6 {
                migrate_v6_empty_struct_to_unrecorded(&mut state);
            }
            state.version = StateFile::CURRENT_VERSION;
            state
        }
    };
    // Map-key addresses written under the legacy `["..."]` shape are
    // rewritten to the canonical form on read so existing state files
    // resolve cleanly against new emissions. See #1903.
    state.canonicalize_addresses();
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeposedInstance {
    /// System-assigned unique key for this generation.
    pub key: DeposedKey,
    /// Provider-side identifier of the still-live old instance.
    pub identifier: String,
    /// Provider instance routing used when this generation was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_instance: Option<String>,
    /// Last known attributes of the old instance.
    pub attributes: HashMap<String, serde_json::Value>,
    /// Binding dependencies inherited from the old row.
    pub dependency_bindings: BTreeSet<String>,
}

/// State of a single managed resource
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceState {
    /// Resource type (e.g., "s3.Bucket", "ec2.Vpc")
    pub resource_type: String,
    /// Resource identity (from the DSL resource address identity)
    #[serde(alias = "name")]
    pub identity: String,
    /// Provider name (e.g., "aws")
    pub provider: String,
    /// AWS internal identifier (e.g., vpc-xxx, subnet-xxx)
    /// If None, the resource is considered not to exist
    #[serde(default)]
    pub identifier: Option<String>,
    /// All attributes of the resource as JSON values
    pub attributes: HashMap<String, serde_json::Value>,
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
    /// from the resource's authored `Value` via
    /// `carina_core::explicit::build_from_resource`.
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

/// Caller intent for treating already-persisted secret hashes as masking
/// authority when provider reads return plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviousSecretHashAuthority<'a> {
    None,
    /// The prior generation is the authority for every key that already
    /// contains a stored secret hash. Used for deposed generations and
    /// replacement snapshots whose old instance state is no longer described
    /// by the desired resource.
    AllPreviouslyHashedKeys(&'a HashMap<String, serde_json::Value>),
    /// The desired resource is authoritative for explicitly-authored keys.
    /// Previously stored hashes only protect keys now absent from desired
    /// state, preserving orphan recovery without blocking secret->plain
    /// demotion.
    OnlyKeysAbsentFromDesired(&'a HashMap<String, serde_json::Value>),
}

impl<'a> PreviousSecretHashAuthority<'a> {
    fn protects_key(self, resource: &Resource, key: &str) -> bool {
        match self {
            Self::None => false,
            Self::AllPreviouslyHashedKeys(_) => true,
            Self::OnlyKeysAbsentFromDesired(_) => !resource.attributes.contains_key(key),
        }
    }

    fn attributes(self) -> Option<&'a HashMap<String, serde_json::Value>> {
        match self {
            Self::None => None,
            Self::AllPreviouslyHashedKeys(attributes)
            | Self::OnlyKeysAbsentFromDesired(attributes) => Some(attributes),
        }
    }
}

impl ResourceState {
    /// Create a new resource state
    pub fn new(
        resource_type: impl Into<String>,
        identity: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            resource_type: resource_type.into(),
            identity: identity.into(),
            provider: provider.into(),
            identifier: None,
            attributes: HashMap::new(),
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
        self.partial_read = state.partial_read.clone();
        self
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
        resource_identity: impl Into<String>,
        bucket_name: impl Into<String>,
    ) -> Self {
        let bucket_name = bucket_name.into();
        Self::new(resource_type, resource_identity, provider)
            .with_identifier(&bucket_name)
            .with_attribute("bucket", serde_json::json!(bucket_name))
            .with_protected(true)
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

    fn attribute_to_state_json(
        resource_display_type: &str,
        identity: &str,
        key: &str,
        value: &Value,
    ) -> Result<serde_json::Value, String> {
        if contains_secret(value) {
            let ctx = SecretHashContext::new(resource_display_type, identity, key);
            value_to_json_with_context(value, Some(&ctx)).map_err(|e| e.to_string())
        } else {
            value_to_json(value).map_err(|e| e.to_string())
        }
    }

    /// Serialize best-effort provider attributes into the state-file JSON shape.
    ///
    /// Secret values use the same context-aware hashing as
    /// `from_provider_state_for_resource_and_schema`.
    /// Attributes that cannot be represented in state, such as unresolved
    /// `Unknown` placeholders, are skipped instead of aborting state writeback.
    fn attributes_to_state_json_lossy(
        resource_display_type: &str,
        identity: &str,
        attributes: &HashMap<String, Value>,
    ) -> HashMap<String, serde_json::Value> {
        attributes
            .iter()
            .filter_map(|(key, value)| {
                Self::attribute_to_state_json(resource_display_type, identity, key, value)
                    .ok()
                    .map(|json| (key.clone(), json))
            })
            .collect()
    }

    /// Serialize provider attributes with desired-resource and schema-derived
    /// secret overrides.
    ///
    /// When the new desired resource no longer contains an old protected key,
    /// the schema still prevents persisting provider-returned plaintext for
    /// write-only attributes. Such keys are dropped unless the desired value
    /// carries a `Secret(_)` wrapper that can be merged into a hash.
    pub fn attributes_to_state_json_lossy_for_resource_and_schema(
        resource: &Resource,
        schema: Option<&ResourceSchema>,
        attributes: &HashMap<String, Value>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
    ) -> HashMap<String, serde_json::Value> {
        let resource_display_type = resource.id.display_type();
        let identity = resource.id.identity_or_empty();
        let serialized =
            Self::attributes_to_state_json_lossy(&resource_display_type, identity, attributes);
        Self::protect_state_json_for_resource_and_schema(
            resource,
            schema,
            serialized,
            previous_hash_authority,
        )
    }

    /// Apply desired-resource and schema-derived secret protection to an
    /// already serialized state attribute map.
    pub fn protect_state_json_for_resource_and_schema(
        resource: &Resource,
        schema: Option<&ResourceSchema>,
        serialized: HashMap<String, serde_json::Value>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
    ) -> HashMap<String, serde_json::Value> {
        let mut serialized = serialized;
        let resource_display_type = resource.id.display_type();
        let identity = resource.id.identity_or_empty();
        let mut protected_secret_keys = std::collections::HashSet::new();

        for (key, desired_value) in &resource.attributes {
            if !contains_secret(desired_value) {
                continue;
            }

            let ctx = SecretHashContext::new(&resource_display_type, identity, key);
            let masked = if let Some(provider_json) = serialized.get(key).cloned() {
                merge_secrets_into_provider_json(desired_value, &provider_json, Some(&ctx))
            } else {
                value_to_json_with_context(desired_value, Some(&ctx))
            };

            if let Ok(masked) = masked {
                serialized.insert(key.clone(), masked);
                protected_secret_keys.insert(key.clone());
            } else {
                serialized.remove(key);
            }
        }

        Self::protect_previously_hashed_secrets(
            &resource_display_type,
            identity,
            resource,
            previous_hash_authority,
            &mut serialized,
            &mut protected_secret_keys,
        );

        if let Some(schema) = schema {
            for (key, attr) in &schema.attributes {
                if attr.write_only && !protected_secret_keys.contains(key) {
                    serialized.remove(key);
                }
            }
        }

        serialized
    }

    fn protect_previously_hashed_secrets(
        resource_display_type: &str,
        identity: &str,
        resource: &Resource,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
        serialized: &mut HashMap<String, serde_json::Value>,
        protected_secret_keys: &mut std::collections::HashSet<String>,
    ) {
        let Some(existing_attributes) = previous_hash_authority.attributes() else {
            return;
        };

        for (key, previous_value) in existing_attributes {
            if protected_secret_keys.contains(key)
                || !previous_hash_authority.protects_key(resource, key)
            {
                continue;
            }
            let secret_shape = secret_hash_shape(previous_value);
            if secret_shape == SecretHashShape::None {
                continue;
            }

            let ctx = SecretHashContext::new(resource_display_type, identity, key);
            let masked = match (secret_shape, serialized.get(key)) {
                (SecretHashShape::TopLevel, Some(provider_json))
                    if secret_hash_shape(provider_json) == SecretHashShape::TopLevel =>
                {
                    provider_json.clone()
                }
                (SecretHashShape::TopLevel, Some(provider_json)) => {
                    hash_provider_json_as_secret(provider_json, &ctx)
                        .unwrap_or_else(|_| previous_value.clone())
                }
                (SecretHashShape::TopLevel, None) => previous_value.clone(),
                (SecretHashShape::Nested, provider_json) => {
                    merge_previous_secret_hashes_into_provider_json(
                        previous_value,
                        provider_json,
                        &ctx,
                    )
                }
                (SecretHashShape::None, _) => unreachable!("None shape is filtered above"),
            };
            serialized.insert(key.clone(), masked);
            protected_secret_keys.insert(key.clone());
        }
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
        resource: &Resource,
        state: &State,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
    ) -> Result<Self, String> {
        let mut rs = Self::new(
            &resource.id.resource_type,
            resource.id.identity_or_empty(),
            resource.id.provider.clone(),
        );
        rs.identifier = state.identifier.clone();
        rs.partial_read = state.partial_read.clone();
        let resource_display_type = resource.id.display_type();
        let identity = resource.id.identity_or_empty();
        for (k, v) in &state.attributes {
            rs.attributes.insert(
                k.clone(),
                Self::attribute_to_state_json(&resource_display_type, identity, k, v)?,
            );
        }
        // For secret attributes, override the provider-returned plain value
        // with the Argon2id hash. The provider returns the actual value (since
        // secrets are unwrapped before sending), but state should only store
        // the hash to avoid persisting sensitive data.
        // For nested secrets (inside Maps/Lists), merge the hashed values into
        // the provider-returned structure to preserve extra keys from the provider.
        let mut protected_secret_keys = std::collections::HashSet::new();
        for (k, v) in &resource.attributes {
            if contains_secret(v) {
                let ctx = SecretHashContext::new(&resource_display_type, identity, k);
                if let Some(provider_json) = rs.attributes.get(k).cloned() {
                    rs.attributes.insert(
                        k.clone(),
                        merge_secrets_into_provider_json(v, &provider_json, Some(&ctx))
                            .map_err(|e| e.to_string())?,
                    );
                } else {
                    rs.attributes.insert(
                        k.clone(),
                        Self::attribute_to_state_json(&resource_display_type, identity, k, v)?,
                    );
                }
                protected_secret_keys.insert(k.clone());
            }
        }
        if let Some(existing) = existing {
            Self::protect_previously_hashed_secrets(
                &resource_display_type,
                identity,
                resource,
                PreviousSecretHashAuthority::OnlyKeysAbsentFromDesired(&existing.attributes),
                &mut rs.attributes,
                &mut protected_secret_keys,
            );
            rs.protected = existing.protected;
            rs.name_overrides = existing.name_overrides.clone();
        }
        if let Some(schema) = schema {
            for (key, attr) in &schema.attributes {
                if attr.write_only && !protected_secret_keys.contains(key) {
                    rs.attributes.remove(key);
                }
            }
        }
        rs.directives = resource.directives.clone();
        if rs.directives.provider_instance.is_none() {
            rs.directives.provider_instance = resource.id.provider_instance.clone();
        }
        rs.prefixes = resource.prefixes.clone();
        // Record the structural shape the user wrote in their .crn,
        // so the differ can project actual-state through it and skip
        // server-side defaults the user never authored (refs awscc#206).
        // carina#3280: when `build_from_resource` would produce an
        // empty top-level `Struct` (the user authored no attributes —
        // bodyless resource like `aws.sts.CallerIdentity {}`, or the
        // `carina state import` path that constructs a `Resource`
        // with no DSL attributes), emit `Unrecorded` instead. The
        // empty-Struct-at-top-level shape used to double as the
        // legacy-corruption marker, which forced callers to
        // disambiguate by runtime convention; `Unrecorded` is the
        // typed signal for "no authoring record".
        let built = explicit::build_from_resource(resource);
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
                //    the provider's read). Preserve the populated
                //    `Struct` — collapsing it to `Unrecorded` would
                //    flip-flop the row on every apply.
                //
                // 3. Green-field write: prior row is `None` or
                //    `Leaf`. There is genuinely no authoring record
                //    yet. Emit `Unrecorded`.
                match existing.map(|e| &e.explicit) {
                    Some(ExplicitFields::Unrecorded) if !state.attributes.is_empty() => {
                        let rebuilt: HashMap<String, ExplicitFields> = state
                            .attributes
                            .iter()
                            .filter(|(k, _)| !k.starts_with('_'))
                            .map(|(k, v)| (k.clone(), explicit::build_from_value(v)))
                            .collect();
                        if rebuilt.is_empty() {
                            ExplicitFields::Unrecorded
                        } else {
                            ExplicitFields::Struct { children: rebuilt }
                        }
                    }
                    Some(ExplicitFields::Struct { children }) if !children.is_empty() => {
                        // Idempotent re-apply: keep the populated
                        // authoring record. Cloning the children is
                        // cheap (HashMap of small Leaf/Struct trees).
                        ExplicitFields::Struct {
                            children: children.clone(),
                        }
                    }
                    Some(ExplicitFields::List { element }) => {
                        // Top-level `List` is structurally improbable
                        // (`build_from_resource` always produces a
                        // root `Struct`), but if a prior write ever
                        // landed one, preserve it for the same
                        // idempotency reason.
                        ExplicitFields::List {
                            element: element.clone(),
                        }
                    }
                    // None (no prior row), Leaf (default), or
                    // Some(Struct { children: {} }) — the last is
                    // unreachable post-migration but kept here so a
                    // pre-migration in-memory `StateFile` (e.g.
                    // constructed in tests) still emits the typed
                    // signal rather than the ambiguous shape.
                    _ => ExplicitFields::Unrecorded,
                }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretHashShape {
    None,
    TopLevel,
    Nested,
}

fn secret_hash_shape(value: &serde_json::Value) -> SecretHashShape {
    match value {
        serde_json::Value::String(stored) if stored.starts_with(SECRET_PREFIX) => {
            SecretHashShape::TopLevel
        }
        serde_json::Value::Array(items) if items.iter().any(contains_secret_hash_json) => {
            SecretHashShape::Nested
        }
        serde_json::Value::Object(map) if map.values().any(contains_secret_hash_json) => {
            SecretHashShape::Nested
        }
        _ => SecretHashShape::None,
    }
}

fn contains_secret_hash_json(value: &serde_json::Value) -> bool {
    secret_hash_shape(value) != SecretHashShape::None
}

fn merge_previous_secret_hashes_into_provider_json(
    previous_value: &serde_json::Value,
    provider_value: Option<&serde_json::Value>,
    context: &SecretHashContext,
) -> serde_json::Value {
    match previous_value {
        serde_json::Value::String(stored) if stored.starts_with(SECRET_PREFIX) => {
            let Some(provider_value) = provider_value else {
                return previous_value.clone();
            };
            if secret_hash_shape(provider_value) == SecretHashShape::TopLevel {
                return provider_value.clone();
            }
            hash_provider_json_as_secret(provider_value, context)
                .unwrap_or_else(|_| previous_value.clone())
        }
        serde_json::Value::Object(previous_object) => {
            let Some(serde_json::Value::Object(provider_object)) = provider_value else {
                return previous_value.clone();
            };
            let mut merged = provider_object.clone();
            for (key, previous_child) in previous_object {
                if contains_secret_hash_json(previous_child) {
                    let provider_child = provider_object.get(key);
                    merged.insert(
                        key.clone(),
                        merge_previous_secret_hashes_into_provider_json(
                            previous_child,
                            provider_child,
                            context,
                        ),
                    );
                }
            }
            serde_json::Value::Object(merged)
        }
        serde_json::Value::Array(previous_items) => {
            let Some(serde_json::Value::Array(provider_items)) = provider_value else {
                return previous_value.clone();
            };
            if !previous_secret_hash_array_alignment_is_trusted(previous_items, provider_items) {
                return provider_value
                    .and_then(|provider| hash_provider_json_as_secret(provider, context).ok())
                    .unwrap_or_else(|| previous_value.clone());
            }
            let mut merged = provider_items.clone();
            for (index, previous_child) in previous_items.iter().enumerate() {
                if contains_secret_hash_json(previous_child) {
                    let merged_child = merge_previous_secret_hashes_into_provider_json(
                        previous_child,
                        provider_items.get(index),
                        context,
                    );
                    merged[index] = merged_child;
                }
            }
            serde_json::Value::Array(merged)
        }
        _ => provider_value
            .cloned()
            .unwrap_or_else(|| previous_value.clone()),
    }
}

fn previous_secret_hash_array_alignment_is_trusted(
    previous_items: &[serde_json::Value],
    provider_items: &[serde_json::Value],
) -> bool {
    if previous_items.len() != provider_items.len() {
        return false;
    }

    previous_items
        .iter()
        .zip(provider_items)
        .filter(|(previous, _)| contains_secret_hash_json(previous))
        .all(|(previous, provider)| hash_bearing_item_alignment_is_trusted(previous, provider))
}

fn hash_bearing_item_alignment_is_trusted(
    previous: &serde_json::Value,
    provider: &serde_json::Value,
) -> bool {
    let (serde_json::Value::Object(previous_object), serde_json::Value::Object(provider_object)) =
        (previous, provider)
    else {
        return false;
    };

    let anchors: Vec<_> = previous_object
        .iter()
        .filter(|(_, value)| !contains_secret_hash_json(value))
        .collect();
    if anchors.is_empty() {
        return false;
    }

    anchors
        .into_iter()
        .all(|(key, previous_value)| provider_object.get(key) == Some(previous_value))
}

fn hash_provider_json_as_secret(
    provider_json: &serde_json::Value,
    context: &SecretHashContext,
) -> Result<serde_json::Value, String> {
    let provider_value = json_to_dsl_value(provider_json)
        .ok_or_else(|| "cannot hash null provider secret".to_string())?;
    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(provider_value)));
    value_to_json_with_context(&secret_value, Some(context)).map_err(|err| err.to_string())
}

/// Returns true if the `ExplicitFields` is the default (`Leaf`) — used
/// as a `skip_serializing_if` predicate so resources that have not yet
/// been touched by `from_provider_state_for_resource_and_schema` (or that legitimately have no
/// authored attributes) don't emit a verbose `"explicit": {"kind": "leaf"}`
/// line.
fn is_empty_explicit(e: &ExplicitFields) -> bool {
    matches!(e, ExplicitFields::Leaf)
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
            rs.provider == provider && rs.resource_type == resource_type && rs.identity == identity
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
/// current `build_from_resource` produces this shape only when
/// `resource.attributes` is empty, and the v8 writeback path emits
/// `Unrecorded` for that case instead. Migrating eliminates the
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

#[cfg(test)]
mod tests;
