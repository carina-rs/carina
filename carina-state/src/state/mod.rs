//! State file structures for persisting infrastructure state

use std::borrow::Cow;

use carina_core::binding_index::SecretPreservingAttributes;
use carina_core::differ::{ComparisonContext, Orphan, ReferenceEquality};
use carina_core::effect::{MoveAuthority, ReleaseListKeysEffect};
use carina_core::executor::{AcceptedRequest, AcceptedRequestKind};
use carina_core::explicit::{self, ExplicitFields};
use carina_core::name_override::NameOverrideSource;
pub use carina_core::name_override::{ApplyDecision, NameOverride, should_apply_override};
use carina_core::parser::StateBlockAddress;
use carina_core::plan::ReplacementPreviousAttributesForStateWriteback;
use carina_core::provider::{
    AppliedIdentifier, EmptyAppliedIdentifier, HostCreateObservation, HostIdentifiedCreate,
    InFlightCreate, InFlightPersistence, NestedWriteOnlyRecord, PersistedAttributes,
    PersistedAttributesError, RawSavedAttrs, Sent, SentDesiredAttributes, SentRecord,
    StateTransformMap, UnknownCreateOutcome, merge_previous_secret_hashes_into_provider_value,
    previous_hash,
};
pub use carina_core::resource::DeposedKey;
#[cfg(test)]
use carina_core::resource::Resource;
use carina_core::resource::knowledge::is_known_public;
use carina_core::resource::{
    AbsoluteBindingAddress, ConcreteValue, Current, DataSource, PartialReadMarker, PendingReRead,
    PersistedDestroyDependencies, PersistedDirectives, ResolvedResource, ResolvedResourceId,
    ResourceId, ResourceIdentity, SnapshotState, SnapshotValue, State, StateValueLeaf,
    UnavailableSecretError, Value,
};
use carina_core::schema::{
    CompiledResourceSchema as ResourceSchema, FieldName, SchemaKind, SchemaRegistry,
    schema_write_only_keys,
};
use carina_core::validation::admission::final_input::{FinalDesiredAttributes, FinalDesiredValues};
use carina_core::value::{
    PersistedExportValue, PersistedValue, SerializationError, contains_unredacted_secret,
    merge_secrets_into_observation_json, merge_secrets_into_provider_json,
    union_secret_shapes_after_resolution,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::backend::{BackendError, InvalidResourceIdentityError, InvalidStateJsonError};
pub use carina_core::state_document::CurrentInstance;
use carina_core::state_document::{
    StateDocumentDecodeError as StateFileDecodeError, decode_persisted_exports,
    validate_deposed_identities, validate_exports_version, validate_resource_identities,
};

#[cfg(any(test, feature = "test-utils"))]
fn persisted_attributes_from_test_json(
    attributes: impl IntoIterator<Item = (String, serde_json::Value)>,
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

// Release authority is private to the row serialization seam. Public row
// writers receive previous attributes and cannot choose a releasing policy.
enum PreviousSecretHashAuthority<'a> {
    None,
    AllPreviouslyHashedKeys(&'a PersistedAttributes),
    MatchingFinalDesiredValue {
        previous: &'a PersistedAttributes,
        desired: &'a FinalDesiredAttributes,
    },
    AcceptedWrite {
        previous: &'a PersistedAttributes,
        request: &'a AcceptedRequest,
        request_attributes: Cow<'a, IndexMap<String, Value>>,
        final_desired: Option<&'a FinalDesiredAttributes>,
    },
}

#[derive(Clone, Copy)]
struct DesiredValueComparison<'a> {
    schema: Option<&'a ResourceSchema>,
    context: carina_core::differ::ComparisonSchema<'a>,
}

/// Whether desired provides a fully concrete public value for an attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesiredKey {
    Absent,
    KnownPublic,
    NotKnownPublic,
}

impl DesiredKey {
    fn from_attributes(attributes: &IndexMap<String, Value>, key: &str) -> Self {
        // Known proofs can contain protected Secret nodes; exclude those too.
        match attributes.get(key) {
            None => Self::Absent,
            Some(value) if is_known_public(value) => Self::KnownPublic,
            Some(_) => Self::NotKnownPublic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviousSecretHashDecision {
    Release,
    Protect,
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
    encoded: BTreeMap<String, PersistedValue>,
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

    fn from_provider_values_lossy<P: StateValueLeaf>(
        attributes: &BTreeMap<String, Value<P>>,
        schema: Option<&ResourceSchema>,
        additionally_recorded: &[String],
    ) -> Result<Self, SerializationError> {
        let write_only_keys = Self::write_only_keys(schema, additionally_recorded);
        let mut encoded = BTreeMap::new();
        for (key, value) in attributes {
            if write_only_keys.contains(key) {
                continue;
            }
            match PersistedValue::encode_provider_value(value, None) {
                Ok(value) => {
                    encoded.insert(key.clone(), value);
                }
                Err(error @ SerializationError::UnavailableSecret(_)) => return Err(error),
                Err(_) => {}
            }
        }
        Ok(Self {
            encoded,
            write_only_keys,
            retained_write_only: HashSet::new(),
        })
    }

    fn try_from_provider_values<P: StateValueLeaf>(
        attributes: &BTreeMap<String, Value<P>>,
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
            .collect::<BTreeMap<_, _>>();
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

    fn authority_attributes<'a>(
        authority: &'a PreviousSecretHashAuthority<'_>,
    ) -> Option<&'a PersistedAttributes> {
        match authority {
            PreviousSecretHashAuthority::None => None,
            PreviousSecretHashAuthority::AllPreviouslyHashedKeys(attributes)
            | PreviousSecretHashAuthority::MatchingFinalDesiredValue {
                previous: attributes,
                ..
            }
            | PreviousSecretHashAuthority::AcceptedWrite {
                previous: attributes,
                ..
            } => Some(attributes),
        }
    }

    fn decide_previous_secret_hash(
        authority: &PreviousSecretHashAuthority<'_>,
        key: &str,
        provider_value: Option<&PersistedValue>,
        comparison: Option<DesiredValueComparison<'_>>,
    ) -> PreviousSecretHashDecision {
        let final_desired = match authority {
            PreviousSecretHashAuthority::None => return PreviousSecretHashDecision::Release,
            PreviousSecretHashAuthority::AllPreviouslyHashedKeys(_) => {
                return PreviousSecretHashDecision::Protect;
            }
            PreviousSecretHashAuthority::AcceptedWrite {
                request,
                request_attributes,
                final_desired,
                ..
            } => {
                if DesiredKey::from_attributes(request_attributes, key) == DesiredKey::KnownPublic
                    && match request.kind() {
                        AcceptedRequestKind::Create => true,
                        AcceptedRequestKind::Update { sent, .. } => sent.contains(key),
                    }
                {
                    return PreviousSecretHashDecision::Release;
                }
                match final_desired {
                    Some(desired) => *desired,
                    None => return PreviousSecretHashDecision::Protect,
                }
            }
            PreviousSecretHashAuthority::MatchingFinalDesiredValue { desired, .. } => *desired,
        };
        let desired_attributes = final_desired.attributes();
        if DesiredKey::from_attributes(desired_attributes, key) != DesiredKey::KnownPublic {
            return PreviousSecretHashDecision::Protect;
        }
        // Decode through the existing state projection. A retained hash has no
        // plaintext projection and cannot prove equality to a final public value.
        // Neither comparison operand nor projection error leaves this seam.
        let provider_plaintext = provider_value
            .and_then(PersistedValue::decode)
            .and_then(|value| value.to_value_at(key).ok());
        if provider_plaintext
            .as_ref()
            .zip(comparison)
            .is_some_and(|(provider, comparison)| {
                carina_core::differ::public_final_desired_matches_provider(
                    provider,
                    final_desired,
                    key,
                    comparison.schema,
                    &comparison.context,
                )
            })
        {
            PreviousSecretHashDecision::Release
        } else {
            PreviousSecretHashDecision::Protect
        }
    }

    fn protect_previous_secret_hashes(
        &mut self,
        authority: &PreviousSecretHashAuthority<'_>,
        protected_secret_keys: &mut HashSet<String>,
        comparison: Option<DesiredValueComparison<'_>>,
    ) -> Result<(), SerializationError> {
        let Some(previous) = Self::authority_attributes(authority) else {
            return Ok(());
        };
        let keys = previous.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            if protected_secret_keys.contains(&key)
                || Self::decide_previous_secret_hash(
                    authority,
                    &key,
                    self.encoded.get(&key),
                    comparison,
                ) == PreviousSecretHashDecision::Release
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
            )?
            else {
                continue;
            };
            self.encoded.insert(key.clone(), masked);
            protected_secret_keys.insert(key);
        }
        Ok(())
    }

