//! State file structures for persisting infrastructure state

use carina_core::binding_index::SecretPreservingAttributes;
use carina_core::deps::get_resource_dependencies;
use carina_core::differ::{ComparisonContext, Orphan};
use carina_core::effect::{MoveAuthority, ReleaseListKeysEffect};
use carina_core::executor::{AcceptedRequest, AcceptedRequestKind};
use carina_core::explicit::{self, ExplicitFields};
use carina_core::name_override::NameOverrideSource;
pub use carina_core::name_override::{ApplyDecision, NameOverride, should_apply_override};
use carina_core::parser::StateBlockAddress;
use carina_core::plan::ReplacementPreviousAttributesForStateWriteback;
pub use carina_core::provider::PreviousSecretHashAuthority;
use carina_core::provider::{
    AppliedIdentifier, CreateObservation, EmptyAppliedIdentifier, InFlightCreate,
    InFlightPersistence, NestedWriteOnlyRecord, ObservedCreate, PersistedAttributes,
    PersistedAttributesError, RawSavedAttrs, Sent, SentDesiredAttributes, SentRecord,
    StateRowInFlightCreate, StateRowUnknownCreateOutcome, StateTransformMap, UnknownCreateOutcome,
    merge_previous_secret_hashes_into_provider_value, previous_hash,
};
pub use carina_core::resource::DeposedKey;
#[cfg(test)]
use carina_core::resource::Resource;
#[cfg(any(test, feature = "test-utils"))]
use carina_core::resource::absolute_binding_address_for_test;
use carina_core::resource::{
    AbsoluteBindingAddress, ConcreteValue, Current, DataSource, Directives, PartialReadMarker,
    PendingReRead, PersistedDirectives, ResolvedResource, ResourceId, ResourceIdentity, State,
    Value, canonicalize_persisted_absolute_binding_address,
};
use carina_core::schema::{
    FieldName, ResourceSchema, SchemaKind, SchemaRegistry, schema_write_only_keys,
};
use carina_core::value::{
    PersistedExportValue, PersistedValue, SerializationError, contains_unredacted_secret,
    merge_secrets_into_observation_json, merge_secrets_into_provider_json,
    union_secret_shapes_after_resolution,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::backend::{BackendError, InvalidResourceIdentityError};

#[cfg(any(test, feature = "test-utils"))]
fn persisted_attributes_from_test_json(
    attributes: HashMap<String, serde_json::Value>,
) -> PersistedAttributes {
    PersistedAttributes::from_encoded(attributes)
        .expect("test state row contains valid persisted JSON")
}

/// Select persisted values that count as recorded write-only state.
///
/// Schema-backed paths infer the set from every schema write-only key present
/// in the persisted map, including rows written before metadata was reliable.
/// Orphan refresh additionally supplies the row's recorded list so it can
/// preserve values when the schema is unavailable or no longer marks a key
/// write-only.
fn recorded_write_only_keys(
    schema: Option<&ResourceSchema>,
    attributes: &PersistedAttributes,
    orphan_recorded: &[String],
) -> HashSet<String> {
    let mut keys = schema_write_only_keys(schema);
    keys.extend(orphan_recorded.iter().cloned());
    keys.retain(|key| attributes.contains_key(key));
    keys
}

/// Provider values selected for a state row before secret protection.
///
/// Construction always strips every write-only key known from the schema (and
/// any state-row metadata supplied by the orphan path). Such a key can re-enter
/// only through the private accepted-request or persisted-generation overlay
/// methods below. Keeping this type private prevents downstream callers from
/// relabelling an arbitrary provider map as provenance-safe.
#[derive(Debug)]
struct SelectedStateAttributes {
    encoded: HashMap<String, PersistedValue>,
    write_only_keys: HashSet<String>,
    retained_write_only: HashSet<String>,
}

impl SelectedStateAttributes {
    fn write_only_keys(
        schema: Option<&ResourceSchema>,
        additionally_recorded: &[String],
    ) -> HashSet<String> {
        let mut keys = schema_write_only_keys(schema);
        keys.extend(additionally_recorded.iter().cloned());
        keys
    }

    fn from_provider_values_lossy(
        attributes: &HashMap<String, Value>,
        schema: Option<&ResourceSchema>,
        additionally_recorded: &[String],
    ) -> Self {
        let write_only_keys = Self::write_only_keys(schema, additionally_recorded);
        let encoded = attributes
            .iter()
            .filter(|(key, _)| !write_only_keys.contains(*key))
            .filter_map(|(key, value)| {
                PersistedValue::encode_provider_value(value, None)
                    .ok()
                    .map(|value| (key.clone(), value))
            })
            .collect();
        Self {
            encoded,
            write_only_keys,
            retained_write_only: HashSet::new(),
        }
    }

    fn try_from_provider_values(
        attributes: &HashMap<String, Value>,
        schema: Option<&ResourceSchema>,
        additionally_recorded: &[String],
        preferred_hashes: Option<&PersistedAttributes>,
        previous: Option<&PersistedAttributes>,
    ) -> Result<Self, SerializationError> {
        let write_only_keys = Self::write_only_keys(schema, additionally_recorded);
        let encoded = attributes
            .iter()
            .filter(|(key, _)| !write_only_keys.contains(*key))
            .map(|(key, value)| {
                PersistedValue::encode_provider_value(
                    value,
                    previous_hash(preferred_hashes, previous, key),
                )
                .map(|value| (key.clone(), value))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            encoded,
            write_only_keys,
            retained_write_only: HashSet::new(),
        })
    }

    fn from_persisted_generation(
        attributes: &PersistedAttributes,
        schema: Option<&ResourceSchema>,
        additionally_recorded: &[String],
    ) -> Self {
        let write_only_keys = Self::write_only_keys(schema, additionally_recorded);
        let encoded = attributes
            .keys()
            .filter_map(|key| {
                attributes
                    .encoded_value(key)
                    .cloned()
                    .map(|value| (key.clone(), value))
            })
            .collect::<HashMap<_, _>>();
        let retained_write_only = write_only_keys
            .iter()
            .filter(|key| encoded.contains_key(*key))
            .cloned()
            .collect();
        Self {
            encoded,
            write_only_keys,
            retained_write_only,
        }
    }

    fn insert_write_only(&mut self, key: String, value: PersistedValue) {
        self.write_only_keys.insert(key.clone());
        self.retained_write_only.insert(key.clone());
        self.encoded.insert(key, value);
    }

    fn carry_write_only(
        &mut self,
        previous: &PersistedAttributes,
        keys: impl IntoIterator<Item = String>,
    ) {
        for key in keys {
            if let Some(value) = previous.encoded_value(&key).cloned() {
                self.insert_write_only(key, value);
            }
        }
    }

    fn overlay_sent(&mut self, sent: &PersistedAttributes, sent_write_only: &HashSet<String>) {
        for key in sent.keys() {
            let Some(value) = sent.encoded_value(key).cloned() else {
                continue;
            };
            if sent_write_only.contains(key) {
                self.insert_write_only(key.clone(), value);
            } else if !self.write_only_keys.contains(key) {
                self.encoded.entry(key.clone()).or_insert(value);
            }
        }
    }

    fn authority_attributes(
        authority: PreviousSecretHashAuthority<'_>,
    ) -> Option<&PersistedAttributes> {
        match authority {
            PreviousSecretHashAuthority::None => None,
            PreviousSecretHashAuthority::AllPreviouslyHashedKeys(attributes)
            | PreviousSecretHashAuthority::OnlyKeysAbsentFromDesired(attributes) => {
                Some(attributes)
            }
        }
    }

    fn authority_protects_key(
        authority: PreviousSecretHashAuthority<'_>,
        key_is_desired: bool,
    ) -> bool {
        match authority {
            PreviousSecretHashAuthority::None => false,
            PreviousSecretHashAuthority::AllPreviouslyHashedKeys(_) => true,
            PreviousSecretHashAuthority::OnlyKeysAbsentFromDesired(_) => !key_is_desired,
        }
    }

    fn protect_previous_secret_hashes(
        &mut self,
        authority: PreviousSecretHashAuthority<'_>,
        mut is_desired_key: impl FnMut(&str) -> bool,
        protected_secret_keys: &mut HashSet<String>,
    ) {
        let Some(previous) = Self::authority_attributes(authority) else {
            return;
        };
        let keys = previous.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            if protected_secret_keys.contains(&key)
                || !Self::authority_protects_key(authority, is_desired_key(&key))
                || (self.write_only_keys.contains(&key) && !self.retained_write_only.contains(&key))
            {
                continue;
            }
            let Some(previous_value) = previous.encoded_value(&key) else {
                continue;
            };
            let Some(masked) = merge_previous_secret_hashes_into_provider_value(
                previous_value,
                self.encoded.get(&key),
            ) else {
                continue;
            };
            self.encoded.insert(key.clone(), masked);
            protected_secret_keys.insert(key);
        }
    }

    fn protect_for_resource<S>(
        mut self,
        resource: &ResolvedResource<S>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
        preferred_hashes: Option<&PersistedAttributes>,
        strict_desired: bool,
    ) -> Result<Self, SerializationError> {
        let mut protected_secret_keys = HashSet::new();
        for (key, desired_value) in &resource.attributes {
            let is_write_only = self.write_only_keys.contains(key);
            let is_retained_write_only = is_write_only && self.retained_write_only.contains(key);
            if is_write_only && !is_retained_write_only {
                continue;
            }
            if !contains_unredacted_secret(desired_value) {
                continue;
            }
            let previous = previous_hash(
                preferred_hashes,
                Self::authority_attributes(previous_hash_authority),
                key,
            );
            let masked = match self.encoded.get(key).cloned() {
                Some(selected_value) if is_retained_write_only => {
                    merge_secrets_into_observation_json(desired_value, &selected_value, previous)
                }
                Some(provider_value) => {
                    merge_secrets_into_provider_json(desired_value, &provider_value, previous)
                }
                None if is_write_only => continue,
                None => PersistedValue::encode_desired_value(desired_value, previous),
            };
            match masked {
                Ok(masked) => {
                    self.encoded.insert(key.clone(), masked);
                    protected_secret_keys.insert(key.clone());
                }
                Err(error) if strict_desired => return Err(error),
                Err(_) => {
                    self.encoded.remove(key);
                }
            }
        }
        self.protect_previous_secret_hashes(
            previous_hash_authority,
            |key| resource.attributes.contains_key(key),
            &mut protected_secret_keys,
        );
        Ok(self)
    }

    fn protect_observation<S>(
        mut self,
        resource: &ResolvedResource<S>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
        preferred_hashes: Option<&PersistedAttributes>,
    ) -> Result<Self, SerializationError> {
        let mut protected_secret_keys = HashSet::new();
        for (key, desired_value) in &resource.attributes {
            if !contains_unredacted_secret(desired_value) {
                continue;
            }
            let Some(observed_or_sent_value) = self.encoded.get(key).cloned() else {
                continue;
            };
            let previous = previous_hash(
                preferred_hashes,
                Self::authority_attributes(previous_hash_authority),
                key,
            );
            let masked = merge_secrets_into_observation_json(
                desired_value,
                &observed_or_sent_value,
                previous,
            )?;
            self.encoded.insert(key.clone(), masked);
            protected_secret_keys.insert(key.clone());
        }

        let present_keys = self.encoded.keys().cloned().collect::<HashSet<_>>();
        self.protect_previous_secret_hashes(
            previous_hash_authority,
            |key| resource.attributes.contains_key(key),
            &mut protected_secret_keys,
        );
        self.encoded.retain(|key, _| present_keys.contains(key));
        Ok(self)
    }

    fn protect_orphan(mut self, previous: &PersistedAttributes) -> Self {
        self.protect_previous_secret_hashes(
            PreviousSecretHashAuthority::AllPreviouslyHashedKeys(previous),
            |_| false,
            &mut HashSet::new(),
        );
        self
    }

    fn finish(mut self) -> Result<(PersistedAttributes, Vec<String>), PersistedAttributesError> {
        self.retained_write_only
            .retain(|key| self.encoded.contains_key(key));
        let mut write_only_attributes = self.retained_write_only.into_iter().collect::<Vec<_>>();
        write_only_attributes.sort();
        let attributes = PersistedAttributes::from_persisted_values(self.encoded)?;
        Ok((attributes, write_only_attributes))
    }
}

