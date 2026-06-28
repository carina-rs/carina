//! State file structures for persisting infrastructure state

use carina_core::deps::get_resource_dependencies;
use carina_core::explicit::{self, ExplicitFields};
pub use carina_core::plan::NameOverride;
use carina_core::resource::{
    ConcreteValue, Directives, PartialReadMarker, Resource, ResourceId, State, Value,
};
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
    /// v6: Replaced flat `desired_keys: Vec<String>` with recursive
    ///     `explicit: ExplicitFields` (refs awscc#206). Reads of v5
    ///     state lift each top-level key to a `Leaf` child of the
    ///     root `Struct`; the next plan/apply rebuilds a full tree
    ///     from the resource's authored `Value`.
    /// v7: Replaced top-level empty explicit structs with `Unrecorded`.
    /// v8: Replaced `name_overrides: HashMap<String, String>` with
    ///     `HashMap<String, NameOverride>` so permanent CBD temporary
    ///     names remember the DSL value that produced them.
    pub const CURRENT_VERSION: u32 = 8;

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
    /// `resource_name` must equal the desired resource's resolved name
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
    /// assert_eq!(state.resources[0].name, "aws_s3_bucket_a3f2b1c8");
    /// ```
    pub fn with_managed_state_bucket(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        resource_name: impl Into<String>,
        bucket_name: impl Into<String>,
    ) -> Self {
        let mut state = Self::new();
        state.upsert_resource(ResourceState::managed_state_bucket(
            provider,
            resource_type,
            resource_name,
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

    /// Build a map of ResourceId -> Directives from this state file.
    pub fn build_directives(&self) -> HashMap<ResourceId, Directives> {
        let mut directives_map = HashMap::new();
        for rs in &self.resources {
            let id = ResourceId::with_provider(
                &rs.provider,
                &rs.resource_type,
                &rs.name,
                rs.directives.provider_instance.clone(),
            );
            directives_map.insert(id, rs.directives.clone());
        }
        directives_map
    }

    /// Build a map of saved attributes, converting JSON values to DSL values.
    pub fn build_saved_attrs(&self) -> HashMap<ResourceId, HashMap<String, Value>> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            let id = ResourceId::with_provider(
                &rs.provider,
                &rs.resource_type,
                &rs.name,
                rs.directives.provider_instance.clone(),
            );
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
                .find_resource(&id.provider, &id.resource_type, id.name_str())
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
                let id = ResourceId::with_provider(
                    &rs.provider,
                    &rs.resource_type,
                    &rs.name,
                    rs.directives.provider_instance.clone(),
                );
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
        let rs = self.find_resource(&id.provider, &id.resource_type, id.name_str());
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
            let id = ResourceId::with_provider(
                &rs.provider,
                &rs.resource_type,
                &rs.name,
                rs.directives.provider_instance.clone(),
            );
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
            let id = ResourceId::with_provider(
                &rs.provider,
                &rs.resource_type,
                &rs.name,
                rs.directives.provider_instance.clone(),
            );
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
    pub fn build_name_overrides(&self) -> HashMap<ResourceId, HashMap<String, NameOverride>> {
        let mut result = HashMap::new();
        for rs in &self.resources {
            if !rs.name_overrides.is_empty() {
                let id = ResourceId::with_provider(
                    &rs.provider,
                    &rs.resource_type,
                    &rs.name,
                    rs.directives.provider_instance.clone(),
                );
                result.insert(id, rs.name_overrides.clone());
            }
        }
        result
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
        if info.from <= 7 && info.to >= 8 {
            eprintln!(
                "Warning: v7 -> v8 migration: override original values are unknown; \
                 the first apply may overwrite an in-flight DSL rename. Re-run plan to verify."
            );
        }
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
            // empty, and the v7 writeback path emits `Unrecorded`
            // instead). Rewriting every top-level empty Struct to
            // `Unrecorded` on read makes the variant the single
            // source of truth and lets every `match` arm be exhaustive
            // again.
            if v <= 6 {
                migrate_v6_empty_struct_to_unrecorded(&mut state);
            }
            // v7 → v8: `ResourceState.name_overrides` changed from
            // `HashMap<String, String>` to `HashMap<String, NameOverride>`.
            // The concrete lift is handled by `NameOverride`'s untagged
            // serde reader (legacy bare String → `{ temp_value, original_value: "" }`).
            // Keep this no-op migration step explicit so future wire-shape
            // changes do not accidentally depend on that fallback silently.
            if v <= 7 {
                migrate_v7_name_overrides_to_structs(&mut state);
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
    // A managed resource that has never returned an identifier from
    // the provider never reaches state writeback (it lands as
    // `add_cleanup` instead), so identifier=None in `state.resources`
    // is exclusively a historical-artifact shape.
    state.resources.retain(|rs| rs.identifier.is_some());
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
    /// Carina-side directives persisted from the DSL `directives` block.
    #[serde(default)]
    pub directives: Directives,
    /// Attribute prefixes used to generate names (e.g., {"bucket_name": "my-app-"})
    #[serde(default)]
    pub prefixes: HashMap<String, String>,
    /// Permanent name overrides from create_before_destroy with name attributes.
    /// Maps attribute name to the permanent temporary name and the DSL
    /// value that produced it.
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
    /// `resource_name` is the state-side resource name and must equal the
    /// resolved name of the desired resource (anonymous resources get a
    /// hash-derived id like `aws_s3_bucket_<hash>`); `bucket_name` is the
    /// AWS bucket name used as the provider identifier and as the value
    /// of the `bucket` attribute. Mixing the two breaks the differ — see
    /// #2533 for the failure mode.
    pub fn managed_state_bucket(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        resource_name: impl Into<String>,
        bucket_name: impl Into<String>,
    ) -> Self {
        let bucket_name = bucket_name.into();
        Self::new(resource_type, resource_name, provider)
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
        rs.partial_read = state.partial_read.clone();
        for (k, v) in &state.attributes {
            rs.attributes
                .insert(k.clone(), value_to_json(v).map_err(|e| e.to_string())?);
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
                        merge_secrets_into_provider_json(v, &provider_json, Some(&ctx))
                            .map_err(|e| e.to_string())?,
                    );
                } else {
                    rs.attributes.insert(
                        k.clone(),
                        carina_core::value::value_to_json_with_context(v, Some(&ctx))
                            .map_err(|e| e.to_string())?,
                    );
                }
            }
        }
        if let Some(existing) = existing {
            rs.protected = existing.protected;
            rs.name_overrides = existing.name_overrides.clone();
        }
        rs.directives = resource.directives.clone();
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

/// Returns true if the `ExplicitFields` is the default (`Leaf`) — used
/// as a `skip_serializing_if` predicate so resources that have not yet
/// been touched by `from_provider_state` (or that legitimately have no
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
/// Resources are matched by `(provider, resource_type, name)` because
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
        let name = raw_rs.get("name").and_then(|v| v.as_str());
        let Some(((provider, resource_type), name)) = provider.zip(resource_type).zip(name) else {
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
            rs.provider == provider && rs.resource_type == resource_type && rs.name == name
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
/// `resource.attributes` is empty, and the v7 writeback path emits
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

/// v7 → v8: state files written under v7 encode name overrides as
/// bare strings (`"name": "tmp"`). `NameOverride`'s untagged
/// deserializer has already lifted those into
/// `NameOverride { temp_value, original_value: "" }` by the time this
/// helper runs. The empty body is intentional: it documents the migration
/// seam so a future `NameOverride` wire-shape change has an explicit place
/// to preserve v7 compatibility.
fn migrate_v7_name_overrides_to_structs(_state: &mut StateFile) {}

#[cfg(test)]
mod tests;