    fn protect_for_resource<S>(
        mut self,
        resource: &ResolvedResource<S>,
        previous_hash_authority: PreviousSecretHashAuthority<'_>,
        preferred_hashes: Option<&PersistedAttributes>,
        strict_desired: bool,
        comparison: Option<DesiredValueComparison<'_>>,
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
                Self::authority_attributes(&previous_hash_authority),
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
                Err(error)
                    if strict_desired
                        || matches!(&error, SerializationError::UnavailableSecret(_)) =>
                {
                    return Err(error);
                }
                Err(_) => {
                    self.encoded.remove(key);
                }
            }
        }
        self.protect_previous_secret_hashes(
            &previous_hash_authority,
            &mut protected_secret_keys,
            comparison,
        )?;
        Ok(self)
    }

    fn protect_observation<S>(
        mut self,
        resource: &ResolvedResource<S>,
        previous_attributes: Option<&PersistedAttributes>,
        final_desired: Option<&FinalDesiredAttributes>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: Option<DesiredValueComparison<'_>>,
    ) -> Result<Self, SerializationError> {
        let mut protected_secret_keys = HashSet::new();
        for (key, desired_value) in &resource.attributes {
            if !contains_unredacted_secret(desired_value) {
                continue;
            }
            let Some(observed_or_sent_value) = self.encoded.get(key).cloned() else {
                continue;
            };
            let previous = previous_hash(preferred_hashes, previous_attributes, key);
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
            &match (previous_attributes, final_desired) {
                (Some(previous), Some(desired)) => {
                    PreviousSecretHashAuthority::MatchingFinalDesiredValue { previous, desired }
                }
                (Some(previous), None) => {
                    PreviousSecretHashAuthority::AllPreviouslyHashedKeys(previous)
                }
                (None, _) => PreviousSecretHashAuthority::None,
            },
            &mut protected_secret_keys,
            comparison,
        )?;
        self.encoded.retain(|key, _| present_keys.contains(key));
        Ok(self)
    }

    fn protect_orphan(
        mut self,
        previous: &PersistedAttributes,
    ) -> Result<Self, SerializationError> {
        self.protect_previous_secret_hashes(
            &PreviousSecretHashAuthority::AllPreviouslyHashedKeys(previous),
            &mut HashSet::new(),
            None,
        )?;
        Ok(self)
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
    #[error(transparent)]
    UnavailableSecret(#[from] UnavailableSecretError),
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

/// Provenance of a row rebuilt from provider material. An observation never
/// carries an accepted request; only the provider branch has a RowBasis.
pub enum ResourceStateWrite<'a> {
    Provider {
        state: &'a SnapshotState,
        basis: RowBasis<'a>,
    },
    Observation(&'a HostIdentifiedCreate),
}

#[derive(Debug, Clone)]
enum DesiredAuthoringBasis<'a> {
    Sent(SentDesiredAttributes<'a>),
    DesiredRead,
    CarriedRead,
}

/// Published values retained until source-key selection and admission.
///
/// Construction validates the persisted namespace and structure. Only admission
/// decodes installed keys, so corrupt stale values cannot block downstreams.
/// Managed resource rows are not decoded or checked against the state version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedExports {
    values: HashMap<String, PersistedExportValue>,
}

#[derive(Deserialize)]
struct PublishedExportsEnvelope {
    exports_version: Option<u32>,
    #[serde(default)]
    exports: HashMap<String, serde_json::Value>,
}

impl PublishedExports {
    /// Current persisted exports encoding version.
    ///
    /// Bump this only when [`PersistedExportValue`]'s encoding changes.
    pub const CURRENT_VERSION: u32 = carina_core::state_document::CURRENT_EXPORTS_VERSION;

    /// Read exports without decoding resource rows or checking their state version.
    pub fn from_state_bytes(bytes: &[u8]) -> Result<Self, BackendError> {
        let envelope: PublishedExportsEnvelope =
            serde_json::from_slice(bytes).map_err(|error| {
                BackendError::InvalidState(format!(
                    "Failed to parse published exports envelope: {error}"
                ))
            })?;
        validate_exports_version(envelope.exports_version).map_err(map_state_file_decode_error)?;
        decode_persisted_exports(envelope.exports)
            .map(|values| Self { values })
            .map_err(map_state_file_decode_error)
    }

    pub fn values(&self) -> &HashMap<String, PersistedExportValue> {
        &self.values
    }
}

/// The main state file structure that persists to the backend
#[derive(Debug, Clone)]
pub struct StateFile {
    /// Monotonically increasing number for each state modification
    pub serial: u64,
    /// Unique identifier for this state lineage (prevents accidental overwrites)
    pub lineage: String,
    /// Version of Carina that last modified this state
    pub carina_version: String,
    /// All managed resources and their current state
    resources: Vec<ResourceState>,
    /// Published exports for remote_state consumers
    pub exports: HashMap<String, PersistedExportValue>,
    /// Loaded field order; typed values and lifecycle state remain authoritative.
    wire_order: Option<std::sync::Arc<carina_core::json_field_order::JsonFieldOrder>>,
    /// Immutable evidence produced by the original decode, before mutations
    /// of this working state. New, unwritten files have no backend evidence.
    loaded_observation: Option<ObservationStateAuthority>,
}

impl carina_core::value::BindingState for StateFile {
    fn observation_attributes(&self, id: &ResourceId) -> Option<&PersistedAttributes> {
        self.find_resource_for_id(id)
            .and_then(ResourceState::persisted_attributes)
    }
}

#[derive(Serialize)]
struct WritableStateFile<'a> {
    version: u32,
    exports_version: u32,
    serial: u64,
    lineage: &'a str,
    carina_version: &'a str,
    resources: &'a [ResourceState],
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    exports: &'a HashMap<String, PersistedExportValue>,
}