/// A provider or desired value could not be encoded into persisted state.
#[derive(Debug, thiserror::Error)]
pub enum ResourceStateEncodingError {
    #[error(transparent)]
    Serialization(#[from] SerializationError),
    #[error(transparent)]
    PersistedAttributes(#[from] PersistedAttributesError),
    #[error("sent attributes are outside the desired resource: {attributes:?}")]
    InvalidSentAttributes { attributes: Vec<String> },
}

/// Provenance for all desired-backed metadata while rebuilding a provider row.
///
/// Coupling write-only and authoring provenance makes invalid combinations
/// unrepresentable: an accepted request always establishes the desired
/// authoring record, while a carried refresh rebuilds current desired keys
/// against fresh stored values and retains live pending removals at any
/// authored depth from the prior record. Ambiguous list-element removals are
/// retained as whole-value comparisons rather than assigned to guessed
/// elements.
#[derive(Debug, Clone, Copy)]
pub enum RowBasis<'a> {
    /// A provider accepted this create or update request.
    Applied(&'a AcceptedRequest),
    /// Import established the desired authoring record without sending values.
    Imported,
    /// No desired write completed; rebuild desired metadata while retaining
    /// live pending removals at any authored depth from known prior metadata.
    /// Ambiguous list-element removals remain whole-value comparisons.
    Carried,
}

#[derive(Debug, Clone)]
enum DesiredAuthoringBasis<'a> {
    Sent(SentDesiredAttributes<'a>),
    DesiredRead,
    CarriedRead,
}

/// Published values read without decoding managed resource rows.
///
/// Construction is restricted to [`Self::from_state_bytes`], so exports-only
/// consumers cannot accidentally cross the full state-format version gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedExports {
    values: HashMap<String, PersistedExportValue>,
}

#[derive(Deserialize)]
struct PublishedExportsEnvelope {
    version: u32,
    exports_version: Option<u32>,
    #[serde(default)]
    exports: HashMap<String, serde_json::Value>,
}

impl PublishedExports {
    /// Current persisted exports encoding version.
    ///
    /// Bump this only when [`PersistedExportValue`]'s encoding changes. The
    /// state-format changes in v12, v13, and v14 would each have required an
    /// exports-version bump if this independent contract had existed then.
    pub const CURRENT_VERSION: u32 = 1;

    /// Read the published-exports envelope from raw state-file bytes.
    ///
    /// Current exports encodings are decoded directly without deserializing
    /// resource rows or checking [`StateFile::CURRENT_VERSION`]. State files
    /// written before the independent exports version existed are migrated
    /// through the legacy full-state path because their value encoding may
    /// require the v12-v14 migrations.
    pub fn from_state_bytes(bytes: &[u8]) -> Result<Self, BackendError> {
        Self::decode_state_bytes(bytes).map(|(exports, _)| exports)
    }

    pub(crate) fn decode_state_bytes(
        bytes: &[u8],
    ) -> Result<(Self, Option<MigrationInfo>), BackendError> {
        let envelope: PublishedExportsEnvelope =
            serde_json::from_slice(bytes).map_err(|error| {
                BackendError::InvalidState(format!(
                    "Failed to parse published exports envelope: {error}"
                ))
            })?;

        match classify_exports_version(envelope.version, envelope.exports_version)
            .map_err(|error| map_state_file_decode_error(error, None))?
        {
            ExportsVersionClassification::Current => decode_persisted_exports(envelope.exports)
                .map(|values| (Self { values }, None))
                .map_err(|error| map_state_file_decode_error(error, None)),
            ExportsVersionClassification::Legacy => {
                let (state, migration) = check_and_migrate_bytes(bytes)?.into_parts();
                Ok((
                    Self {
                        values: state.exports,
                    },
                    migration,
                ))
            }
        }
    }

    pub fn values(&self) -> &HashMap<String, PersistedExportValue> {
        &self.values
    }

    /// Decode published values into the binding map consumed by
    /// `upstream_state`.
    pub fn build_remote_bindings(&self) -> HashMap<String, Value> {
        self.values
            .iter()
            .filter_map(|(key, value)| value.decode().map(|value| (key.clone(), value)))
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
struct CurrentExportsVersion;

impl Serialize for CurrentExportsVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(PublishedExports::CURRENT_VERSION)
    }
}

/// The main state file structure that persists to the backend
#[derive(Debug, Clone, Serialize)]
pub struct StateFile {
    /// State file format version
    pub version: u32,
    #[serde(rename = "exports_version")]
    exports_version_marker: CurrentExportsVersion,
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

/// Classification used by import planning without weakening the general
/// "occupied state rows make import a no-op" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportTargetDisposition {
    Vacant,
    ReplaceCreateOutcomeUnknown,
    Occupied,
}

/// Why an address could not produce the proof required by `state forget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotOutcomeUnknownKind {
    Missing,
    DifferentLifecycle,
}

/// Typed rejection from [`StateFile::find_outcome_unknown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotOutcomeUnknown {
    Missing { address: StateBlockAddress },
    DifferentLifecycle { address: StateBlockAddress },
}

impl NotOutcomeUnknown {
    pub fn kind(&self) -> NotOutcomeUnknownKind {
        match self {
            Self::Missing { .. } => NotOutcomeUnknownKind::Missing,
            Self::DifferentLifecycle { .. } => NotOutcomeUnknownKind::DifferentLifecycle,
        }
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "no state row exists at this address",
            Self::DifferentLifecycle { .. } => {
                "no state row at this address is a create whose outcome is unknown"
            }
        }
    }

    fn fmt_with_project_dir(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        project_dir: &std::path::Path,
    ) -> std::fmt::Result {
        let address = match self {
            Self::Missing { address } | Self::DifferentLifecycle { address } => address,
        };
        let reason = self.reason();
        let resource_type = address.display_type().to_string();
        let forget_command = carina_core::hint::ProjectCommand::new("state forget", project_dir)
            .with_argument(&resource_type)
            .with_argument(address.name_str());
        let identity = carina_core::parser::display_string_literal(address.name_str());
        write!(
            f,
            "{reason}; `{forget_command}` only accepts create-outcome-unknown rows; use `removed {{ from = {} {identity} }}` for ordinary state removal",
            address.display_type(),
        )
    }

    /// Render this rejection with the exact project-scoped command that was
    /// rejected.
    pub fn display_with_project_dir<'a>(
        &'a self,
        project_dir: &'a std::path::Path,
    ) -> impl std::fmt::Display + 'a {
        NotOutcomeUnknownDisplay {
            error: self,
            project_dir,
        }
    }
}

struct NotOutcomeUnknownDisplay<'a> {
    error: &'a NotOutcomeUnknown,
    project_dir: &'a std::path::Path,
}

impl std::fmt::Display for NotOutcomeUnknownDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt_with_project_dir(f, self.project_dir)
    }
}

impl std::fmt::Display for NotOutcomeUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for NotOutcomeUnknown {}

/// Exclusive proof that one row currently has an outcome-unknown create.
///
/// The mutable borrow prevents the state file from changing between lookup
/// and consumption, and the fields are private so callers cannot forge it.
#[derive(Debug)]
pub struct OutcomeUnknownRow<'a> {
    state: &'a mut StateFile,
    index: usize,
}

impl OutcomeUnknownRow<'_> {
    /// Borrow the exact protected row selected by the typed lookup.
    pub fn resource(&self) -> &ResourceState {
        &self.state.resources[self.index]
    }
}

/// Persisted orphans classified per row for safe provider refresh.
///
/// Converting to planner input preserves malformed rows as existing states
/// without identifiers, so the differ emits
/// [`carina_core::plan::PlanErrorKind::MissingProviderIdentifier`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedOrphans {
    by_id: HashMap<ResourceId, Orphan>,
}

impl LoadedOrphans {
    pub fn insert(&mut self, orphan: Orphan) {
        let id = match orphan.current() {
            Current::Absent(state) => state.state().id.clone(),
            Current::Present(present) => present.state().id.clone(),
            Current::CreateInFlight(pending) => pending.id().clone(),
            Current::CreateOutcomeUnknown(unknown) => unknown.id().clone(),
            Current::MissingIdentifier(state) => state.state().id.clone(),
        };
        self.by_id.insert(id, orphan);
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ResourceId, &Orphan)> {
        self.by_id.iter()
    }

    /// Cross the final planning boundary after refresh classification has been
    /// consumed. Missing-identifier rows remain present and invalid in the
    /// returned provider state; they are not converted to `not_found`.
    pub fn into_plan_orphans(self) -> carina_core::differ::Orphans {
        self.by_id.into_values().collect()
    }
}

impl IntoIterator for LoadedOrphans {
    type Item = (ResourceId, Orphan);
    type IntoIter = std::collections::hash_map::IntoIter<ResourceId, Orphan>;

    fn into_iter(self) -> Self::IntoIter {
        self.by_id.into_iter()
    }
}

impl FromIterator<Orphan> for LoadedOrphans {
    fn from_iter<T: IntoIterator<Item = Orphan>>(iter: T) -> Self {
        let mut loaded = Self::default();
        for orphan in iter {
            loaded.insert(orphan);
        }
        loaded
    }
}

#[derive(Deserialize)]
struct UncheckedStateFile {
    version: u32,
    exports_version: Option<u32>,
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
    ExportsVersionTooNew { found: u32, supported: u32 },
    InvalidResourceIdentity(InvalidResourceIdentityError),
    Validation(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportsVersionClassification {
    Current,
    Legacy,
}

fn classify_exports_version(
    state_version: u32,
    exports_version: Option<u32>,
) -> Result<ExportsVersionClassification, StateFileDecodeError> {
    match exports_version {
        Some(_) if state_version < StateFile::EXPORTS_VERSION_INTRODUCED => {
            Err(StateFileDecodeError::Validation(format!(
                "top-level exports_version is invalid before state version {}",
                StateFile::EXPORTS_VERSION_INTRODUCED
            )))
        }
        Some(found) if found > PublishedExports::CURRENT_VERSION => {
            Err(StateFileDecodeError::ExportsVersionTooNew {
                found,
                supported: PublishedExports::CURRENT_VERSION,
            })
        }
        Some(found) if found < PublishedExports::CURRENT_VERSION => {
            Err(StateFileDecodeError::Validation(format!(
                "exports version {found} is older than supported version {}; no exports migration exists",
                PublishedExports::CURRENT_VERSION
            )))
        }
        Some(_) => Ok(ExportsVersionClassification::Current),
        None if state_version >= StateFile::EXPORTS_VERSION_INTRODUCED => {
            Err(StateFileDecodeError::Validation(
                "missing required top-level exports_version".to_string(),
            ))
        }
        None => Ok(ExportsVersionClassification::Legacy),
    }
}

fn decode_persisted_exports(
    exports: HashMap<String, serde_json::Value>,
) -> Result<HashMap<String, PersistedExportValue>, StateFileDecodeError> {
    exports
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
        .collect()
}

fn map_state_file_decode_error(
    error: StateFileDecodeError,
    migration_from: Option<u32>,
) -> BackendError {
    match error {
        StateFileDecodeError::ExportsVersionTooNew { found, supported } => {
            BackendError::ExportsVersionTooNew { found, supported }
        }
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
        classify_exports_version(unchecked.version, unchecked.exports_version)?;
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
                        .get("current")
                        .and_then(|current| current.get("identifier"))
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
        let exports = decode_persisted_exports(unchecked.exports)?;
        let state = Self {
            version: unchecked.version,
            exports_version_marker: CurrentExportsVersion,
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
    ///     root `Struct`; a later applied/imported desired write rebuilds
    ///     a full tree from the resource's authored `Value`.
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
    /// v12: Persisted secret strings carry their own salt. Raw v11 legacy
    ///      strings are upgraded using their resource-row identity.
    /// v13: Persisted canonical enums use the host-reserved `$enum` tag and
    ///      user map keys beginning with `$` are escaped with one extra `$`.
    /// v14: Redacted secrets use the host-reserved `$secret` tag. Legacy
    ///      marker strings are migrated only in resource attribute trees.
    /// v15: Current instance is a tagged union. An applied identifier and a
    ///      pending create continuation cannot coexist in one row.
    /// v16: Added `ExplicitFields::WholeValue` with validated authored-path
    ///      evidence for carried ambiguous-list removals. Older rows need no
    ///      structural rewrite because they cannot contain the new marker.
    /// v17: Added top-level `exports_version` so published exports have a
    ///      compatibility contract independent of managed resource rows. See
    ///      [`Self::EXPORTS_VERSION_INTRODUCED`].
    /// v18: Added per-instance nested write-only leaf provenance. The v17
    ///      migration initializes the record to empty.
    /// v19: Replaced the partial-read marker's untyped detail with a typed
    ///      pending re-read reason. The v18 migration classifies legacy
    ///      details as incomplete reads.
    /// v20: Added a token-less create-outcome-unknown current instance.
    pub const CURRENT_VERSION: u32 = 20;

    /// State file version that introduced the independent exports version.
    /// This value is fixed forever.
    pub const EXPORTS_VERSION_INTRODUCED: u32 = 17;

    /// Create a new empty state file
    pub fn new() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            exports_version_marker: CurrentExportsVersion,
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
    /// use carina_core::provider::AppliedIdentifier;
    ///
    /// let state = StateFile::with_managed_state_bucket(
    ///     "aws",
    ///     "s3.Bucket",
    ///     "aws_s3_bucket_a3f2b1c8",
    ///     AppliedIdentifier::new("my-state-bucket").unwrap(),
    /// ).unwrap();
    /// assert_eq!(state.resources().len(), 1);
    /// assert_eq!(state.resources()[0].identity.as_str(), "aws_s3_bucket_a3f2b1c8");
    /// ```
    pub fn with_managed_state_bucket(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        resource_identity: impl Into<ResourceIdentity>,
        bucket_name: AppliedIdentifier,
    ) -> Result<Self, ResourceStateEncodingError> {
        let mut state = Self::new();
        state.resources.push(ResourceState::managed_state_bucket(
            provider,
            resource_type,
            resource_identity,
            bucket_name,
        )?);
        Ok(state)
    }

    /// Create a new state file with a specific lineage (for initialization)
    pub fn with_lineage(lineage: String) -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            exports_version_marker: CurrentExportsVersion,
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

    /// Find the persisted row identified by a routing-agnostic state-block
    /// address. Matching delegates to the same typed rule used by planning.
    pub fn find_resource_by_state_block_address(
        &self,
        address: &StateBlockAddress,
    ) -> Option<&ResourceState> {
        self.resources.iter().find(|row| {
            let resource_id = Self::id_for_resource_state(row);
            address.matches_resource_id(&resource_id)
        })
    }

    /// Classify an import target using its complete provider route.
    pub fn import_target_disposition_for_id(&self, id: &ResourceId) -> ImportTargetDisposition {
        let Some(row) = self.find_resource_for_id(id) else {
            return ImportTargetDisposition::Vacant;
        };
        match row.current() {
            CurrentInstance::CreateOutcomeUnknown { .. } => {
                ImportTargetDisposition::ReplaceCreateOutcomeUnknown
            }
            CurrentInstance::None
            | CurrentInstance::Applied { .. }
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::MissingIdentifier { .. } => ImportTargetDisposition::Occupied,
        }
    }

    /// Borrow the state file through a proof that `address` currently names
    /// an outcome-unknown create. No other lifecycle can mint this handle.
    ///
    /// State loading and mutation enforce one row per
    /// `(provider, resource_type, identity)` tuple. Provider routing is row
    /// metadata rather than another persisted identity axis, so this
    /// routing-agnostic address can match at most one row.
    pub fn find_outcome_unknown(
        &mut self,
        address: &StateBlockAddress,
    ) -> Result<OutcomeUnknownRow<'_>, NotOutcomeUnknown> {
        let Some(index) = self.resources.iter().position(|row| {
            let resource_id = Self::id_for_resource_state(row);
            address.matches_resource_id(&resource_id)
        }) else {
            return Err(NotOutcomeUnknown::Missing {
                address: address.clone(),
            });
        };
        if matches!(
            self.resources[index].current(),
            CurrentInstance::CreateOutcomeUnknown { .. }
        ) {
            Ok(OutcomeUnknownRow { state: self, index })
        } else {
            Err(NotOutcomeUnknown::DifferentLifecycle {
                address: address.clone(),
            })
        }
    }

    /// Consume the only typed authorization for forgetting an unknown create.
    /// Deposed generations remain tracked through `remove_resource`.
    pub fn forget(row: OutcomeUnknownRow<'_>) -> ResourceState {
        let OutcomeUnknownRow { state, index } = row;
        state.remove_resource_at(index)
    }

    /// Find the persisted row addressed by a full resource ID, including its
    /// provider-instance routing dimension.
    pub fn find_resource_for_id(&self, id: &ResourceId) -> Option<&ResourceState> {
        let identity = id.identity_str()?;
        self.resources.iter().find(|row| {
            row.provider == id.provider
                && row.resource_type == id.resource_type
                && row.identity.as_str() == identity
                && row.directives.provider_instance == id.provider_instance
        })
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

    /// Remove one deposed generation after that exact generation's Delete
    /// succeeded. If the row was an exact deposed-only shell and this removes
    /// its last generation, the now-empty shell is dropped. A malformed
    /// current instance without an identifier is retained for typed error
    /// classification.
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
        let was_deposed_only_shell = self.resources[row_pos].is_deposed_only_shell();
        let removed = self.resources[row_pos].deposed.remove(deposed_pos);
        if was_deposed_only_shell && self.resources[row_pos].deposed.is_empty() {
            self.resources.remove(row_pos);
        }
        Some(removed)
    }

    /// Build a map of ResourceId -> Directives from this state file.
    pub fn build_directives(&self) -> HashMap<ResourceId, Directives> {
        let mut directives_map = HashMap::new();
        for rs in &self.resources {
            let id = Self::id_for_resource_state(rs);
            directives_map.insert(id, Directives::from(&rs.directives));
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
        let mut attributes = HashMap::new();
        let mut nested_write_only = HashMap::new();
        for rs in &self.resources {
            let id = Self::id_for_resource_state(rs);
            if let Some(persisted) = rs.persisted_attributes() {
                attributes.insert(id.clone(), persisted.clone());
                nested_write_only.insert(id, rs.nested_write_only().clone());
            }
        }
        RawSavedAttrs::from_persisted(attributes, nested_write_only)
    }

    /// Remove historical rows that correspond to typed `read` declarations.
    ///
    /// Releases before #3181 could persist data-source results in the managed
    /// resource table. Match both the complete typed address supplied by the
    /// parser and the legacy row shape. An identifier-bearing row is managed
    /// state even when a new data source now occupies the same address.
    pub fn remove_legacy_data_source_rows<S>(&mut self, data_sources: &[DataSource<S>]) -> usize {
        let data_source_ids: HashSet<_> = data_sources.iter().map(|source| &source.id).collect();
        let original_len = self.resources.len();
        self.resources.retain(|row| {
            matches!(
                row.current(),
                CurrentInstance::Applied { .. }
                    | CurrentInstance::CreateInFlight { .. }
                    | CurrentInstance::CreateOutcomeUnknown { .. }
            ) || !data_source_ids.contains(&Self::id_for_resource_state(row))
        });
        original_len - self.resources.len()
    }

    /// Restore partial-read markers from the state file onto refreshed states.
    pub fn restore_partial_read_markers(&self, states: &mut StateTransformMap) {
        self.restore_partial_read_markers_in(states.iter_mut());
    }

    fn restore_partial_read_markers_in<'a>(
        &self,
        states: impl Iterator<Item = (&'a ResourceId, &'a mut State)>,
    ) {
        for (id, current) in states {
            if !current.exists {
                current.partial_read = None;
                continue;
            }
            let marker = id
                .identity_str()
                .and_then(|identity| self.find_resource(&id.provider, &id.resource_type, identity))
                .and_then(ResourceState::partial_read)
                .cloned();
            if let Some(marker) = marker {
                let (pending, mut missing_attributes) = marker.into_parts();
                missing_attributes.retain(|attr| !current.attributes.contains_key(attr));
                current.partial_read = PartialReadMarker::new(pending, missing_attributes);
            }
        }
    }

    /// Fill provider-omitted write-only values from the persisted current row.
    ///
    /// Every schema write-only key present in the row's persisted attributes
    /// is eligible; legacy metadata is not required. A value returned by the
    /// provider wins: persisted state is the authoritative last-sent fallback,
    /// not a replacement for a newer provider observation.
    /// This is called at the single managed provider-read boundary shared by
    /// planning and saved-plan drift checking.
    pub fn restore_recorded_write_only_attributes_in_state(
        &self,
        current: &mut State,
        schemas: &SchemaRegistry,
    ) {
        if !current.exists {
            return;
        }
        let id = &current.id;
        let Some(identity) = id.identity_str() else {
            return;
        };
        let Some(row) = self.find_resource(&id.provider, &id.resource_type, identity) else {
            return;
        };
        let Some(attributes) = row.persisted_attributes() else {
            return;
        };
        let Some(schema) = schemas.get(&id.provider, &id.resource_type, SchemaKind::Resource)
        else {
            return;
        };
        for key in recorded_write_only_keys(Some(schema.as_schema()), attributes, &[]) {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                current.attributes.entry(key.clone())
                && let Some(value) = attributes.get(&key)
            {
                entry.insert(value);
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

    /// Load the current-instance state for a managed resource.
    ///
    /// Missing and empty identifiers in [`CurrentInstance::MissingIdentifier`]
    /// are returned as the typed [`Current::MissingIdentifier`] condition.
    /// [`CurrentInstance::None`] always means that the current instance is
    /// absent; address metadata and deposed generations do not reclassify it.
    /// Data sources never enter `StateFile::resources`; their provider results
    /// are transient and therefore are not a second identifier-less row kind.
    pub fn build_state_for_resource(&self, id: &ResourceId) -> Current {
        let rs = id
            .identity_str()
            .and_then(|identity| self.find_resource(&id.provider, &id.resource_type, identity));
        let Some(rs) = rs else {
            return Current::absent(id.clone());
        };
        match rs.current() {
            CurrentInstance::None => Current::absent(id.clone()),
            CurrentInstance::Applied {
                identifier,
                attributes,
                partial_read,
                ..
            } => Current::from_state(State {
                id: id.clone(),
                identifier: Some(identifier.as_str().to_string()),
                attributes: attributes.to_values(),
                exists: true,
                dependency_bindings: rs.dependency_bindings.clone(),
                partial_read: partial_read.clone(),
            }),
            CurrentInstance::CreateInFlight { pending } => {
                Current::create_in_flight(pending.clone())
            }
            CurrentInstance::CreateOutcomeUnknown { unknown } => {
                Current::create_outcome_unknown(unknown.clone())
            }
            CurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes: _write_only_attributes,
                nested_write_only: _nested_write_only,
                partial_read,
            } => Current::from_state(State {
                id: id.clone(),
                identifier: None,
                attributes: attributes.to_values(),
                exists: true,
                dependency_bindings: rs.dependency_bindings.clone(),
                partial_read: partial_read.clone(),
            }),
        }
    }

    /// Build each persisted orphan as one typed value containing its
    /// provider-facing state and host-owned binding/directive metadata.
    pub fn build_orphans(
        &self,
        desired_ids: &std::collections::HashSet<ResourceId>,
    ) -> LoadedOrphans {
        let mut loaded = LoadedOrphans::default();
        for rs in &self.resources {
            let id = Self::id_for_resource_state(rs);
            if desired_ids.contains(&id) {
                continue;
            }
            let orphan = match self.build_state_for_resource(&id) {
                Current::Absent(_state) => continue,
                Current::Present(present) => {
                    Orphan::present(present, rs.binding.clone(), (&rs.directives).into())
                }
                Current::CreateInFlight(pending) => {
                    Orphan::create_in_flight(pending, rs.binding.clone(), (&rs.directives).into())
                }
                Current::CreateOutcomeUnknown(unknown) => Orphan::create_outcome_unknown(
                    unknown,
                    rs.binding.clone(),
                    (&rs.directives).into(),
                ),
                Current::MissingIdentifier(state) => {
                    Orphan::missing_identifier(state, rs.binding.clone(), (&rs.directives).into())
                }
            };
            loaded.insert(orphan);
        }
        loaded
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
            if let Some(binding) = r.binding.take() {
                r.binding = Some(canonicalize_persisted_absolute_binding_address(&binding));
            }
            r.dependency_bindings = r
                .dependency_bindings
                .iter()
                .map(canonicalize_persisted_absolute_binding_address)
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
        let pos = self.resources.iter().position(|r| {
            r.provider == provider
                && r.resource_type == resource_type
                && r.identity.as_str() == identity
        })?;
        Some(self.remove_resource_at(pos))
    }

    fn remove_resource_at(&mut self, pos: usize) -> ResourceState {
        if self.resources[pos].deposed.is_empty() {
            self.resources.remove(pos)
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
            removed
        }
    }

    /// Remove the entire persisted row, including deposed generations.
    ///
    /// This is reserved for address moves after the complete row has already
    /// been written at its destination. Ordinary current-instance cleanup must
    /// use [`Self::remove_resource`] so historical generations stay tracked.
    pub fn remove_resource_row(
        &mut self,
        provider: &str,
        resource_type: &str,
        identity: &str,
    ) -> Option<ResourceState> {
        let pos = self.resources.iter().position(|resource| {
            resource.provider == provider
                && resource.resource_type == resource_type
                && resource.identity.as_str() == identity
        })?;
        Some(self.resources.remove(pos))
    }

    /// Complete one validated address move after all writes and successful
    /// deposed-generation Deletes for the apply have been recorded.
    ///
    /// The destination's current instance and address metadata win. Every
    /// still-live deposed generation remaining at the source is merged into
    /// that destination before the source row is removed. If no destination
    /// row was written (for example, a resumed create ended), a deposed-only
    /// destination shell is derived from the source row. Accepting only
    /// [`MoveAuthority`] keeps this destructive whole-row cleanup coupled to
    /// a validated move edge.
    pub fn complete_resource_move(
        &mut self,
        authority: MoveAuthority<'_>,
    ) -> Result<Option<ResourceState>, BackendError> {
        let from = authority.from();
        let to = authority.to();
        let Some(from_identity) = from.identity() else {
            return Ok(None);
        };
        let Some(to_identity) = to.identity() else {
            return Ok(None);
        };
        let Some(source) = self
            .find_resource(&from.provider, &from.resource_type, from_identity.as_str())
            .cloned()
        else {
            return Ok(None);
        };

        // Build the complete result on a clone so a conflicting historical
        // generation cannot leave the caller with a half-applied move.
        let mut next = self.clone();
        next.remove_resource_row(&from.provider, &from.resource_type, from_identity.as_str());

        if !source.deposed.is_empty() {
            let mut destination = next
                .find_resource(&to.provider, &to.resource_type, to_identity.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    let mut shell = source.clone();
                    shell.provider = to.provider.clone();
                    shell.resource_type = to.resource_type.clone();
                    shell.identity = to_identity.clone();
                    shell.directives.provider_instance = to.provider_instance.clone();
                    shell.current = CurrentInstance::None;
                    shell.deposed.clear();
                    shell
                });
            merge_deposed_generations(&mut destination, source.deposed.clone())
                .map_err(BackendError::InvalidState)?;
            remove_current_identifier_from_deposed(&mut destination);
            next.upsert_resource(destination)?;
        }

        *self = next;
        Ok(Some(source))
    }

    /// Apply a keyed partial-update release to one persisted row.
    ///
    /// The effect capability can only be minted from
    /// `Effect::ReleaseListKeys`, and `key_field` can only be obtained from a
    /// validated schema declaration. This API mutates authoring metadata only;
    /// it never changes the provider-observed attribute value.
    pub fn release_list_keys(
        &mut self,
        release: ReleaseListKeysEffect<'_>,
        key_field: &FieldName,
    ) -> Result<(), BackendError> {
        let id = release.id();
        let Some(position) = self.resources.iter().position(|row| {
            row.provider == id.provider
                && row.resource_type == id.resource_type
                && row.identity.as_str() == id.identity_str()
                && row.directives.provider_instance == id.provider_instance
        }) else {
            return Err(BackendError::InvalidState(format!(
                "cannot release keyed list entries for {id}: resource row is missing"
            )));
        };

        let attribute = release.attribute();
        let stored = self.resources[position]
            .persisted_attributes()
            .and_then(|attributes| attributes.get(attribute))
            .ok_or_else(|| {
                BackendError::InvalidState(format!(
                    "cannot release keyed list entries for {id}.{attribute}: stored list is missing"
                ))
            })?;
        let Value::Concrete(ConcreteValue::List(stored_elements)) = stored else {
            return Err(BackendError::InvalidState(format!(
                "cannot release keyed list entries for {id}.{attribute}: stored value is not a list"
            )));
        };

        let ExplicitFields::Struct { children } = &mut self.resources[position].explicit else {
            return Err(BackendError::InvalidState(format!(
                "cannot release keyed list entries for {id}.{attribute}: root authoring record is not a struct"
            )));
        };
        let authoring = children.get_mut(attribute).ok_or_else(|| {
            BackendError::InvalidState(format!(
                "cannot release keyed list entries for {id}.{attribute}: authoring record is missing"
            ))
        })?;

        if !matches!(authoring, ExplicitFields::ListElements { .. }) {
            let element_authoring = match authoring.clone() {
                ExplicitFields::List { element } => *element,
                ExplicitFields::Unrecorded => ExplicitFields::Unrecorded,
                ExplicitFields::Leaf
                | ExplicitFields::Struct { .. }
                | ExplicitFields::WholeValue { .. } => authoring.clone(),
                ExplicitFields::ListElements { .. } => unreachable!("matched above"),
            };
            *authoring = ExplicitFields::ListElements {
                elements: vec![element_authoring; stored_elements.len()],
            };
        }
        let ExplicitFields::ListElements { elements } = authoring else {
            unreachable!("normalized to ListElements")
        };
        if elements.len() != stored_elements.len() {
            return Err(BackendError::InvalidState(format!(
                "cannot release keyed list entries for {id}.{attribute}: authoring record has {} elements but stored list has {}",
                elements.len(),
                stored_elements.len(),
            )));
        }

        let released = release
            .keys()
            .iter()
            .map(|key| key.key())
            .collect::<HashSet<_>>();
        for (stored, explicit) in stored_elements.iter().zip(elements.iter_mut()) {
            let Value::Concrete(ConcreteValue::Map(fields)) = stored else {
                continue;
            };
            let Some(Value::Concrete(ConcreteValue::String(key))) = fields.get(key_field.as_str())
            else {
                continue;
            };
            if released.contains(key.expose_plaintext()) {
                *explicit = ExplicitFields::Unrecorded;
            }
        }
        Ok(())
    }

    /// Clear only the current instance after a provider verdict says a
    /// recorded create ended without a resource.
    ///
    /// If deposed generations remain, retain every address-level field
    /// byte-for-byte. Those generations still belong to this address, and a
    /// later writeback must not lose routing, dependency, or name-override
    /// metadata merely because the replacement create ended. With no
    /// deposed generations there is no remaining row to retain.
    pub fn clear_current_instance_preserving_address_metadata(
        &mut self,
        provider: &str,
        resource_type: &str,
        identity: &str,
    ) -> Option<ResourceState> {
        let pos = self.resources.iter().position(|resource| {
            resource.provider == provider
                && resource.resource_type == resource_type
                && resource.identity.as_str() == identity
        })?;
        if self.resources[pos].deposed.is_empty() {
            return Some(self.resources.remove(pos));
        }

        let previous = self.resources[pos].clone();
        self.resources[pos].current = CurrentInstance::None;
        Some(previous)
    }
}

const _: () = assert!(StateFile::EXPORTS_VERSION_INTRODUCED <= StateFile::CURRENT_VERSION);

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
    if let Some(identifier) = resource
        .identifier()
        .map(AppliedIdentifier::as_str)
        .map(str::to_owned)
    {
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

/// Minimal struct for extracting the compatibility markers from a state file.
#[derive(Deserialize)]
struct VersionCheck {
    version: u32,
    exports_version: Option<u32>,
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
/// carina#3315. Read-only full-state call sites (`plan`, `state
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
    /// (`plan`, `state list/show/lookup`) where the on-disk
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
        log_state_migration(info, display_target);
    }
}

pub(crate) fn log_state_migration(info: MigrationInfo, display_target: &str) {
    eprintln!(
        "Warning: state file {} is v{} on disk; in-memory migration \
         to v{} applied for this run. Disk state will be rewritten \
         during the next apply or state refresh of that project.",
        display_target, info.from, info.to
    );
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
            classify_exports_version(v, check.exports_version)
                .map_err(|error| map_state_file_decode_error(error, None))?;
            migration = Some(MigrationInfo {
                from: v,
                to: StateFile::CURRENT_VERSION,
            });
            // v12 → v13: migrate every value tree before typed state
            // deserialization validates the reserved `$` namespace.
            let migrated_v12 = if v <= 12 {
                Some(migrate_v12_persisted_values(content)?)
            } else {
                None
            };
            let pre_secret_migration = migrated_v12.as_deref().unwrap_or(content);
            let migrated_v13 = if v <= 13 {
                Some(migrate_v13_resource_secret_markers(pre_secret_migration)?)
            } else {
                None
            };
            let pre_current_migration = migrated_v13.as_deref().unwrap_or(pre_secret_migration);
            let migrated_v14 = if v <= 14 {
                Some(migrate_v14_current_instance(pre_current_migration)?)
            } else {
                None
            };
            let pre_nested_write_only_migration =
                migrated_v14.as_deref().unwrap_or(pre_current_migration);
            let migrated_v17 = if v <= 17 {
                Some(migrate_v17_nested_write_only(
                    pre_nested_write_only_migration,
                )?)
            } else {
                None
            };
            let pre_partial_read_migration = migrated_v17
                .as_deref()
                .unwrap_or(pre_nested_write_only_migration);
            let migrated_v18 = if v <= 18 {
                Some(migrate_v18_partial_read_reason(pre_partial_read_migration)?)
            } else {
                None
            };
            let pre_outcome_unknown_migration = migrated_v18
                .as_deref()
                .unwrap_or(pre_partial_read_migration);
            let migrated_v19 = if v <= 19 {
                Some(migrate_v19_create_outcome_unknown(
                    pre_outcome_unknown_migration,
                )?)
            } else {
                None
            };
            let migrated_content = migrated_v19
                .as_deref()
                .unwrap_or(pre_outcome_unknown_migration);
            let mut state = StateFile::from_json_str(migrated_content)
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
            // expansion path. Rewriting every top-level empty Struct
            // from v6 to `Unrecorded` preserves that legacy meaning.
            // Current-version desired-backed writes legitimately emit an
            // empty Struct when the user authored no attributes;
            // this migration is version-gated and never rewrites them.
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
            // v11 → v12 secret strings and v13 → v14 structural secret tags
            // were migrated in raw JSON before typed deserialization above.
            // v12 → v13 key/tag migration ran first so a newly inserted
            // `$secret` host tag could not be mistaken for a user key.
            state.version = StateFile::CURRENT_VERSION;
            state
        }
    };
    // Map-key addresses written under the legacy `["..."]` shape are
    // rewritten to the canonical form on read so existing state files
    // resolve cleanly against new emissions. See #1903.
    state.canonicalize_addresses()?;
    // `state.resources` is managed-only by invariant (since #3181). Do not
    // classify rows by identifier absence here: a malformed managed row must
    // survive to `build_state_for_resource` so planning fails closed instead
    // of recreating the live resource. Once the parsed configuration supplies
    // typed data-source addresses, `remove_legacy_data_source_rows` removes
    // matching pre-#3181 artifacts without conflating the two row kinds. The
    // one legitimate null-identifier shape, an empty deposed-only shell, is
    // classified explicitly by the resource loader.
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
/// use [`DeposedInstance::persisted_attributes`] so structural secret tags
/// retain their typed meaning.
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
    pub dependency_bindings: BTreeSet<AbsoluteBindingAddress>,
}

/// Attributes admitted to a deposed generation by a state-owned writeback
/// path.
///
/// The wrapper does not claim that its entire map was loaded from state. Its
/// narrow guarantee is that a key classified as write-only by the schema used
/// to construct it can enter only from a persisted current or deposed
/// generation. No-previous paths may retain arbitrary ordinary or
/// schema-unknown provider/history attributes after stripping known
/// write-only keys. The private inner map prevents callers from relabelling
/// arbitrary known write-only values as carried state.
///
/// ```compile_fail
/// use carina_core::provider::PersistedAttributes;
/// use carina_state::DeposedAttributes;
///
/// let arbitrary = PersistedAttributes::default();
/// let _previous = DeposedAttributes(arbitrary);
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeposedAttributes(PersistedAttributes);

impl DeposedAttributes {
    pub fn as_persisted(&self) -> &PersistedAttributes {
        &self.0
    }
}

impl DeposedInstance {
    /// Construct a deposed generation from the opaque output of the state
    /// writeback encoder.
    pub fn from_persisted_attributes(
        key: DeposedKey,
        identifier: String,
        provider_instance: Option<String>,
        attributes: DeposedAttributes,
        dependency_bindings: BTreeSet<AbsoluteBindingAddress>,
    ) -> Self {
        Self {
            key,
            identifier,
            provider_instance,
            attributes: attributes.0,
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
            DeposedAttributes(persisted_attributes_from_test_json(attributes)),
            dependency_bindings
                .into_iter()
                .map(absolute_binding_address_for_test)
                .collect(),
        )
    }

    /// Borrow this generation's attributes through the persisted decode seam.
    pub fn persisted_attributes(&self) -> &PersistedAttributes {
        &self.attributes
    }

    /// Rebuild this generation from a provider read while carrying write-only
    /// values only from this persisted generation. Provider echoes never
    /// become provenance for a deposed row.
    pub fn refreshed_attributes_for_resource_and_schema<S>(
        &self,
        resource: &ResolvedResource<S>,
        schema: Option<&ResourceSchema>,
        fresh_attributes: &HashMap<String, Value>,
    ) -> DeposedAttributes {
        let carried_write_only = recorded_write_only_keys(schema, &self.attributes, &[]);
        let mut selected =
            SelectedStateAttributes::from_provider_values_lossy(fresh_attributes, schema, &[]);
        selected.carry_write_only(&self.attributes, carried_write_only);
        let selected = selected
            .protect_for_resource(
                resource,
                PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&self.attributes),
                None,
                false,
            )
            .expect("lossy secret protection does not return serialization errors");
        let (selected, _write_only_attributes) = selected
            .finish()
            .expect("selected persisted values are already validated");
        DeposedAttributes(selected)
    }

    pub fn with_refreshed_attributes(mut self, attributes: DeposedAttributes) -> Self {
        self.attributes = attributes.0;
        self
    }
}

/// The mutually exclusive state of a row's current resource instance.
///
/// The tagged shape is the persistence invariant for state v15: an applied
/// identifier and provider continuation token cannot be represented together.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CurrentInstance {
    /// The row retains only deposed generations.
    None,
    /// A completed resource instance.
    Applied {
        identifier: AppliedIdentifier,
        attributes: PersistedAttributes,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        write_only_attributes: Vec<String>,
        nested_write_only: NestedWriteOnlyRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_read: Option<PartialReadMarker>,
    },
    /// A create operation accepted by the provider but not yet ended.
    CreateInFlight {
        #[serde(flatten)]
        pending: InFlightCreate,
    },
    /// A create may have reached the cloud, but yielded neither an identifier
    /// nor a resumable continuation.
    CreateOutcomeUnknown {
        #[serde(flatten)]
        unknown: UnknownCreateOutcome,
    },
    /// A legacy current instance whose provider identifier is absent.
    MissingIdentifier {
        attributes: PersistedAttributes,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        write_only_attributes: Vec<String>,
        nested_write_only: NestedWriteOnlyRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_read: Option<PartialReadMarker>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EncodedCurrentInstance {
    None,
    Applied {
        identifier: AppliedIdentifier,
        attributes: HashMap<String, serde_json::Value>,
        #[serde(default)]
        write_only_attributes: Vec<String>,
        nested_write_only: NestedWriteOnlyRecord,
        #[serde(default)]
        partial_read: Option<PartialReadMarker>,
    },
    CreateInFlight {
        envelope: serde_json::Value,
        started_at: serde_json::Value,
    },
    CreateOutcomeUnknown {
        started_at: serde_json::Value,
    },
    MissingIdentifier {
        attributes: HashMap<String, serde_json::Value>,
        #[serde(default)]
        write_only_attributes: Vec<String>,
        nested_write_only: NestedWriteOnlyRecord,
        #[serde(default)]
        partial_read: Option<PartialReadMarker>,
    },
}

/// State of a single managed resource.
///
/// Persisted attributes cannot be read as ordinary JSON values. Callers must
/// use [`ResourceState::persisted_attributes`] so structural secret tags retain
/// their typed meaning.
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
    /// The current resource instance, if any.
    current: CurrentInstance,
    /// Old replacement generations that still need a successful Delete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deposed: Vec<DeposedInstance>,
    /// Whether this resource is protected from deletion (e.g., state bucket)
    #[serde(default)]
    pub protected: bool,
    /// Carina-side directives persisted from the DSL `directives` block.
    #[serde(default)]
    pub directives: PersistedDirectives,
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
    /// the root `Struct`. Later writes rebuild a full tree from the resource's
    /// authored `Value` via an evidence-specific aligned builder.
    #[serde(default, skip_serializing_if = "is_empty_explicit")]
    pub explicit: ExplicitFields,
    /// The binding name for this resource (from `let` bindings in DSL).
    /// Stored so orphan Delete effects can have tree structure.
    #[serde(default)]
    pub binding: Option<AbsoluteBindingAddress>,
    /// Binding names of resources this resource depends on (via ResourceRef).
    /// Stored so orphan Delete effects can have tree structure.
    ///
    /// Set semantics (BTreeSet) — see Resource::dependency_bindings (#2228).
    /// Old state files persisted as JSON arrays continue to deserialize
    /// (serde transparently coerces array → BTreeSet, deduping any
    /// duplicates and re-serializing in sorted order on next write).
    #[serde(default)]
    pub dependency_bindings: BTreeSet<AbsoluteBindingAddress>,
}

/// Owned resource row proven not to contain a create continuation.
#[cfg(any(test, feature = "test-utils"))]
#[derive(Debug)]
pub struct WritableResourceState(ResourceState);

/// An attempted writable projection that retained a protected non-writable
/// lifecycle row.
#[cfg(any(test, feature = "test-utils"))]
#[derive(Debug)]
pub enum NonWritableResourceState {
    CreateInFlight(Box<ResourceState>),
    CreateOutcomeUnknown(Box<ResourceState>),
}

#[cfg(any(test, feature = "test-utils"))]
impl NonWritableResourceState {
    pub fn into_inner(self) -> ResourceState {
        match self {
            Self::CreateInFlight(row) | Self::CreateOutcomeUnknown(row) => *row,
        }
    }
}

/// Borrowed resource row proven not to contain a create continuation.
pub struct RefreshableResourceState<'a>(&'a ResourceState);