impl Serialize for StateFile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let wire = WritableStateFile {
            version: Self::CURRENT_VERSION,
            exports_version: PublishedExports::CURRENT_VERSION,
            serial: self.serial,
            lineage: &self.lineage,
            carina_version: &self.carina_version,
            resources: &self.resources,
            exports: &self.exports,
        };
        match &self.wire_order {
            Some(order) => order.serialize(&wire, serializer),
            None => wire.serialize(serializer),
        }
    }
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

/// Cached or refreshed orphan values ready for planner conversion.
///
/// Converting to planner input preserves malformed rows as existing states
/// without identifiers, so the differ emits
/// [`carina_core::plan::PlanErrorKind::MissingProviderIdentifier`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedOrphans {
    by_id: carina_core::differ::Orphans,
}

impl LoadedOrphans {
    pub fn insert(&mut self, key: ResolvedResourceId, orphan: Orphan) {
        self.by_id.insert(key, orphan);
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
        self.by_id
    }
}

impl IntoIterator for LoadedOrphans {
    type Item = (ResourceId, Orphan);
    type IntoIter = <carina_core::differ::Orphans as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.by_id.into_iter()
    }
}

impl FromIterator<(ResolvedResourceId, Orphan)> for LoadedOrphans {
    fn from_iter<T: IntoIterator<Item = (ResolvedResourceId, Orphan)>>(iter: T) -> Self {
        Self {
            by_id: iter.into_iter().collect(),
        }
    }
}

fn map_state_file_decode_error(error: StateFileDecodeError) -> BackendError {
    match error {
        StateFileDecodeError::ExportsVersionTooNew { found, supported } => {
            BackendError::ExportsVersionTooNew { found, supported }
        }
        StateFileDecodeError::StateVersionTooOld { found, supported } => {
            BackendError::StateVersionTooOld { found, supported }
        }
        StateFileDecodeError::StateVersionTooNew { found, supported } => {
            BackendError::StateVersionTooNew { found, supported }
        }
        StateFileDecodeError::InvalidResourceIdentity {
            resource_index,
            provider,
            resource_type,
            identifier,
            source,
        } => InvalidResourceIdentityError::new(
            resource_index,
            provider,
            resource_type,
            identifier,
            source,
        )
        .into(),
        StateFileDecodeError::Json { json_path, source } => BackendError::InvalidStateJson(
            InvalidStateJsonError::new("Failed to parse state file".to_string(), json_path, source),
        ),
        StateFileDecodeError::Validation(message) => {
            BackendError::InvalidState(format!("Failed to parse state file: {message}"))
        }
    }
}