#[derive(Deserialize)]
struct EncodedDeposedInstance {
    key: DeposedKey,
    identifier: String,
    #[serde(default)]
    provider_instance: Option<String>,
    attributes: HashMap<String, serde_json::Value>,
    dependency_bindings: BTreeSet<AbsoluteBindingAddress>,
}

#[derive(Deserialize)]
struct EncodedResourceState {
    resource_type: String,
    #[serde(alias = "name")]
    identity: ResourceIdentity,
    provider: String,
    current: EncodedCurrentInstance,
    #[serde(default)]
    deposed: Vec<EncodedDeposedInstance>,
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    directives: PersistedDirectives,
    #[serde(default)]
    prefixes: HashMap<String, String>,
    #[serde(default)]
    name_overrides: HashMap<String, NameOverride>,
    #[serde(default)]
    explicit: ExplicitFields,
    #[serde(default)]
    binding: Option<AbsoluteBindingAddress>,
    #[serde(default)]
    dependency_bindings: BTreeSet<AbsoluteBindingAddress>,
}

impl<'de> Deserialize<'de> for ResourceState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let row = serde_json::Value::deserialize(deserializer)?;
        let encoded = serde_json::from_value::<EncodedResourceState>(row.clone())
            .map_err(serde::de::Error::custom)?;
        let current = match encoded.current {
            EncodedCurrentInstance::None => CurrentInstance::None,
            EncodedCurrentInstance::Applied {
                identifier,
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => CurrentInstance::Applied {
                identifier,
                attributes: PersistedAttributes::from_encoded(attributes)
                    .map_err(|error| serde::de::Error::custom(ErrorSourceChain(&error)))?,
                write_only_attributes,
                nested_write_only,
                partial_read,
            },
            EncodedCurrentInstance::CreateInFlight {
                envelope: _envelope,
                started_at: _started_at,
            } => {
                let pending = serde_json::from_value::<StateRowInFlightCreate>(row)
                    .map_err(serde::de::Error::custom)?
                    .into_inner();
                CurrentInstance::CreateInFlight { pending }
            }
            EncodedCurrentInstance::CreateOutcomeUnknown {
                started_at: _started_at,
            } => {
                let unknown = serde_json::from_value::<StateRowUnknownCreateOutcome>(row)
                    .map_err(serde::de::Error::custom)?
                    .into_inner();
                CurrentInstance::CreateOutcomeUnknown { unknown }
            }
            EncodedCurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read,
            } => CurrentInstance::MissingIdentifier {
                attributes: PersistedAttributes::from_encoded(attributes)
                    .map_err(|error| serde::de::Error::custom(ErrorSourceChain(&error)))?,
                write_only_attributes,
                nested_write_only,
                partial_read,
            },
        };
        let deposed = encoded
            .deposed
            .into_iter()
            .map(|instance| {
                Ok(DeposedInstance {
                    key: instance.key,
                    identifier: instance.identifier,
                    provider_instance: instance.provider_instance,
                    attributes: PersistedAttributes::from_encoded(instance.attributes)
                        .map_err(|error| serde::de::Error::custom(ErrorSourceChain(&error)))?,
                    dependency_bindings: instance.dependency_bindings,
                })
            })
            .collect::<Result<Vec<_>, D::Error>>()?;
        Ok(Self {
            resource_type: encoded.resource_type,
            identity: encoded.identity,
            provider: encoded.provider,
            current,
            deposed,
            protected: encoded.protected,
            directives: encoded.directives,
            prefixes: encoded.prefixes,
            name_overrides: encoded.name_overrides,
            explicit: encoded.explicit,
            binding: encoded.binding,
            dependency_bindings: encoded.dependency_bindings,
        })
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl WritableResourceState {
    #[doc(hidden)]
    pub fn with_attributes_from_state(
        mut self,
        state: &State,
    ) -> Result<ResourceState, ResourceStateEncodingError> {
        let previous = self.0.persisted_attributes().cloned().unwrap_or_default();
        let encoded = state
            .attributes
            .iter()
            .filter_map(|(key, value)| {
                PersistedValue::encode_provider_value(value, previous.encoded_value(key))
                    .ok()
                    .map(|value| (key.clone(), value))
            })
            .collect();
        let attributes = PersistedAttributes::from_persisted_values(encoded)?;
        self.0.current = match state
            .identifier
            .as_deref()
            .and_then(|identifier| AppliedIdentifier::new(identifier).ok())
        {
            Some(identifier) => CurrentInstance::Applied {
                identifier,
                attributes,
                write_only_attributes: Vec::new(),
                nested_write_only: NestedWriteOnlyRecord::default(),
                partial_read: state.partial_read.clone(),
            },
            None => CurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes: Vec::new(),
                nested_write_only: NestedWriteOnlyRecord::default(),
                partial_read: state.partial_read.clone(),
            },
        };
        Ok(self.0)
    }
}

impl RefreshableResourceState<'_> {
    /// Replace the provider-observed portion of an orphan row while retaining
    /// every host-owned field from the existing state entry.
    pub fn refreshed_from_provider_state(
        self,
        state: &State,
        schema: Option<&ResourceSchema>,
    ) -> Result<ResourceState, ResourceStateEncodingError> {
        let source = self.0;
        let mut refreshed = source.clone();
        let previous = source.persisted_attributes().cloned().unwrap_or_default();
        let write_only_keys =
            recorded_write_only_keys(schema, &previous, source.write_only_attributes());
        let mut selected = SelectedStateAttributes::try_from_provider_values(
            &state.attributes,
            schema,
            source.write_only_attributes(),
            None,
            Some(&previous),
        )?;
        selected.carry_write_only(&previous, write_only_keys);
        let (attributes, write_only_attributes) = selected.protect_orphan(&previous).finish()?;
        refreshed.current = match state
            .identifier
            .as_deref()
            .and_then(|identifier| AppliedIdentifier::new(identifier).ok())
        {
            Some(identifier) => CurrentInstance::Applied {
                identifier,
                attributes,
                write_only_attributes,
                nested_write_only: source.nested_write_only().clone(),
                partial_read: state.partial_read.clone(),
            },
            None => CurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes,
                nested_write_only: source.nested_write_only().clone(),
                partial_read: state.partial_read.clone(),
            },
        };
        // An orphan has no desired resource from which to rebuild authoring
        // metadata. Preserve the prior record, conservatively discarding only
        // stored-index alignment that a provider refresh may have invalidated.
        refreshed.explicit = explicit::demote_list_elements_to_union(&source.explicit);
        Ok(refreshed)
    }
}

/// Rebuild the desired resource used by state encoding from the apply-time
/// resolved shape. That shape is authoritative for deferred authored nodes,
/// while explicit authored `Secret` markers at concrete positions are unioned
/// into it. If neither shape can be projected, only that unprojectable child
/// fails closed; its container and siblings retain their own shapes.
fn resource_with_secret_shape_union<S: Clone>(
    resource: &ResolvedResource<S>,
    applied: &SecretPreservingAttributes,
) -> ResolvedResource<S> {
    let mut resolved = resource.as_inner().clone();
    resolved.attributes = applied
        .values()
        .iter()
        .map(|(key, value)| {
            let protected = resource.attributes.get(key).map_or_else(
                || value.clone(),
                |authored_shape| union_secret_shapes_after_resolution(value, authored_shape),
            );
            (key.clone(), protected)
        })
        .collect();
    ResolvedResource::new(resolved)
}

impl ResourceState {
    /// Borrow the typed current-instance payload.
    pub fn current(&self) -> &CurrentInstance {
        &self.current
    }