impl StateFile {
    #[cfg(any(test, feature = "test-utils"))]
    fn fixture_observation_authority(
        &self,
    ) -> Result<ObservationStateAuthority, StateFileDecodeError> {
        // Synthetic fixtures can mutate or construct their entire history.
        #[derive(Serialize)]
        struct ObservationFile<'a> {
            version: u32,
            serial: u64,
            lineage: &'a str,
            carina_version: &'a str,
            exports_version: u32,
            exports: &'a HashMap<String, PersistedExportValue>,
            resources: &'a [ResourceState],
        }
        let document = ObservationFile {
            version: Self::CURRENT_VERSION,
            serial: self.serial,
            lineage: &self.lineage,
            carina_version: &self.carina_version,
            exports_version: PublishedExports::CURRENT_VERSION,
            exports: &self.exports,
            resources: &self.resources,
        };
        let bytes = serde_json::to_vec(&document).map_err(StateFileDecodeError::json_at_root)?;
        Ok(ObservationStateAuthority {
            state: std::sync::Arc::new(carina_core::value::ObservationState::read_state_file(
                &bytes,
            )?),
        })
    }

    /// Crate-private validated parse seam. `StateFile` intentionally has no
    /// public `Deserialize` impl: every external parse must go through the
    /// version gate ([`load_state`] or [`load_state_bytes`]), so
    /// consumers can read only current-version state files
    /// (carina#3731). Parsing here still validates duplicate identities on
    /// every parse (carina#2858).
    pub(crate) fn from_json_str(content: &str) -> Result<Self, StateFileDecodeError> {
        let (document, authority) =
            carina_core::value::ObservationState::read_state_file(content.as_bytes())?
                .into_document_and_authority();
        Ok(Self {
            serial: document.serial,
            lineage: document.lineage,
            carina_version: document.carina_version,
            resources: document
                .resources
                .into_iter()
                .map(ResourceState::from_decoded)
                .collect(),
            exports: document.exports,
            wire_order: Some(document.wire_order),
            loaded_observation: Some(ObservationStateAuthority {
                state: std::sync::Arc::new(authority),
            }),
        })
    }

    /// Current state format: tagged current instances, structural persisted values,
    /// independently versioned exports, and resolved managed-row dependencies.
    pub const CURRENT_VERSION: u32 = carina_core::state_document::CURRENT_STATE_VERSION;

    /// Create a new empty state file
    pub fn new() -> Self {
        Self {
            serial: 0,
            lineage: uuid::Uuid::new_v4().to_string(),
            carina_version: env!("CARGO_PKG_VERSION").to_string(),
            resources: Vec::new(),
            exports: HashMap::new(),
            wire_order: None,
            loaded_observation: None,
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
            serial: 0,
            lineage,
            wire_order: None,
            loaded_observation: None,
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
        validate_resource_identities(self.resources.iter().map(|row| {
            (
                row.identity_fields(),
                row.deposed.iter().map(DeposedInstance::identity_fields),
            )
        }))
    }

    /// Copy typed stored rows into core's immutable planning source.
    /// All fields have already crossed state decoding; projection is infallible.
    pub fn planning_rows(&self) -> carina_core::planning_state::PlanningStateRows {
        use carina_core::planning_state::{
            PlanningCurrentKind, PlanningDeposedRow, PlanningStateRow,
        };
        self.resources
            .iter()
            .map(|row| {
                PlanningStateRow::from_persisted(
                    ResolvedResourceId::with_provider_identity(
                        row.provider.clone(),
                        row.resource_type.clone(),
                        row.identity.clone(),
                        row.directives.provider_instance.clone(),
                    ),
                    row.identifier().cloned(),
                    match row.current() {
                        CurrentInstance::None => PlanningCurrentKind::None,
                        CurrentInstance::Applied { .. } => PlanningCurrentKind::Applied,
                        CurrentInstance::MissingIdentifier { .. } => {
                            PlanningCurrentKind::MissingIdentifier
                        }
                        CurrentInstance::CreateInFlight { .. } => {
                            PlanningCurrentKind::CreateInFlight
                        }
                        CurrentInstance::CreateOutcomeUnknown { .. } => {
                            PlanningCurrentKind::CreateOutcomeUnknown
                        }
                    },
                    row.binding.clone(),
                    row.directives.clone(),
                    row.dependency_bindings.clone(),
                    row.deposed
                        .iter()
                        .map(|old| {
                            PlanningDeposedRow::from_persisted(
                                old.key.clone(),
                                old.identifier.clone(),
                                old.provider_instance.clone(),
                                old.dependency_bindings.clone(),
                                old.persisted_attributes().to_values(),
                            )
                        })
                        .collect(),
                )
            })
            .collect()
    }

    /// Project authenticated observations without changing stored rows or serial.
    /// An exact row-key match, including provider instance, supplies the refreshed
    /// identifier. Otherwise the persisted identifier and instance remain in force.
    /// Historical-generation diagnostics always use the persisted identifier.
    /// The CLI authenticates the observation map.
    pub fn planning_rows_after_refresh(
        &self,
        current: &HashMap<ResourceId, Current>,
    ) -> carina_core::planning_state::PlanningStateRows {
        self.planning_rows().with_current_observations(current)
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

    /// Replace only the persisted directives on the exactly routed row
    /// identified by `id`. No other row field is reconstructed or rewritten.
    pub fn update_resource_directives(
        &mut self,
        id: &ResolvedResourceId,
        directives: PersistedDirectives,
    ) -> bool {
        let Some(row) = self.resources.iter_mut().find(|row| {
            row.provider == id.provider
                && row.resource_type == id.resource_type
                && row.identity.as_str() == id.identity_str()
                && row.directives.provider_instance == id.provider_instance
        }) else {
            return false;
        };
        row.directives = directives;
        true
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
        validate_deposed_identities(
            resource.identity_fields(),
            resource
                .deposed
                .iter()
                .map(DeposedInstance::identity_fields),
        )
        .map_err(BackendError::InvalidState)?;
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

    /// Build a map of resource IDs to the typed directives persisted in state.
    pub fn build_directives(&self) -> HashMap<ResourceId, PersistedDirectives> {
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
    pub fn remove_legacy_data_source_rows<S, D>(
        &mut self,
        data_sources: &[DataSource<
            S,
            carina_core::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            D,
        >],
    ) -> usize {
        self.remove_legacy_data_source_rows_by_ids(data_sources.iter().map(|source| &source.id))
    }

    /// Remove legacy identifier-less rows using addresses recorded in state or
    /// a saved plan. Historical definition values never become desired input.
    pub fn remove_legacy_data_source_rows_by_ids<'a>(
        &mut self,
        data_source_ids: impl IntoIterator<Item = &'a ResourceId>,
    ) -> usize {
        let data_source_ids: HashSet<_> = data_source_ids.into_iter().collect();
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

    fn restore_partial_read_markers_in<'a, P: StateValueLeaf + 'a>(
        &self,
        states: impl Iterator<Item = (&'a ResourceId, &'a mut State<P>)>,
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
        current: &mut SnapshotState,
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
        #[allow(clippy::iter_over_hash_type)]
        // keyed rebuild: distinct missing write-only keys preserve existing-value precedence
        for key in recorded_write_only_keys(Some(schema.as_schema()), attributes, &[]) {
            if let std::collections::btree_map::Entry::Vacant(entry) =
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
            } => Current::from_snapshot(SnapshotState {
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
            } => Current::from_snapshot(SnapshotState {
                id: id.clone(),
                identifier: None,
                attributes: attributes.to_values(),
                exists: true,
                dependency_bindings: rs.dependency_bindings.clone(),
                partial_read: partial_read.clone(),
            }),
        }
    }

    /// Bind a persisted current payload to its containing row, just as decoding
    /// the complete row does. In-memory continuations may retain an older ID.
    pub(crate) fn orphan_row(&self, row: &ResourceState) -> (ResolvedResourceId, Current) {
        let id = ResolvedResourceId::with_provider_identity(
            row.provider.clone(),
            row.resource_type.clone(),
            row.identity.clone(),
            row.directives.provider_instance.clone(),
        );
        let current = carina_core::state_document::current_from_state_row(
            &id,
            row.current(),
            &row.dependency_bindings,
        );
        (id, current)
    }

    /// Build cached planner orphans with host-owned binding/directive metadata.
    /// Provider refresh inputs are minted by `PreparedRows` before this split.
    pub fn build_orphans(
        &self,
        desired_ids: &std::collections::HashSet<ResourceId>,
    ) -> LoadedOrphans {
        let mut loaded = LoadedOrphans::default();
        for rs in &self.resources {
            let (id, current) = self.orphan_row(rs);
            if desired_ids.contains(id.as_inner()) {
                continue;
            }
            let mut orphan = match current {
                Current::Absent(_state) => continue,
                Current::Present(present) => {
                    Orphan::present(present, rs.binding.clone(), rs.directives.clone())
                }
                Current::CreateInFlight(pending) => {
                    Orphan::create_in_flight(pending, rs.binding.clone(), rs.directives.clone())
                }
                Current::CreateOutcomeUnknown(unknown) => Orphan::create_outcome_unknown(
                    unknown,
                    rs.binding.clone(),
                    rs.directives.clone(),
                ),
                Current::MissingIdentifier(state) => {
                    Orphan::missing_identifier(state, rs.binding.clone(), rs.directives.clone())
                }
            };
            orphan.set_dependency_bindings(rs.dependency_bindings.clone());
            loaded.insert(id, orphan);
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

        fn canonicalize_binding(
            binding: AbsoluteBindingAddress,
        ) -> Result<AbsoluteBindingAddress, BackendError> {
            let canonical = carina_core::utils::canonicalize_map_key_address(binding.as_str());
            serde_json::from_value(serde_json::Value::String(canonical))
                .map_err(|error| BackendError::invalid_canonical_binding(binding, error))
        }

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
                r.binding = Some(canonicalize_binding(binding)?);
            }
            r.dependency_bindings = canonicalize_resolved_dependencies(
                std::mem::take(&mut r.dependency_bindings),
                "current resource",
            )?;
            r.directives.depends_on = canonicalize_resolved_dependencies(
                std::mem::take(&mut r.directives.depends_on),
                "current directives",
            )?;
            for generation in &mut r.deposed {
                generation.dependency_bindings = canonicalize_resolved_dependencies(
                    std::mem::take(&mut generation.dependency_bindings),
                    "deposed generation",
                )?;
            }
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

fn canonicalize_resolved_dependencies(
    dependencies: PersistedDestroyDependencies,
    context: &str,
) -> Result<PersistedDestroyDependencies, BackendError> {
    use carina_core::utils::canonicalize_map_key_address;

    dependencies.try_map_addresses(|dependency| {
        let mut id = dependency.into_inner();
        let canonical = canonicalize_map_key_address(
            id.identity_str()
                .expect("resolved dependency identity is always present"),
        );
        let identity = ResourceIdentity::try_from(canonical).map_err(|error| {
            BackendError::InvalidStateIdentity {
                context: format!("cannot canonicalize dependency for {context}"),
                error,
            }
        })?;
        id.set_identity(identity);
        Ok(ResolvedResourceId::new(id))
    })
}

impl Default for StateFile {
    fn default() -> Self {
        Self::new()
    }
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

/// Load exactly the current state format and canonicalize its addresses.
pub fn load_state(content: &str) -> Result<StateFile, BackendError> {
    let mut state = StateFile::from_json_str(content).map_err(map_state_file_decode_error)?;
    state.canonicalize_addresses()?;
    state
        .validate_unique_identities()
        .map_err(BackendError::InvalidState)?;
    Ok(state)
}

/// Load a current state document from backend bytes.
pub fn load_state_bytes(bytes: &[u8]) -> Result<StateFile, BackendError> {
    let content = std::str::from_utf8(bytes).map_err(|error| {
        BackendError::InvalidState(format!("State file is not valid UTF-8: {error}"))
    })?;
    load_state(content)
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
    pub dependency_bindings: PersistedDestroyDependencies,
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
    fn identity_fields(&self) -> carina_core::state_document::DeposedIdentity<'_> {
        (
            &self.key,
            &self.identifier,
            self.provider_instance.as_deref(),
        )
    }

    /// Construct a deposed generation from the opaque output of the state
    /// writeback encoder.
    pub fn from_persisted_attributes(
        key: DeposedKey,
        identifier: String,
        provider_instance: Option<String>,
        attributes: DeposedAttributes,
        dependency_bindings: PersistedDestroyDependencies,
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
        attributes: impl IntoIterator<Item = (String, serde_json::Value)>,
        dependency_bindings: PersistedDestroyDependencies,
    ) -> Self {
        Self::from_persisted_attributes(
            key,
            identifier,
            provider_instance,
            DeposedAttributes(persisted_attributes_from_test_json(attributes)),
            dependency_bindings,
        )
    }

    /// Borrow this generation's attributes through the persisted decode seam.
    pub fn persisted_attributes(&self) -> &PersistedAttributes {
        &self.attributes
    }

    /// Rebuild this generation from a provider read while carrying write-only
    /// values only from this persisted generation. Provider echoes never
    /// become provenance for a deposed row.
    pub fn refreshed_attributes_for_resource_and_schema<S, P: StateValueLeaf>(
        &self,
        resource: &ResolvedResource<S>,
        schema: Option<&ResourceSchema>,
        fresh_attributes: &BTreeMap<String, Value<P>>,
    ) -> Result<DeposedAttributes, ResourceStateEncodingError> {
        let carried_write_only = recorded_write_only_keys(schema, &self.attributes, &[]);
        let mut selected =
            SelectedStateAttributes::from_provider_values_lossy(fresh_attributes, schema, &[])?;
        selected.carry_write_only(&self.attributes, carried_write_only);
        let selected = selected.protect_for_resource(
            resource,
            PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&self.attributes),
            None,
            false,
            None,
        )?;
        let (selected, _write_only_attributes) = selected.finish()?;
        Ok(DeposedAttributes(selected))
    }

    pub fn with_refreshed_attributes(mut self, attributes: DeposedAttributes) -> Self {
        self.attributes = attributes.0;
        self
    }
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
    pub prefixes: std::collections::BTreeMap<String, String>,
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
    /// Writes rebuild the tree from authored values using an aligned builder.
    #[serde(default, skip_serializing_if = "is_empty_explicit")]
    pub explicit: ExplicitFields,
    /// The binding name for this resource (from `let` bindings in DSL).
    /// Stored so orphan Delete effects can have tree structure.
    #[serde(default)]
    pub binding: Option<AbsoluteBindingAddress>,
    /// Complete resource addresses this resource depends on (via value refs).
    /// Stored so orphan Delete effects can have tree structure.
    ///
    /// Set semantics are provided by
    /// [`carina_core::resource::ResolvedDependencies`].
    #[serde(default)]
    pub dependency_bindings: PersistedDestroyDependencies,
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

impl<'de> Deserialize<'de> for ResourceState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        carina_core::state_document::DecodedResourceState::deserialize(deserializer)
            .map(Self::from_decoded)
    }
}
impl ResourceState {
    fn identity_fields(&self) -> carina_core::state_document::StateRowIdentity<'_> {
        (&self.provider, &self.resource_type, &self.identity)
    }

    fn from_decoded(row: carina_core::state_document::DecodedResourceState) -> Self {
        Self {
            resource_type: row.resource_type,
            identity: row.identity,
            provider: row.provider,
            current: row.current,
            protected: row.protected,
            directives: row.directives,
            prefixes: row.prefixes,
            name_overrides: row.name_overrides,
            explicit: row.explicit,
            binding: row.binding,
            dependency_bindings: row.dependency_bindings,
            deposed: row
                .deposed
                .into_iter()
                .map(|old| DeposedInstance {
                    key: old.key,
                    identifier: old.identifier,
                    provider_instance: old.provider_instance,
                    attributes: old.attributes,
                    dependency_bindings: old.dependency_bindings,
                })
                .collect(),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl WritableResourceState {
    #[doc(hidden)]
    pub fn with_attributes_from_state<P: StateValueLeaf>(
        mut self,
        state: &State<P>,
    ) -> Result<ResourceState, ResourceStateEncodingError> {
        let previous = self.0.persisted_attributes().cloned().unwrap_or_default();
        let encoded: BTreeMap<_, _> = state
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
    pub fn refreshed_from_provider_state<P: StateValueLeaf>(
        self,
        state: &State<P>,
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
        let (attributes, write_only_attributes) = selected.protect_orphan(&previous)?.finish()?;
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
) -> Result<ResolvedResource<S>, UnavailableSecretError> {
    let mut resolved = resource.as_inner().clone();
    resolved.attributes = applied
        .values()
        .iter()
        .map(|(key, value)| {
            let value = value.to_value_at(format!("{}.{key}", resource.id))?;
            let protected = resource.attributes.get(key).map_or_else(
                || value.clone(),
                |authored_shape| union_secret_shapes_after_resolution(&value, authored_shape),
            );
            Ok((key.clone(), protected))
        })
        .collect::<Result<_, UnavailableSecretError>>()?;
    Ok(ResolvedResource::new(resolved))
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
        matches!(self.current, CurrentInstance::None)
            && !self.deposed.is_empty()
            && !self.protected
            && !self.directives.force_delete
            && !self.directives.create_before_destroy
            && !self.directives.explicitly_prevents_destroy()
            && self.directives.depends_on.is_empty()
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
            prefixes: Default::default(),
            name_overrides: HashMap::new(),
            explicit: ExplicitFields::default(),
            binding: None,
            dependency_bindings: PersistedDestroyDependencies::default(),
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
    pub fn from_provider_state<P: StateValueLeaf>(
        resource_type: impl Into<String>,
        identity: impl Into<ResourceIdentity>,
        provider: impl Into<String>,
        state: &State<P>,
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
                &Value::<carina_core::resource::NoPlaceholder>::Concrete(ConcreteValue::String(
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
    pub fn deposed_attributes_without_previous<S, P: StateValueLeaf>(
        resource: &ResolvedResource<S>,
        schema: Option<&ResourceSchema>,
        attributes: &BTreeMap<String, Value<P>>,
        previous_attributes: Option<&PersistedAttributes>,
    ) -> Result<DeposedAttributes, ResourceStateEncodingError> {
        let previous_hash_authority = previous_attributes
            .map(PreviousSecretHashAuthority::AllPreviouslyHashedKeys)
            .unwrap_or(PreviousSecretHashAuthority::None);
        let selected =
            SelectedStateAttributes::from_provider_values_lossy(attributes, schema, &[])?
                .protect_for_resource(resource, previous_hash_authority, None, false, None)?;
        let (selected, _write_only_attributes) = selected.finish()?;
        Ok(DeposedAttributes(selected))
    }

    /// Encode plan-time replacement history when no persisted generation
    /// exists. Provider-read write-only values are deliberately excluded.
    pub fn deposed_attributes_from_replacement_history<S>(
        resource: &ResolvedResource<S>,
        schema: Option<&ResourceSchema>,
        previous: ReplacementPreviousAttributesForStateWriteback<'_>,
        previous_attributes: Option<&PersistedAttributes>,
    ) -> Result<DeposedAttributes, ResourceStateEncodingError> {
        let previous_hash_authority = previous_attributes
            .map(PreviousSecretHashAuthority::AllPreviouslyHashedKeys)
            .unwrap_or(PreviousSecretHashAuthority::None);
        let selected = SelectedStateAttributes::from_provider_values_lossy(
            previous.provider_attributes(),
            schema,
            &[],
        )?
        .protect_for_resource(resource, previous_hash_authority, None, false, None)?;
        let (selected, _write_only_attributes) = selected.finish()?;
        Ok(DeposedAttributes(selected))
    }

    /// Carry values from this persisted current generation into a deposed
    /// generation. The receiver is the type-level source of provenance.
    pub fn deposed_attributes_from_current_generation<S>(
        &self,
        resource: &ResolvedResource<S>,
        schema: Option<&ResourceSchema>,
    ) -> Result<DeposedAttributes, ResourceStateEncodingError> {
        let previous = self.persisted_attributes().cloned().unwrap_or_default();
        let selected = SelectedStateAttributes::from_persisted_generation(&previous, schema, &[])
            .protect_for_resource(
            resource,
            PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&previous),
            None,
            false,
            None,
        )?;
        let (selected, _write_only_attributes) = selected.finish()?;
        Ok(DeposedAttributes(selected))
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

    fn populate_desired_metadata<S, R: ReferenceEquality>(
        &mut self,
        resource: &ResolvedResource<S>,
        existing: Option<&ResourceState>,
        stored_attributes: &BTreeMap<String, SnapshotValue>,
        schema: Option<&ResourceSchema>,
        authoring_basis: DesiredAuthoringBasis<'_>,
        comparison: &ComparisonContext<'_, R>,
    ) {
        if let Some(existing) = existing {
            self.protected = existing.protected;
            self.name_overrides = existing.name_overrides.clone();
        }
        self.directives = PersistedDirectives::desired_for(resource);
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
        self.dependency_bindings = resource.persisted_value_destroy_dependencies().clone();
    }

    fn carried_explicit_for_stored_values<S, R: ReferenceEquality>(
        resource: &ResolvedResource<S>,
        existing: Option<&ResourceState>,
        stored_attributes: &BTreeMap<String, SnapshotValue>,
        schema: Option<&ResourceSchema>,
        comparison: &ComparisonContext<'_, R>,
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
    /// but whose finishing step failed or completed with partial read-back. The
    /// typed observation says whether the resource was found. Values are
    /// selected strictly as observed, then sent, then absent. Desired state
    /// controls masking but cannot introduce an unsent value. The create
    /// establishes the current desired resource as the authoring baseline.
    pub fn from_observation<S: Clone, R: ReferenceEquality>(
        resource: &ResolvedResource<S>,
        identified: &HostIdentifiedCreate,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: &ComparisonContext<'_, R>,
    ) -> Result<Self, ResourceStateEncodingError> {
        Self::from_observation_with_final_desired(
            resource,
            identified,
            existing,
            schema,
            None,
            preferred_hashes,
            comparison,
        )
    }

    /// Common row-write entry for consumers that hold optional final desired
    /// evidence. The source preserves accepted-write versus observation authority.
    #[allow(clippy::too_many_arguments)]
    pub fn from_write_with_final_desired<S: Clone, R: ReferenceEquality>(
        resource: &ResolvedResource<S>,
        source: ResourceStateWrite<'_>,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        final_desired: Option<&FinalDesiredValues>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: &ComparisonContext<'_, R>,
    ) -> Result<Self, ResourceStateEncodingError> {
        match source {
            ResourceStateWrite::Provider { state, basis } => {
                Self::from_provider_state_with_final_desired(
                    resource,
                    state,
                    existing,
                    schema,
                    basis,
                    final_desired,
                    preferred_hashes,
                    comparison,
                )
            }
            ResourceStateWrite::Observation(observed) => Self::from_observation_with_final_desired(
                resource,
                observed,
                existing,
                schema,
                final_desired,
                preferred_hashes,
                comparison,
            ),
        }
    }

    /// Observations have no accepted-request authority. Only final plan
    /// inputs can prove that a public desired value equals the observed value.
    #[allow(clippy::too_many_arguments)]
    pub fn from_observation_with_final_desired<S: Clone, R: ReferenceEquality>(
        resource: &ResolvedResource<S>,
        identified: &HostIdentifiedCreate,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        final_desired: Option<&FinalDesiredValues>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: &ComparisonContext<'_, R>,
    ) -> Result<Self, ResourceStateEncodingError> {
        let applied_desired =
            resource_with_secret_shape_union(resource, identified.resolved_desired_attributes())?;
        let writeback_resource = &applied_desired;
        let sent_desired = identified.sent_desired_attributes_for(writeback_resource);
        let previous = existing.and_then(ResourceState::persisted_attributes);
        let no_observed_values = BTreeMap::new();
        let (observed_values, pending_reread) = match identified.observation() {
            HostCreateObservation::Observed(state) => (state.attributes(), None),
            HostCreateObservation::Unobserved(
                carina_core::provider::HostUnobservedReason::NotFound,
            ) => (&no_observed_values, Some(PendingReRead::not_found())),
            HostCreateObservation::Unobserved(
                carina_core::provider::HostUnobservedReason::ReadFailed(error),
            ) => (
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

        let (sent_values, sent_write_only) = match identified.sent() {
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
        let selected = selected.protect_observation(
            writeback_resource,
            previous,
            final_desired.and_then(|values| values.get(&resource.id)),
            preferred_hashes,
            Some(DesiredValueComparison {
                schema,
                context: comparison.schema(),
            }),
        )?;
        let (attributes, write_only_attributes) = selected.finish()?;
        let partial_read = identified
            .partial_read_diagnostic()
            .and_then(|diagnostic| {
                PartialReadMarker::new(
                    PendingReRead::incomplete(diagnostic.raw_reason().to_owned()),
                    diagnostic
                        .missing_attributes()
                        .iter()
                        .map(|name| name.as_str().to_owned())
                        .collect(),
                )
            })
            .or_else(|| {
                pending_reread.and_then(|pending| {
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
                })
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
            identifier: identified.identifier().clone(),
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

    /// Build the mutually exclusive continuation row. A persisted re-pend
    /// keeps the prior envelope, timestamp, and host metadata, including a
    /// known authoring record. A legacy `Unrecorded` authoring record is seeded
    /// from desired keys. A freshly sent create encodes sent write-only and
    /// secret values and establishes the current desired authoring baseline.
    pub fn from_in_flight<S: Clone, R: ReferenceEquality>(
        resource: &ResolvedResource<S>,
        pending: &InFlightCreate,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: &ComparisonContext<'_, R>,
    ) -> Result<Self, ResourceStateEncodingError> {
        let applied_desired = match pending.persistence() {
            InFlightPersistence::Fresh(fresh) => Some(resource_with_secret_shape_union(
                resource,
                fresh.resolved_desired_attributes(),
            )?),
            InFlightPersistence::Persisted => None,
        };
        let writeback_resource = applied_desired.as_ref().unwrap_or(resource);
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
                            &BTreeMap::<String, SnapshotValue>::new(),
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
                    writeback_resource,
                    fresh.sent(),
                    existing,
                    preferred_hashes,
                )?;
                let mut write_only = selected.clone();
                let sent_write_only = Self::sent_write_only_keys(schema, &selected);
                write_only.retain_keys(|key| sent_write_only.contains(key));
                let mut secrets = selected;
                secrets.retain_keys(|key| {
                    writeback_resource
                        .attributes
                        .get(key)
                        .is_some_and(carina_core::value::contains_secret)
                });
                (
                    fresh.persist(write_only, secrets),
                    Some(fresh.sent_desired_attributes()),
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
            writeback_resource,
            existing,
            &BTreeMap::new(),
            schema,
            authoring_basis,
            comparison,
        );
        Ok(row)
    }

    /// Build the token-less protected row for a create whose cloud outcome is
    /// unknown. Existing host metadata and deposed generations are retained.
    pub fn from_unknown_create_outcome<S, R: ReferenceEquality>(
        resource: &ResolvedResource<S>,
        unknown: &UnknownCreateOutcome,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        comparison: &ComparisonContext<'_, R>,
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
            &BTreeMap::new(),
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
    /// protect any key not released by an accepted sent public value or by
    /// known-public desired/provider equality. `row_basis` couples
    /// write-only and authoring provenance so a successful request cannot
    /// retain a stale authoring record. When `existing` is provided,
    /// protected/name override metadata and existing deposed generations are
    /// preserved by the row-write path.
    ///
    /// Returns an error if any provider attribute value cannot be converted to JSON.
    pub fn from_provider_state_for_resource_and_schema<
        S: Clone,
        R: ReferenceEquality,
        P: StateValueLeaf,
    >(
        resource: &ResolvedResource<S>,
        state: &State<P>,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        row_basis: RowBasis<'_>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: &ComparisonContext<'_, R>,
    ) -> Result<Self, ResourceStateEncodingError> {
        Self::from_provider_state_with_final_desired(
            resource,
            state,
            existing,
            schema,
            row_basis,
            None,
            preferred_hashes,
            comparison,
        )
    }

    /// A carried/imported row can release only against the final desired side
    /// produced by plan preparation. Without that witness it protects every
    /// previous hash. Applied rows retain their accepted-request authority.
    #[allow(clippy::too_many_arguments)]
    pub fn from_provider_state_with_final_desired<
        S: Clone,
        R: ReferenceEquality,
        P: StateValueLeaf,
    >(
        resource: &ResolvedResource<S>,
        state: &State<P>,
        existing: Option<&ResourceState>,
        schema: Option<&ResourceSchema>,
        row_basis: RowBasis<'_>,
        final_desired: Option<&FinalDesiredValues>,
        preferred_hashes: Option<&PersistedAttributes>,
        comparison: &ComparisonContext<'_, R>,
    ) -> Result<Self, ResourceStateEncodingError> {
        // The resource carried by a saved plan still contains deferred expressions.
        // Once an operation has run, secret protection must follow the provider-ready
        // desired values that were actually accepted, or a secret selected by a
        // coalesce/function/interpolation can be persisted as provider plaintext.
        let applied_desired = match row_basis {
            RowBasis::Applied(request) => Some(resource_with_secret_shape_union(
                resource,
                request.resolved_desired_attributes(),
            )?),
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
        let schema_write_only: BTreeSet<_> = schema_write_only_keys(schema).into_iter().collect();
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

        let previous_hash_authority = match (previous, row_basis) {
            (Some(previous), RowBasis::Applied(request)) => {
                // Rule 1 reads only the accepted request projection and sent set.
                // Rule 2 uses the final plan witness, including for unsent keys.
                let request_attributes = Cow::Owned(
                    request
                        .resolved_desired_attributes()
                        .values()
                        .iter()
                        .map(|(key, value)| {
                            value.to_value_at(key).map(|value| (key.clone(), value))
                        })
                        .collect::<Result<IndexMap<_, _>, _>>()
                        .map_err(SerializationError::UnavailableSecret)?,
                );
                PreviousSecretHashAuthority::AcceptedWrite {
                    previous,
                    request,
                    request_attributes,
                    final_desired: final_desired.and_then(|values| values.get(&resource.id)),
                }
            }
            (Some(previous), RowBasis::Carried | RowBasis::Imported) => {
                match final_desired.and_then(|values| values.get(&resource.id)) {
                    Some(desired) => {
                        PreviousSecretHashAuthority::MatchingFinalDesiredValue { previous, desired }
                    }
                    None => PreviousSecretHashAuthority::AllPreviouslyHashedKeys(previous),
                }
            }
            (None, _) => PreviousSecretHashAuthority::None,
        };
        let selected = selected.protect_for_resource(
            writeback_resource,
            previous_hash_authority,
            preferred_hashes,
            true,
            Some(DesiredValueComparison {
                schema,
                context: comparison.schema(),
            }),
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
                DesiredAuthoringBasis::Sent(request.sent_desired_attributes())
            }
            RowBasis::Imported => DesiredAuthoringBasis::DesiredRead,
            RowBasis::Carried => DesiredAuthoringBasis::CarriedRead,
        };
        rs.populate_desired_metadata(
            resource,
            existing,
            &state.to_snapshot().attributes,
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

#[cfg(test)]
mod tests;

/// State-load wrapper retaining immutable core authority, including a
/// backend-confirmed absence. Its fields and raw constructors are private.
///
/// ```compile_fail,E0599
/// use carina_state::ObservationStateAuthority;
/// let forged = ObservationStateAuthority::default();
/// ```
/// ```compile_fail,E0308
/// use carina_state::ObservedStateFile;
/// let omitted: ObservedStateFile = None;
/// ```
#[derive(Debug, Clone)]
pub struct ObservationStateAuthority {
    state: std::sync::Arc<carina_core::value::ObservationState>,
}
impl ObservationStateAuthority {
    pub async fn load(
        backend: &dyn crate::backend::StateBackend,
    ) -> crate::backend::BackendResult<(Option<StateFile>, Self)> {
        let loaded = backend.read_state().await?;
        Self::from_backend_load(loaded)
    }
    pub fn for_provisionable_storage(
        _storage: &crate::backends::S3BootstrapInfo,
    ) -> crate::backend::BackendResult<Self> {
        Self::from_backend_load(None).map(|(_, authority)| authority)
    }
    fn from_backend_load(
        loaded: Option<StateFile>,
    ) -> crate::backend::BackendResult<(Option<StateFile>, Self)> {
        let authority = match loaded.as_ref() {
            Some(state) => match &state.loaded_observation {
                Some(authority) => authority.clone(),
                #[cfg(any(test, feature = "test-utils"))]
                None => state
                    .fixture_observation_authority()
                    .map_err(map_state_file_decode_error)?,
                #[cfg(not(any(test, feature = "test-utils")))]
                None => {
                    return Err(BackendError::InvalidState(
                        "cannot load observation authority from state file".into(),
                    ));
                }
            },
            None => {
                let bytes = format!(
                    "{{\"version\":{},\"exports_version\":{},\"serial\":0,\"lineage\":\"absent\",\"carina_version\":\"absent\",\"resources\":[],\"exports\":{{}}}}",
                    StateFile::CURRENT_VERSION,
                    PublishedExports::CURRENT_VERSION,
                );
                Self {
                    state: std::sync::Arc::new(
                        carina_core::value::ObservationState::read_state_file(bytes.as_bytes())
                            .map_err(map_state_file_decode_error)?,
                    ),
                }
            }
        };
        Ok((loaded, authority))
    }
    /// Pair the immutable load snapshot with mutable working state. This does
    /// not assert that `file` still equals the document originally loaded.
    pub fn bind(self, file: Option<StateFile>) -> ObservedStateFile {
        ObservedStateFile {
            file,
            authority: self,
        }
    }
    pub fn protect<S: Clone, V: carina_core::resource::PlanValueKind>(
        &self,
        inputs: carina_core::value::ObservationInputs<'_, S, V>,
    ) -> Result<carina_core::value::ProtectedObservations, SerializationError> {
        self.state.protect(inputs)
    }
}
/// Mutable working state paired with immutable observation evidence from a
/// backend load. The working file may be updated or replaced after
/// loading; this wrapper does not authenticate its contents against that snapshot.
#[derive(Debug, Clone)]
pub struct ObservedStateFile {
    file: Option<StateFile>,
    authority: ObservationStateAuthority,
}
impl ObservedStateFile {
    pub fn authority(&self) -> &ObservationStateAuthority {
        &self.authority
    }
    pub fn into_file(self) -> Option<StateFile> {
        self.file
    }
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(file: Option<StateFile>) -> Self {
        let authority = file
            .as_ref()
            .map_or_else(
                || StateFile::new().fixture_observation_authority(),
                StateFile::fixture_observation_authority,
            )
            .expect("fixture state has valid observation authority");
        Self { file, authority }
    }
}
impl std::ops::Deref for ObservedStateFile {
    type Target = Option<StateFile>;
    fn deref(&self) -> &Self::Target {
        &self.file
    }
}
impl std::ops::DerefMut for ObservedStateFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.file
    }
}