    /// Borrow persisted resource attributes when this row has an attribute-
    /// bearing current instance. In-flight and deposed-only rows deliberately
    /// return `None` rather than presenting a continuation envelope as state.
    pub fn persisted_attributes(&self) -> Option<&PersistedAttributes> {
        match &self.current {
            CurrentInstance::Applied { attributes, .. }
            | CurrentInstance::MissingIdentifier { attributes, .. } => Some(attributes),
            CurrentInstance::None
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. } => None,
        }
    }

    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        match &self.current {
            CurrentInstance::Applied { identifier, .. } => Some(identifier),
            CurrentInstance::None
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. }
            | CurrentInstance::MissingIdentifier { .. } => None,
        }
    }

    pub fn write_only_attributes(&self) -> &[String] {
        match &self.current {
            CurrentInstance::Applied {
                write_only_attributes,
                ..
            }
            | CurrentInstance::MissingIdentifier {
                write_only_attributes,
                ..
            } => write_only_attributes,
            CurrentInstance::None
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. } => &[],
        }
    }

    /// Borrow accepted desired values for nested write-only leaves.
    pub fn nested_write_only(&self) -> &NestedWriteOnlyRecord {
        match &self.current {
            CurrentInstance::Applied {
                nested_write_only, ..
            }
            | CurrentInstance::MissingIdentifier {
                nested_write_only, ..
            } => nested_write_only,
            CurrentInstance::None
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. } => {
                static EMPTY: NestedWriteOnlyRecord = NestedWriteOnlyRecord::new();
                &EMPTY
            }
        }
    }

    pub fn partial_read(&self) -> Option<&PartialReadMarker> {
        match &self.current {
            CurrentInstance::Applied { partial_read, .. }
            | CurrentInstance::MissingIdentifier { partial_read, .. } => partial_read.as_ref(),
            CurrentInstance::None
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. } => None,
        }
    }

    /// Whether this row contains a current instance in addition to any
    /// historical deposed generations.
    ///
    /// Address-level metadata and deposed generations do not change the typed
    /// current-instance state.
    pub fn has_current_instance(&self) -> bool {
        !matches!(self.current, CurrentInstance::None)
    }

    /// Whether this row is the minimal shell produced solely to retain
    /// deposed generations. This controls removal of an empty shell only; it
    /// must not be used to classify [`CurrentInstance::None`] while loading.
    fn is_deposed_only_shell(&self) -> bool {
        let shell_directives = PersistedDirectives {
            provider_instance: self.directives.provider_instance.clone(),
            ..PersistedDirectives::default()
        };
        matches!(self.current, CurrentInstance::None)
            && !self.deposed.is_empty()
            && !self.protected
            && self.directives == shell_directives
            && self.prefixes.is_empty()
            && self.name_overrides.is_empty()
            && is_empty_explicit(&self.explicit)
            && self.binding.is_none()
            && self.dependency_bindings.is_empty()
    }

    /// Set one raw persisted attribute in tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn set_persisted_attribute(&mut self, key: impl Into<String>, value: serde_json::Value) {
        let key = key.into();
        let upgraded = persisted_attributes_from_test_json(HashMap::from([(key.clone(), value)]));
        match &mut self.current {
            CurrentInstance::Applied { attributes, .. }
            | CurrentInstance::MissingIdentifier { attributes, .. } => attributes.extend(upgraded),
            CurrentInstance::None => {
                self.current = CurrentInstance::MissingIdentifier {
                    attributes: upgraded,
                    write_only_attributes: Vec::new(),
                    nested_write_only: NestedWriteOnlyRecord::default(),
                    partial_read: None,
                };
            }
            CurrentInstance::CreateInFlight { .. } => {
                panic!("cannot add resource attributes to an in-flight row")
            }
            CurrentInstance::CreateOutcomeUnknown { .. } => {
                panic!("cannot add resource attributes to an outcome-unknown row")
            }
        }
    }

    /// Replace current-instance write-only metadata in fixtures.
    #[cfg(test)]
    pub(crate) fn set_write_only_attributes_for_test(&mut self, value: Vec<String>) {
        match &mut self.current {
            CurrentInstance::Applied {
                write_only_attributes,
                ..
            }
            | CurrentInstance::MissingIdentifier {
                write_only_attributes,
                ..
            } => *write_only_attributes = value,
            CurrentInstance::None
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. } => {
                panic!("test fixture has no attribute-bearing current instance")
            }
        }
    }

    /// Replace current-instance partial-read metadata in fixtures.
    #[cfg(test)]
    pub(crate) fn set_partial_read_for_test(&mut self, value: Option<PartialReadMarker>) {
        match &mut self.current {
            CurrentInstance::Applied { partial_read, .. }
            | CurrentInstance::MissingIdentifier { partial_read, .. } => *partial_read = value,
            CurrentInstance::None
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. } => {
                panic!("test fixture has no attribute-bearing current instance")
            }
        }
    }

    /// Render the sorted attribute map for `carina state show` without
    /// exposing the encoded values to value-layer callers.
    pub fn format_attributes_for_state_show(&self) -> String {
        let sorted: std::collections::BTreeMap<String, serde_json::Value> = self
            .persisted_attributes()
            .map(PersistedAttributes::to_decoded_json)
            .unwrap_or_default()
            .into_iter()
            .collect();
        serde_json::to_string_pretty(&sorted).expect("state JSON values are serializable")
    }

    /// Render one stored attribute for `carina state show`.
    pub fn format_attribute_for_state_show(&self, key: &str, json_output: bool) -> Option<String> {
        self.persisted_attributes()?
            .decoded_json_value(key)
            .map(|value| {
                if json_output {
                    serde_json::to_string_pretty(&value)
                        .expect("state JSON values are serializable")
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

        let current = match &self.current {
            CurrentInstance::None => serde_json::Value::Null,
            CurrentInstance::Applied {
                identifier,
                attributes,
                ..
            } => serde_json::json!({
                "status": "applied",
                "identifier": identifier.as_str(),
                "attributes": sorted_attributes(attributes),
            }),
            CurrentInstance::CreateInFlight { pending } => serde_json::json!({
                "status": "create_in_flight",
                "started_at": pending.started_at().as_str(),
            }),
            CurrentInstance::CreateOutcomeUnknown { unknown } => serde_json::json!({
                "status": "create_outcome_unknown",
                "started_at": unknown.started_at().as_str(),
            }),
            CurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes: _write_only_attributes,
                nested_write_only: _nested_write_only,
                partial_read: _partial_read,
            } => serde_json::json!({
                "status": "missing_identifier",
                "identifier": serde_json::Value::Null,
                "attributes": sorted_attributes(attributes),
            }),
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
            current: CurrentInstance::None,
            deposed: Vec::new(),
            protected: false,
            directives: PersistedDirectives::default(),
            prefixes: HashMap::new(),
            name_overrides: HashMap::new(),
            explicit: ExplicitFields::default(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
        }
    }

    /// Construct a completed current instance from a validated identifier.
    pub fn applied(
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
        provider: impl Into<String>,
        identifier: AppliedIdentifier,
    ) -> Self {
        let mut row = Self::new(resource_type, identity, provider);
        row.current = CurrentInstance::Applied {
            identifier,
            attributes: PersistedAttributes::default(),
            write_only_attributes: Vec::new(),
            nested_write_only: NestedWriteOnlyRecord::default(),
            partial_read: None,
        };
        row
    }

    /// Validate an identifier and construct a completed current instance.
    pub fn try_applied(
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
        provider: impl Into<String>,
        identifier: impl Into<String>,
    ) -> Result<Self, EmptyAppliedIdentifier> {
        Ok(Self::applied(
            resource_type,
            identity,
            provider,
            AppliedIdentifier::new(identifier)?,
        ))
    }

    /// Set the identifier (AWS internal ID like vpc-xxx).
    ///
    /// Applied rows are constructed directly; an arbitrary row cannot be
    /// converted to applied state and thereby overwrite an in-flight create.
    ///
    /// ```compile_fail
    /// use carina_state::ResourceState;
    ///
    /// fn overwrite(row: ResourceState) -> ResourceState {
    ///     row.with_identifier("provider-id")
    /// }
    /// ```
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn into_writable(self) -> Result<WritableResourceState, NonWritableResourceState> {
        match self.current {
            CurrentInstance::CreateInFlight { .. } => {
                Err(NonWritableResourceState::CreateInFlight(Box::new(self)))
            }
            CurrentInstance::CreateOutcomeUnknown { .. } => Err(
                NonWritableResourceState::CreateOutcomeUnknown(Box::new(self)),
            ),
            CurrentInstance::None
            | CurrentInstance::Applied { .. }
            | CurrentInstance::MissingIdentifier { .. } => Ok(WritableResourceState(self)),
        }
    }

    pub fn refreshable(&self) -> Option<RefreshableResourceState<'_>> {
        match self.current {
            CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. } => None,
            CurrentInstance::None
            | CurrentInstance::Applied { .. }
            | CurrentInstance::MissingIdentifier { .. } => Some(RefreshableResourceState(self)),
        }
    }

    /// Set a raw persisted attribute in tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn with_attribute(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.set_persisted_attribute(key, value);
        self
    }

    /// Populate attributes from a provider-returned State.
    ///
    /// ```compile_fail
    /// use carina_core::resource::State;
    /// use carina_state::ResourceState;
    ///
    /// fn overwrite(row: ResourceState, state: &State) {
    ///     let _ = row.with_attributes_from_state(state);
    /// }
    /// ```
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn from_provider_state(
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
        provider: impl Into<String>,
        state: &State,
    ) -> Result<Self, ResourceStateEncodingError> {
        WritableResourceState(Self::new(resource_type, identity, provider))
            .with_attributes_from_state(state)
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
    /// existing. A missing identifier is now surfaced as a typed load error;
    /// older clients treated it as "not found" and re-issued `CreateBucket`,
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
        bucket_name: AppliedIdentifier,
    ) -> Result<Self, ResourceStateEncodingError> {
        let attributes = PersistedAttributes::from_persisted_values(HashMap::from([(
            "bucket".to_string(),
            PersistedValue::encode_provider_value(
                &Value::Concrete(ConcreteValue::String(
                    bucket_name.as_str().to_string().into(),
                )),
                None,
            )?,
        )]))?;
        let mut state = Self::new(resource_type, resource_identity, provider);
        state.current = CurrentInstance::Applied {
            identifier: bucket_name,
            attributes,
            write_only_attributes: Vec::new(),
            nested_write_only: NestedWriteOnlyRecord::default(),
            partial_read: None,
        };
        state.protected = true;
        Ok(state)
    }

    /// Encode a provider/history fallback that has no persisted generation to
    /// carry from. All schema write-only values are excluded.
    pub fn deposed_attributes_without_previous<S>(
        resource: &ResolvedResource<S>,
        schema: Option<&ResourceSchema>,
        attributes: &HashMap<String, Value>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
    ) -> DeposedAttributes {
        let selected = SelectedStateAttributes::from_provider_values_lossy(attributes, schema, &[])
            .protect_for_resource(resource, previous_hash_authority, None, false)
            .expect("lossy secret protection does not return serialization errors");
        let (selected, _write_only_attributes) = selected
            .finish()
            .expect("selected persisted values are already validated");
        DeposedAttributes(selected)
    }

    /// Encode plan-time replacement history when no persisted generation
    /// exists. Provider-read write-only values are deliberately excluded.
    pub fn deposed_attributes_from_replacement_history<S>(
        resource: &ResolvedResource<S>,
        schema: Option<&ResourceSchema>,
        previous: ReplacementPreviousAttributesForStateWriteback<'_>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
    ) -> DeposedAttributes {
        let selected = SelectedStateAttributes::from_provider_values_lossy(
            previous.provider_attributes(),
            schema,
            &[],
        )
        .protect_for_resource(resource, previous_hash_authority, None, false)
        .expect("lossy secret protection does not return serialization errors");
        let (selected, _write_only_attributes) = selected
            .finish()
            .expect("selected persisted values are already validated");
        DeposedAttributes(selected)
    }

    /// Carry values from this persisted current generation into a deposed
    /// generation. The receiver is the type-level source of provenance.
    pub fn deposed_attributes_from_current_generation<S>(
        &self,
        resource: &ResolvedResource<S>,
        schema: Option<&ResourceSchema>,
    ) -> DeposedAttributes {
        let previous = self.persisted_attributes().cloned().unwrap_or_default();
        let selected = SelectedStateAttributes::from_persisted_generation(&previous, schema, &[])
            .protect_for_resource(
                resource,
                PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&previous),
                None,
                false,
            )
            .expect("lossy secret protection does not return serialization errors");
        let (selected, _write_only_attributes) = selected
            .finish()
            .expect("selected persisted values are already validated");
        DeposedAttributes(selected)
    }

    fn selected_sent_attributes<S>(
        resource: &ResolvedResource<S>,
        sent: &Sent,
        existing: Option<&ResourceState>,
        preferred_hashes: Option<&PersistedAttributes>,
    ) -> Result<PersistedAttributes, ResourceStateEncodingError> {
        let request_keys = resource.attributes.keys().cloned().collect::<BTreeSet<_>>();
        if !sent.is_valid_for(&request_keys) {
            let attributes = match sent {
                Sent::AllRequestAttributes => Vec::new(),
                Sent::Only(attributes) => attributes
                    .difference(&request_keys)
                    .cloned()
                    .collect::<Vec<_>>(),
            };
            return Err(ResourceStateEncodingError::InvalidSentAttributes { attributes });
        }
        let selected = sent.names(&request_keys);
        let previous = existing.and_then(ResourceState::persisted_attributes);
        let mut encoded = HashMap::with_capacity(selected.len());
        for key in selected {
            let Some(value) = resource.attributes.get(key) else {
                return Err(ResourceStateEncodingError::InvalidSentAttributes {
                    attributes: vec![key.clone()],
                });
            };
            let previous_value = previous_hash(preferred_hashes, previous, key);
            encoded.insert(
                key.clone(),
                PersistedValue::encode_desired_value(value, previous_value)?,
            );
        }
        Ok(PersistedAttributes::from_persisted_values(encoded)?)
    }

    fn sent_write_only_keys(
        schema: Option<&ResourceSchema>,
        attributes: &PersistedAttributes,
    ) -> HashSet<String> {
        recorded_write_only_keys(schema, attributes, &[])
    }

    fn populate_desired_metadata<S>(
        &mut self,
        resource: &ResolvedResource<S>,
        existing: Option<&ResourceState>,
        stored_attributes: &HashMap<String, Value>,
        schema: Option<&ResourceSchema>,
        authoring_basis: DesiredAuthoringBasis<'_>,
        comparison: &ComparisonContext<'_>,
    ) {
        if let Some(existing) = existing {
            self.protected = existing.protected;
            self.name_overrides = existing.name_overrides.clone();
        }
        self.directives = PersistedDirectives::from(&resource.directives);
        if self.directives.provider_instance.is_none() {
            self.directives.provider_instance = resource.id.provider_instance.clone();
        }
        self.prefixes = resource.prefixes.clone();
        self.explicit = match authoring_basis {
            DesiredAuthoringBasis::Sent(sent_desired) => {
                let prior_stored_attributes = existing.map(|existing| {
                    existing
                        .persisted_attributes()
                        .map(PersistedAttributes::to_values)
                        .unwrap_or_default()
                });
                let prior_stored_authoring = existing
                    .zip(prior_stored_attributes.as_ref())
                    .map(|(existing, attributes)| (attributes, &existing.explicit));
                explicit::build_from_resource_for_stored_values_after_send(
                    resource,
                    prior_stored_authoring,
                    stored_attributes,
                    &sent_desired,
                    schema,
                    comparison,
                )
            }
            DesiredAuthoringBasis::DesiredRead => {
                explicit::build_from_read_resource_for_stored_values(
                    resource,
                    stored_attributes,
                    schema,
                    comparison,
                )
            }
            DesiredAuthoringBasis::CarriedRead => Self::carried_explicit_for_stored_values(
                resource,
                existing,
                stored_attributes,
                schema,
                comparison,
            ),
        };
        self.binding = resource.binding.clone();
        let dependencies = get_resource_dependencies(resource);
        if !dependencies.is_empty() {
            self.dependency_bindings = dependencies.into_iter().collect();
        }
    }

    fn carried_explicit_for_stored_values<S>(
        resource: &ResolvedResource<S>,
        existing: Option<&ResourceState>,
        stored_attributes: &HashMap<String, Value>,
        schema: Option<&ResourceSchema>,
        comparison: &ComparisonContext<'_>,
    ) -> ExplicitFields {
        let Some(existing) = existing else {
            return explicit::build_from_read_resource_for_stored_values(
                resource,
                stored_attributes,
                schema,
                comparison,
            );
        };
        let prior_stored_attributes = existing
            .persisted_attributes()
            .map(PersistedAttributes::to_values)
            .unwrap_or_default();
        explicit::build_carried_from_resource_for_stored_values(
            resource,
            &prior_stored_attributes,
            &existing.explicit,
            stored_attributes,
            schema,
            comparison,
        )
    }

    /// Build an applied row for a create whose provider reported an identifier
    /// but whose finishing step failed. The typed observation says whether the
    /// resource was found. Values are selected strictly as observed, then sent,
    /// then absent. Desired state controls masking but cannot introduce an
    /// unsent value. The create establishes the current desired resource as the
    /// authoring baseline.
    pub fn from_observation<S: Clone>(
        resource: &ResolvedResource<S>,
        created: &ObservedCreate,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: &ComparisonContext<'_>,
    ) -> Result<Self, ResourceStateEncodingError> {
        let applied_desired = created
            .resolved_desired_attributes()
            .map(|attributes| resource_with_secret_shape_union(resource, attributes));
        let writeback_resource = applied_desired.as_ref().unwrap_or(resource);
        let sent_desired = created.sent_desired_attributes_for(writeback_resource);
        let previous = existing.and_then(ResourceState::persisted_attributes);
        let no_observed_values = HashMap::new();
        let (observed_values, pending_reread) = match created.observation() {
            CreateObservation::Observed(state) => (state.attributes(), None),
            CreateObservation::Unobserved(carina_core::provider::UnobservedReason::NotFound) => {
                (&no_observed_values, Some(PendingReRead::not_found()))
            }
            CreateObservation::Unobserved(carina_core::provider::UnobservedReason::ReadFailed(
                error,
            )) => (
                &no_observed_values,
                Some(PendingReRead::read_failed(error.to_string())),
            ),
        };
        let mut selected = SelectedStateAttributes::try_from_provider_values(
            observed_values,
            schema,
            &[],
            preferred_hashes,
            previous,
        )?;

        let (sent_values, sent_write_only) = match created.sent() {
            SentRecord::Request(sent) => {
                let sent_values = Self::selected_sent_attributes(
                    writeback_resource,
                    sent,
                    existing,
                    preferred_hashes,
                )?;
                let write_only = Self::sent_write_only_keys(schema, &sent_values);
                (sent_values, write_only)
            }
            SentRecord::Persisted(envelope) => {
                let mut sent_values = envelope.write_only().clone();
                sent_values.extend(envelope.secrets().clone());
                let write_only = envelope.write_only().keys().cloned().collect();
                (sent_values, write_only)
            }
        };
        let sent_keys_for_nested_write_only = sent_values.keys().cloned().collect::<Vec<_>>();
        selected.overlay_sent(&sent_values, &sent_write_only);
        let previous_hash_authority = previous
            .map(PreviousSecretHashAuthority::OnlyKeysAbsentFromDesired)
            .unwrap_or(PreviousSecretHashAuthority::None);
        let selected = selected.protect_observation(
            writeback_resource,
            previous_hash_authority,
            preferred_hashes,
        )?;
        let (attributes, write_only_attributes) = selected.finish()?;
        let partial_read = pending_reread.and_then(|pending| {
            let missing_attributes = writeback_resource
                .attributes
                .keys()
                .chain(
                    schema
                        .into_iter()
                        .flat_map(|schema| schema.attributes().keys()),
                )
                .filter(|key| !attributes.contains_key(key))
                .cloned()
                .collect::<BTreeSet<_>>();
            PartialReadMarker::new(pending, missing_attributes)
        });
        let stored_attributes = attributes.to_values();
        let mut nested_write_only = NestedWriteOnlyRecord::default();
        if let Some(schema) = schema {
            nested_write_only.record_sent(
                schema,
                sent_keys_for_nested_write_only.iter().map(String::as_str),
                &writeback_resource.attributes,
                existing.map(ResourceState::nested_write_only),
            )?;
        }

        let mut row = Self::new(
            &resource.id.resource_type,
            resource.identity().clone(),
            resource.id.provider.clone(),
        );
        row.current = CurrentInstance::Applied {
            identifier: created.identifier().clone(),
            attributes,
            write_only_attributes,
            nested_write_only,
            partial_read,
        };
        let authoring_basis = sent_desired
            .map(DesiredAuthoringBasis::Sent)
            .unwrap_or(DesiredAuthoringBasis::CarriedRead);
        row.populate_desired_metadata(
            resource,
            existing,
            &stored_attributes,
            schema,
            authoring_basis,
            comparison,
        );
        Ok(row)
    }

    /// Build the mutually-exclusive v15 continuation row. A persisted re-pend
    /// keeps the prior envelope, timestamp, and host metadata, including a
    /// known authoring record. A legacy `Unrecorded` authoring record is seeded
    /// from desired keys. A freshly sent create encodes sent write-only and
    /// secret values and establishes the current desired authoring baseline.
    pub fn from_in_flight<S>(
        resource: &ResolvedResource<S>,
        pending: &InFlightCreate,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: &ComparisonContext<'_>,
    ) -> Result<Self, ResourceStateEncodingError> {
        let (persisted, sent_desired) = match pending.persistence() {
            InFlightPersistence::Persisted => {
                if let Some(existing) = existing {
                    let mut row = existing.clone();
                    // A re-pend normally keeps the loaded row byte-for-byte,
                    // but a `moved` block deliberately changes the address
                    // that owns it. Write the cloned metadata under the
                    // desired/pending address so Move cleanup cannot delete
                    // the continuation we just preserved.
                    row.provider = resource.id.provider.clone();
                    row.resource_type = resource.id.resource_type.clone();
                    row.identity = resource.identity().clone();
                    // A persisted envelope intentionally contains no route.
                    // Persist the route carried by the rebound continuation,
                    // which is also the route ResumeCreate just used, so the
                    // next load cannot silently bind it back to stale row
                    // directives after a move.
                    row.directives.provider_instance = pending.id().provider_instance.clone();
                    row.current = CurrentInstance::CreateInFlight {
                        pending: pending.clone(),
                    };
                    // A persisted continuation has no fresh provider
                    // attributes with which to reconcile completed versus
                    // pending removals, so preserve every known prior field.
                    // Only a legacy unknown record can be safely seeded from
                    // the desired resource.
                    if matches!(row.explicit, ExplicitFields::Unrecorded) {
                        row.explicit = explicit::build_from_read_resource_for_stored_values(
                            resource,
                            &HashMap::new(),
                            schema,
                            comparison,
                        );
                    }
                    return Ok(row);
                }
                (pending.clone(), None)
            }
            InFlightPersistence::Fresh(fresh) => {
                let selected = Self::selected_sent_attributes(
                    resource,
                    fresh.sent(),
                    existing,
                    preferred_hashes,
                )?;
                let mut write_only = selected.clone();
                let sent_write_only = Self::sent_write_only_keys(schema, &selected);
                write_only.retain_keys(|key| sent_write_only.contains(key));
                let mut secrets = selected;
                secrets.retain_keys(|key| {
                    resource
                        .attributes
                        .get(key)
                        .is_some_and(carina_core::value::contains_secret)
                });
                (
                    fresh.persist(write_only, secrets),
                    Some(fresh.sent_desired_attributes_for(resource)),
                )
            }
        };

        let mut row = Self::new(
            &resource.id.resource_type,
            resource.identity().clone(),
            resource.id.provider.clone(),
        );
        row.current = CurrentInstance::CreateInFlight { pending: persisted };
        let authoring_basis = sent_desired
            .map(DesiredAuthoringBasis::Sent)
            .unwrap_or(DesiredAuthoringBasis::CarriedRead);
        row.populate_desired_metadata(
            resource,
            existing,
            &HashMap::new(),
            schema,
            authoring_basis,
            comparison,
        );
        Ok(row)
    }

    /// Build the token-less protected row for a create whose cloud outcome is
    /// unknown. Existing host metadata and deposed generations are retained.
    pub fn from_unknown_create_outcome<S>(
        resource: &ResolvedResource<S>,
        unknown: &UnknownCreateOutcome,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        comparison: &ComparisonContext<'_>,
    ) -> Self {
        let mut row = existing.cloned().unwrap_or_else(|| {
            Self::new(
                &resource.id.resource_type,
                resource.identity().clone(),
                resource.id.provider.clone(),
            )
        });
        row.provider = resource.id.provider.clone();
        row.resource_type = resource.id.resource_type.clone();
        row.identity = resource.identity().clone();
        row.current = CurrentInstance::CreateOutcomeUnknown {
            unknown: unknown.clone(),
        };
        row.populate_desired_metadata(
            resource,
            existing,
            &HashMap::new(),
            schema,
            DesiredAuthoringBasis::DesiredRead,
            comparison,
        );
        row
    }

    /// Build a `ResourceState` from a desired resource, provider-returned
    /// state, optional previous row state, and optional schema.
    ///
    /// Provider attributes are serialized into state JSON, desired secret
    /// values are masked before storage, and previously-stored secret hashes
    /// can protect keys removed from desired state. `row_basis` couples
    /// write-only and authoring provenance so a successful request cannot
    /// retain a stale authoring record. When `existing` is provided,
    /// protected/name override metadata and existing deposed generations are
    /// preserved by the row-write path.
    ///
    /// Returns an error if any provider attribute value cannot be converted to
    /// JSON, such as non-finite float values.
    pub fn from_provider_state_for_resource_and_schema<S: Clone>(
        resource: &ResolvedResource<S>,
        state: &State,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        row_basis: RowBasis<'_>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: &ComparisonContext<'_>,
    ) -> Result<Self, ResourceStateEncodingError> {
        // The resource carried by a saved plan still contains deferred expressions.
        // Once an operation has run, secret protection must follow the provider-ready
        // desired values that were actually accepted, or a secret selected by a
        // coalesce/function/interpolation can be persisted as provider plaintext.
        let applied_desired = match row_basis {
            RowBasis::Applied(request) => request
                .resolved_desired_attributes()
                .map(|attributes| resource_with_secret_shape_union(resource, attributes)),
            RowBasis::Imported | RowBasis::Carried => None,
        };
        let writeback_resource = applied_desired.as_ref().unwrap_or(resource);
        let mut rs = Self::new(
            &resource.id.resource_type,
            resource.identity().clone(),
            resource.id.provider.clone(),
        );
        let previous = existing.and_then(ResourceState::persisted_attributes);
        let desired_keys = writeback_resource
            .attributes
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let schema_write_only = schema_write_only_keys(schema);
        let carried_write_only = previous
            .map(|attributes| recorded_write_only_keys(schema, attributes, &[]))
            .unwrap_or_default();
        let mut accepted = BTreeSet::new();
        match row_basis {
            RowBasis::Applied(request) => match request.kind() {
                AcceptedRequestKind::Create => {
                    accepted.extend(
                        desired_keys
                            .iter()
                            .filter(|key| schema_write_only.contains(*key))
                            .cloned(),
                    );
                }
                AcceptedRequestKind::Update { sent, .. } => {
                    accepted.extend(
                        sent.iter()
                            .filter(|key| {
                                desired_keys.contains(*key) && schema_write_only.contains(*key)
                            })
                            .cloned(),
                    );
                }
            },
            RowBasis::Imported | RowBasis::Carried => {}
        }
        let desired_fallback = Sent::only(
            accepted
                .iter()
                .filter(|key| !state.attributes.contains_key(*key))
                .cloned(),
        );
        let desired_fallback = Self::selected_sent_attributes(
            writeback_resource,
            &desired_fallback,
            existing,
            preferred_hashes,
        )?;

        let mut selected = SelectedStateAttributes::try_from_provider_values(
            &state.attributes,
            schema,
            &[],
            preferred_hashes,
            previous,
        )?;
        for key in &schema_write_only {
            let value = if accepted.contains(key) {
                match state.attributes.get(key) {
                    Some(value) => Some(PersistedValue::encode_provider_value(
                        value,
                        previous_hash(preferred_hashes, previous, key),
                    )?),
                    None => desired_fallback.encoded_value(key).cloned(),
                }
            } else {
                let carries_previous = match row_basis {
                    RowBasis::Imported | RowBasis::Carried => true,
                    RowBasis::Applied(request) => match request.kind() {
                        AcceptedRequestKind::Create => false,
                        AcceptedRequestKind::Update { removed, .. } => !removed.contains(key),
                    },
                };
                (carries_previous && carried_write_only.contains(key))
                    .then(|| previous.and_then(|attributes| attributes.encoded_value(key)))
                    .flatten()
                    .cloned()
            };
            if let Some(value) = value {
                selected.insert_write_only(key.clone(), value);
            }
        }

        let previous_hash_authority = existing
            .and_then(ResourceState::persisted_attributes)
            .map(PreviousSecretHashAuthority::OnlyKeysAbsentFromDesired)
            .unwrap_or(PreviousSecretHashAuthority::None);
        let selected = selected.protect_for_resource(
            writeback_resource,
            previous_hash_authority,
            preferred_hashes,
            true,
        )?;
        let (attributes, write_only_attributes) = selected.finish()?;
        let previous_nested_write_only = existing
            .map(ResourceState::nested_write_only)
            .cloned()
            .unwrap_or_default();
        let (mut nested_write_only, keys_to_record) = match row_basis {
            RowBasis::Applied(request) => match request.kind() {
                AcceptedRequestKind::Create => (
                    NestedWriteOnlyRecord::default(),
                    writeback_resource
                        .attributes
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                ),
                AcceptedRequestKind::Update { sent, removed } => {
                    let mut record = previous_nested_write_only.clone();
                    for key in removed {
                        record.remove_attribute(key);
                    }
                    (record, sent.iter().map(String::as_str).collect::<Vec<_>>())
                }
            },
            RowBasis::Imported => (NestedWriteOnlyRecord::default(), Vec::new()),
            RowBasis::Carried => (previous_nested_write_only.clone(), Vec::new()),
        };
        if let Some(schema) = schema {
            nested_write_only.record_sent(
                schema,
                keys_to_record,
                &writeback_resource.attributes,
                Some(&previous_nested_write_only),
            )?;
        }
        rs.current = match state
            .identifier
            .as_deref()
            .and_then(|identifier| AppliedIdentifier::new(identifier).ok())
        {
            Some(identifier) => CurrentInstance::Applied {
                identifier,
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read: state.partial_read.clone(),
            },
            None => CurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read: state.partial_read.clone(),
            },
        };
        let authoring_basis = match row_basis {
            RowBasis::Applied(request) => {
                DesiredAuthoringBasis::Sent(request.sent_desired_attributes(writeback_resource))
            }
            RowBasis::Imported => DesiredAuthoringBasis::DesiredRead,
            RowBasis::Carried => DesiredAuthoringBasis::CarriedRead,
        };
        rs.populate_desired_metadata(
            resource,
            existing,
            &state.attributes,
            schema,
            authoring_basis,
            comparison,
        );
        Ok(rs)
    }
}

/// Returns true if the `ExplicitFields` is the default (`Leaf`) — used
/// as a `skip_serializing_if` predicate so resources that have not yet
/// been touched by a state constructor don't emit a verbose
/// `"explicit": {"kind": "leaf"}` line. A recorded empty `Struct` is not
/// skipped: it records that a desired-backed write saw no authored attributes.
fn is_empty_explicit(e: &ExplicitFields) -> bool {
    matches!(e, ExplicitFields::Leaf)
}

/// Exact v14 row shape used only at the v14 → v15 migration boundary.
/// Keeping this DTO separate from [`ResourceState`] prevents the current
/// loader from accepting the legacy parallel identifier/attribute fields.
#[derive(Deserialize)]
struct ResourceStateV14CurrentDto {
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    attributes: HashMap<String, serde_json::Value>,
    #[serde(default)]
    deposed: Vec<serde_json::Value>,
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    directives: PersistedDirectives,
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
    partial_read: Option<serde_json::Value>,
}

fn migrate_v14_current_instance(content: &str) -> Result<String, BackendError> {
    let mut root: serde_json::Value = serde_json::from_str(content).map_err(|error| {
        BackendError::InvalidState(format!("Failed to migrate state file from v14: {error}"))
    })?;
    let resources = root
        .get_mut("resources")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| {
            BackendError::InvalidState(
                "Failed to migrate state file from v14: resources must be an array".to_string(),
            )
        })?;
    for resource in resources {
        let legacy: ResourceStateV14CurrentDto =
            serde_json::from_value(resource.clone()).map_err(|error| {
                BackendError::InvalidState(format!(
                    "Failed to migrate state file from v14: {error}"
                ))
            })?;
        let shell_directives = PersistedDirectives {
            provider_instance: legacy.directives.provider_instance.clone(),
            ..PersistedDirectives::default()
        };
        let is_deposed_only_shell = legacy.identifier.is_none()
            && !legacy.deposed.is_empty()
            && legacy.attributes.is_empty()
            && !legacy.protected
            && legacy.directives == shell_directives
            && legacy.prefixes.is_empty()
            && legacy.name_overrides.is_empty()
            && is_empty_explicit(&legacy.explicit)
            && legacy.binding.is_none()
            && legacy.dependency_bindings.is_empty()
            && legacy.write_only_attributes.is_empty()
            && legacy.partial_read.is_none();
        let current = match legacy
            .identifier
            .filter(|identifier| !identifier.is_empty())
        {
            Some(identifier) => serde_json::json!({
                "kind": "applied",
                "identifier": identifier,
                "attributes": legacy.attributes,
                "write_only_attributes": legacy.write_only_attributes,
                "partial_read": legacy.partial_read,
            }),
            None if is_deposed_only_shell => serde_json::json!({ "kind": "none" }),
            None => serde_json::json!({
                "kind": "missing_identifier",
                "attributes": legacy.attributes,
                "write_only_attributes": legacy.write_only_attributes,
                "partial_read": legacy.partial_read,
            }),
        };
        let object = resource.as_object_mut().ok_or_else(|| {
            BackendError::InvalidState(
                "Failed to migrate state file from v14: resource must be an object".to_string(),
            )
        })?;
        object.remove("identifier");
        object.remove("attributes");
        object.remove("write_only_attributes");
        object.remove("partial_read");
        object.insert("current".to_string(), current);
    }
    root["version"] = serde_json::json!(StateFile::CURRENT_VERSION);
    root["exports_version"] = serde_json::json!(PublishedExports::CURRENT_VERSION);
    serde_json::to_string(&root).map_err(|error| {
        BackendError::InvalidState(format!("Failed to migrate state file from v14: {error}"))
    })
}

/// v17 → v18: initialize leaf provenance on every attribute-bearing current
/// instance. In-flight and deposed-only rows have no accepted applied values.
fn migrate_v17_nested_write_only(content: &str) -> Result<String, BackendError> {
    let mut root: serde_json::Value = serde_json::from_str(content).map_err(|error| {
        BackendError::InvalidState(format!("Failed to migrate state file from v17: {error}"))
    })?;
    let resources = root
        .get_mut("resources")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| {
            BackendError::InvalidState(
                "Failed to migrate state file from v17: resources must be an array".to_string(),
            )
        })?;
    for resource in resources {
        let Some(current) = resource
            .get_mut("current")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        if matches!(
            current.get("kind").and_then(serde_json::Value::as_str),
            Some("applied" | "missing_identifier")
        ) {
            current
                .entry("nested_write_only".to_string())
                .or_insert_with(|| serde_json::json!({}));
        }
    }
    root["version"] = serde_json::json!(StateFile::CURRENT_VERSION);
    root.as_object_mut()
        .expect("a state file root validated as an object")
        .entry("exports_version".to_string())
        .or_insert_with(|| serde_json::json!(PublishedExports::CURRENT_VERSION));
    serde_json::to_string(&root).map_err(|error| {
        BackendError::InvalidState(format!("Failed to migrate state file from v17: {error}"))
    })
}

/// v18 → v19: wrap the legacy marker detail in the typed incomplete-read
/// reason used by current state and runtime unknowns.
fn migrate_v18_partial_read_reason(content: &str) -> Result<String, BackendError> {
    let mut root: serde_json::Value = serde_json::from_str(content).map_err(|error| {
        BackendError::InvalidState(format!("Failed to migrate state file from v18: {error}"))
    })?;
    let resources = root
        .get_mut("resources")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| {
            BackendError::InvalidState(
                "Failed to migrate state file from v18: resources must be an array".to_string(),
            )
        })?;
    for resource in resources {
        let Some(current) = resource
            .get_mut("current")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let empty_partial_read = current
            .get("partial_read")
            .and_then(serde_json::Value::as_object)
            .and_then(|marker| marker.get("missing_attributes"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty);
        if empty_partial_read {
            current.remove("partial_read");
            continue;
        }
        let Some(partial_read) = current.get_mut("partial_read") else {
            continue;
        };
        if partial_read.is_null() {
            continue;
        }
        let marker = partial_read.as_object_mut().ok_or_else(|| {
            BackendError::InvalidState(
                "Failed to migrate state file from v18: partial_read must be an object".to_string(),
            )
        })?;
        let detail = marker
            .remove("detail")
            .and_then(|detail| detail.as_str().map(str::to_owned))
            .ok_or_else(|| {
                BackendError::InvalidState(
                    "Failed to migrate state file from v18: partial_read.detail must be a string"
                        .to_string(),
                )
            })?;
        marker.insert(
            "pending".to_string(),
            serde_json::json!({
                "reason": {
                    "kind": "incomplete",
                    "detail": detail,
                }
            }),
        );
    }
    root["version"] = serde_json::json!(19);
    root.as_object_mut()
        .expect("a state file root validated as an object")
        .entry("exports_version".to_string())
        .or_insert_with(|| serde_json::json!(PublishedExports::CURRENT_VERSION));
    serde_json::to_string(&root).map_err(|error| {
        BackendError::InvalidState(format!("Failed to migrate state file from v18: {error}"))
    })
}

/// v19 → v20: reserve the token-less create-outcome-unknown current kind.
/// Existing resource rows need no structural rewrite.
fn migrate_v19_create_outcome_unknown(content: &str) -> Result<String, BackendError> {
    let mut root: serde_json::Value = serde_json::from_str(content).map_err(|error| {
        BackendError::InvalidState(format!("Failed to migrate state file from v19: {error}"))
    })?;
    root["version"] = serde_json::json!(StateFile::CURRENT_VERSION);
    serde_json::to_string(&root).map_err(|error| {
        BackendError::InvalidState(format!("Failed to migrate state file from v19: {error}"))
    })
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

/// v13 → v14: migrate legacy secret strings only inside current and deposed
/// resource attributes. Export strings never had resource-state marker
/// semantics and deliberately remain literal.
fn migrate_v13_resource_secret_markers(content: &str) -> Result<String, BackendError> {
    let mut raw: serde_json::Value = serde_json::from_str(content).map_err(|error| {
        BackendError::InvalidState(format!(
            "Failed to re-parse state file for v13 secret migration: {error}"
        ))
    })?;
    carina_core::resource::legacy_secret_migration::migrate_state_document_v13(&mut raw).map_err(
        |error| {
            BackendError::InvalidState(format!("Failed to migrate v13 resource secrets: {error}"))
        },
    )?;

    serde_json::to_string(&raw).map_err(|error| {
        BackendError::InvalidState(format!(
            "Failed to serialize state file after v13 secret migration: {error}"
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
/// `ExplicitFields::Unrecorded`. In v6 the empty-Struct shape on disk
/// represented the legacy-corruption pattern (the older for-loop expansion
/// path that lost child attributes before reaching writeback). Current-version
/// rows may legitimately use an empty Struct to record that the desired
/// resource authored no attributes; the migration remains version-gated so
/// those rows are not rewritten.
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
        match &mut resource.current {
            CurrentInstance::Applied { attributes, .. }
            | CurrentInstance::MissingIdentifier { attributes, .. } => {
                attributes.retain_keys(|key| !key.starts_with('_'));
            }
            CurrentInstance::None
            | CurrentInstance::CreateInFlight { .. }
            | CurrentInstance::CreateOutcomeUnknown { .. } => {}
        }
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
